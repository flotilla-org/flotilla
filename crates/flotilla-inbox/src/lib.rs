//! The Message machine's inbox boundary. No live daemon depends on this crate.
//!
//! Binding and evidence are home-local observations. Receipts are edges on
//! consumer resources; Message gates remain the engine's gates.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use flotilla_schema::{Address, Timestamp};
use flotilla_state_machines::{machines::message::*, Home, MemoryLog, Ref, Resource, Source, Transport, Version, WriteError};
use futures::{stream, Stream};
use tokio::sync::Notify;

mod resident;
pub mod sqlite;
mod storage;
pub use resident::{Residency, RESIDENT_BUDGET};
pub use storage::{ChangePage, InboxLog, MemoryInboxLog, ScanPage, WatchReader};

/// Opaque canonical address assigned by the backend; usable as a subject.
pub type MessageId = String;
pub type PendingStream = Pin<Box<dyn Stream<Item = Result<Pending, Error>> + Send>>;
pub type Watch = Pin<Box<dyn Stream<Item = Result<Change, Error>> + Send>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Message(MessageState),
    Receipt { consumer: Receiver, transport: Transport },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub cursor: Cursor,
    pub message: MessageId,
    pub kind: ChangeKind,
}

#[derive(Debug)]
pub enum Error {
    Message(WriteError<Message>),
    Consumer(String),
    Storage(String),
    /// The authoritative log can no longer bridge this cursor; take a new baseline.
    CursorExpired,
    CursorAhead,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(error) => write!(f, "Message write refused: {error:?}"),
            Self::Consumer(error) => write!(f, "consumer receipt: {error}"),
            Self::Storage(error) => write!(f, "inbox storage: {error}"),
            Self::CursorExpired => f.write_str("watch cursor expired; obtain a new pending baseline"),
            Self::CursorAhead => f.write_str("watch cursor is ahead of this home"),
        }
    }
}
impl std::error::Error for Error {}

impl From<WriteError<Message>> for Error {
    fn from(error: WriteError<Message>) -> Self {
        Self::Message(error)
    }
}

/// Rules are ordered: the first matching rule wins, otherwise Turn.
/// Multiple selectors in a rule are conjunctive; an empty rule matches all.
#[derive(Clone, Debug, Default)]
pub struct Rules(pub Vec<Rule>);

#[derive(Clone, Debug, bon::Builder)]
pub struct Rule {
    pub sender: Option<Address>,
    pub relation: Option<Relation>,
    pub expectation: Option<Expectation>,
    pub mode: Transport,
}

impl Rules {
    pub fn choose(&self, spec: &MessageSpec) -> Transport {
        self.0
            .iter()
            .find(|rule| {
                rule.sender.as_ref().is_none_or(|sender| sender == &spec.sender)
                    && rule.relation.is_none_or(|relation| relation == spec.relation)
                    && rule.expectation.as_ref().is_none_or(|expectation| expectation == &spec.expectation)
            })
            .map_or(Transport::Turn, |rule| rule.mode)
    }
}

#[derive(Clone, Debug)]
pub struct Pending {
    pub message: MessageId,
    pub spec: MessageSpec,
    pub mode: Transport,
}

/// A local supervisor lookup, injected by the composition root. Returning None
/// means this sender has no supervisor. The address also names its inbox.
pub trait Supervision: Send + Sync {
    fn supervisor(&self, sender: &Address) -> Option<Address>;
    /// The declared role acting for the system at this home.
    fn system_sender(&self) -> Address;
}

/// Observations consumed by gates, and the home clock. Implementations must
/// supply a consistent snapshot; gates never infer evidence from link health.
pub trait Observations: Send + Sync {
    fn now(&self) -> Timestamp;
    fn with_snapshot(&self, read: &mut dyn FnMut(&MemoryMessageWorld));
}

/// Opaque identity of the Store and home in which a receipt will be committed.
/// Obtain it from the consumer's log rather than constructing it independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptScope {
    pub(crate) store: usize,
    pub(crate) home: String,
}

/// Identifies the log a consumer resource commits to. Memory logs need no
/// shared scope: their watch cursors are assigned by the inbox itself.
pub trait ConsumerLog {
    fn receipt_scope(&self) -> Option<ReceiptScope>;
}
impl<M: flotilla_state_machines::Machine> ConsumerLog for MemoryLog<M> {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        None
    }
}

/// Records Receive on the consumer's own resource. Success means that exact
/// message and transport committed; failures must leave no receipt.
/// SQLite ports must commit to the exact same Store and home as the inbox.
/// The scope is checked before Receive; remote receipt replication is separate.
pub trait ConsumerPort: Send {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        None
    }
    fn receive(&mut self, message: &str, transport: Transport, now: Timestamp) -> Result<(), Error>;
}

#[async_trait]
pub trait InboxBackend: Send + Sync {
    async fn enqueue(&self, spec: MessageSpec) -> Result<MessageId, Error>;
    /// Wait for the already-observed receipts' deliver before adding a binding.
    async fn bind(&self, inbox: Address, consumer: Receiver) -> Result<(), Error>;
    async fn unbind(&self, inbox: &Address, consumer: &Receiver) -> Result<(), Error>;
    /// Read pending messages in admission order. Notify and Mute record their receipts on pull.
    /// Turn receipts are recorded explicitly after the adapter hands over a turn.
    async fn pending(&self, inbox: &Address, consumer: &Receiver, rules: &Rules) -> Result<PendingStream, Error>;
    async fn receive(&self, consumer: &Receiver, message: &str, transport: Transport) -> Result<(), Error>;
    async fn deliver(&self, message: &str, edge: Deliver) -> Result<(), Error>;
    async fn settle(&self, message: &str, edge: Settle) -> Result<(), Error>;
    async fn drop(&self, message: &str, reason: DropReason) -> Result<(), Error>;
    /// Some(cursor) resumes strictly after it. None establishes a live tail at
    /// the current position; establish that watch before reading a fresh pending
    /// baseline, so concurrent changes remain available after the read.
    async fn watch(&self, inbox: &Address, after: Option<Cursor>) -> Result<Watch, Error>;
}

const DEAD_LETTER_PREFIX: &str = "project:inbox/role:dead-letter-";

struct GateWorld {
    names: BTreeSet<String>,
    base: MemoryMessageWorld,
    receipts: Option<(String, Version, BTreeMap<Receiver, Transport>)>,
    binding: Option<(String, Version, InboxBinding)>,
    sent: BTreeSet<String>,
}
impl GateWorld {
    fn check<K: flotilla_state_machines::Kind>(&self, reference: &Ref<K>) {
        debug_assert!(self.names.contains(reference.name()), "undeclared Message gate read: {}", reference.name());
    }
}
impl Source<Receipts> for GateWorld {
    fn lookup(&self, reference: &Ref<Receipts>) -> Option<(Version, &BTreeMap<Receiver, Transport>)> {
        self.check(reference);
        match &self.receipts {
            Some((name, version, receipts)) if name == reference.name() => Some((*version, receipts)),
            _ => self.base.receipts.lookup(reference),
        }
    }
}
impl Source<Binding> for GateWorld {
    fn lookup(&self, reference: &Ref<Binding>) -> Option<(Version, &InboxBinding)> {
        self.check(reference);
        match &self.binding {
            Some((name, version, binding)) if name == reference.name() => Some((*version, binding)),
            _ => self.base.binding.lookup(reference),
        }
    }
}
macro_rules! observed_sources {
    ($($kind:ty => $field:ident),+ $(,)?) => { $(
        impl Source<$kind> for GateWorld {
            fn lookup(&self, reference: &Ref<$kind>) -> Option<(Version, &<$kind as flotilla_state_machines::Kind>::Value)> { self.check(reference); self.base.$field.lookup(reference) }
        }
    )+ };
}
observed_sources! { AppliedThrough => applied_through, Reply => reply, Condition => condition,
DeliveryFailure => delivery_failure, Supersession => supersession, Authority => authority }

impl Source<Sent> for GateWorld {
    fn lookup(&self, reference: &Ref<Sent>) -> Option<(Version, &())> {
        self.check(reference);
        if self.sent.contains(reference.name()) {
            Some((Version(1), &()))
        } else {
            self.base.sent.lookup(reference)
        }
    }
}

struct GateInputs {
    names: BTreeSet<String>,
    receipts: Option<(String, Version, BTreeMap<Receiver, Transport>)>,
    binding: Option<(String, Version, InboxBinding)>,
    sent: BTreeSet<String>,
}
impl GateInputs {
    fn run<T>(self, observations: &dyn Observations, operation: impl FnOnce(&GateWorld) -> T) -> Result<T, Error> {
        let mut snapshot = None;
        observations.with_snapshot(&mut |base| {
            let names = || self.names.iter().map(String::as_str);
            snapshot = Some(MemoryMessageWorld {
                receipts: base.receipts.select(names()),
                binding: base.binding.select(names()),
                applied_through: base.applied_through.select(names()),
                reply: base.reply.select(names()),
                condition: base.condition.select(names()),
                delivery_failure: base.delivery_failure.select(names()),
                supersession: base.supersession.select(names()),
                authority: base.authority.select(names()),
                sent: base.sent.select(names()),
            });
        });
        let base = snapshot.ok_or_else(|| Error::Storage("missing consistent observation snapshot".into()))?;
        let mut receipts = self.receipts;
        if let Some((name, version, rows)) = &mut receipts {
            if let Some((observed_version, observed)) = base.receipts.lookup(&Ref::new(name.as_str())) {
                for (consumer, mode) in observed {
                    rows.entry(consumer.clone()).or_insert(*mode);
                }
                version.0 = version.0.saturating_add(observed_version.0);
            }
        }
        Ok(operation(&GateWorld { names: self.names, base, receipts, binding: self.binding, sent: self.sent }))
    }
}

#[derive(Clone)]
struct InboxState {
    binding: InboxBinding,
    open: usize,
}

struct State<L: InboxLog> {
    home: Home<Message, L>,
    bindings: BTreeMap<Address, InboxState>,
    receipts: BTreeMap<String, BTreeMap<Receiver, Transport>>,
    consumers: BTreeMap<Receiver, Box<dyn ConsumerPort>>,
    inboxes: BTreeMap<Address, BTreeMap<u64, String>>,
    pending_bytes: usize,
    pending_budget: usize,
    sequence: u64,
    budget: usize,
    resident_bytes: usize,
    control_bytes: usize,
    sizes: BTreeMap<String, (usize, u64)>,
    lru: BTreeMap<u64, String>,
    touched: BTreeSet<String>,
    tick: u64,
    observation_version: u64,
}

#[derive(Default)]
struct PendingRead {
    after: u64,
}

/// Single home writer. The mutex covers the engine decision and its committed
/// log write. Watchers sleep on a poke and pull deltas; they hold no writer lock.
/// [Workload and choices](../../../docs/implementation-choices/inbox-contract.md) describe the bounds and expiry.
pub struct EngineInbox<L: InboxLog> {
    state: Arc<Mutex<State<L>>>,
    reader: Arc<tokio::sync::Mutex<Box<dyn WatchReader>>>,
    changed: Arc<Notify>,
    worker: Arc<tokio::sync::Mutex<()>>,
    observations: Arc<dyn Observations>,
    supervision: Arc<dyn Supervision>,
}

pub type MemoryInbox = EngineInbox<MemoryInboxLog>;

impl<L: InboxLog> Clone for EngineInbox<L> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            reader: self.reader.clone(),
            changed: self.changed.clone(),
            worker: self.worker.clone(),
            observations: self.observations.clone(),
            supervision: self.supervision.clone(),
        }
    }
}

impl<L: InboxLog + 'static> EngineInbox<L> {
    pub fn open(writer: &str, log: L, observations: Arc<dyn Observations>, supervision: Arc<dyn Supervision>) -> Result<Self, Error> {
        Self::open_with_budget(writer, log, observations, supervision, RESIDENT_BUDGET)
    }

    pub fn open_with_budget(
        writer: &str,
        log: L,
        observations: Arc<dyn Observations>,
        supervision: Arc<dyn Supervision>,
        budget: usize,
    ) -> Result<Self, Error> {
        Self::open_with_budgets(writer, log, observations, supervision, budget, resident::PENDING_BUDGET)
    }

    /// Separate value/control and Pending-order cache budgets; cache pressure never refuses admission.
    pub fn open_with_budgets(
        writer: &str,
        mut log: L,
        observations: Arc<dyn Observations>,
        supervision: Arc<dyn Supervision>,
        budget: usize,
        pending_budget: usize,
    ) -> Result<Self, Error> {
        let reader = Arc::new(tokio::sync::Mutex::new(log.reader()));
        let sequence = log.sequence()?;
        let state = State {
            home: Home::with_log(writer, log).expect("Message declaration validates"),
            bindings: BTreeMap::new(),
            receipts: BTreeMap::new(),
            consumers: BTreeMap::new(),
            inboxes: BTreeMap::new(),
            pending_bytes: 0,
            pending_budget,
            sequence,
            budget,
            resident_bytes: 0,
            control_bytes: 0,
            sizes: BTreeMap::new(),
            lru: BTreeMap::new(),
            touched: BTreeSet::new(),
            tick: 0,
            observation_version: sequence,
        };
        let backend = Self {
            state: Arc::new(Mutex::new(state)),
            reader,
            changed: Arc::new(Notify::new()),
            worker: Arc::new(tokio::sync::Mutex::new(())),
            observations,
            supervision,
        };
        let mut state = backend.state.lock().expect("inbox writer");
        let mut after = None;
        loop {
            let page = state.home.log_mut().scan(after)?;
            for resource in page.messages {
                Self::validate_admission(&resource)?;
                let name = resource.name().to_owned();
                let recover_reaction = resource.state().drop_reason() == Some(&DropReason::Undeliverable);
                let inbox = resource.spec().inbox.clone();
                if resource.spec().admitted_seq > sequence {
                    continue;
                } // reactions created by this recovery were already counted
                if matches!(resource.state(), MessageState::Pending | MessageState::Awaiting { .. }) {
                    Self::open_inbox(&mut state, &inbox, backend.observations.now());
                    let receipts = state.home.log_mut().receipts(&name)?;
                    if !receipts.is_empty() {
                        state.receipts.insert(name.clone(), receipts);
                    }
                    // A repaired reaction can append a dead letter that appears
                    // in a later startup page; it is already resident.
                    if state.home.get(&name).is_none() {
                        state.home.restore(resource)?;
                    }
                    state.touched.insert(name);
                } else if recover_reaction {
                    // One terminal resource transiently, never all dropped history.
                    state.home.restore(resource)?;
                    state.touched.insert(name.clone());
                    backend.dead_letter(&mut state, &name)?;
                }
                Self::trim(&mut state);
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        // Pending order is rebuilt from its declared index, not scan order.
        let inboxes: Vec<_> = state.bindings.keys().cloned().collect();
        for inbox in inboxes {
            Self::cache_pending(&mut state, &inbox)?;
        }
        drop(state);
        Ok(backend)
    }

    fn validate_admission(resource: &Resource<Message>) -> Result<(), Error> {
        let admitted = resource.spec().admitted_seq;
        if admitted == 0
            || admitted > resource.version().0
            || (*resource.state() == MessageState::Pending && admitted != resource.version().0)
        {
            return Err(Error::Storage("stored Message has no valid immutable admission sequence".into()));
        }
        Ok(())
    }

    // Successful load keeps the value in Home while this operation holds the
    // writer lock. Callers may immediately get/expect it; never trim between
    // load and its last use. Trim runs after the decision/result is owned.
    fn load(state: &mut State<L>, name: &str) -> Result<bool, Error> {
        state.touched.insert(name.to_owned());
        if state.home.get(name).is_some() {
            return Ok(true);
        }
        let Some(resource) = state.home.log_mut().load(name)? else { return Ok(false) };
        Self::validate_admission(&resource)?;
        if matches!(resource.state(), MessageState::Pending | MessageState::Awaiting { .. }) {
            let receipts = state.home.log_mut().receipts(name)?;
            if !receipts.is_empty() {
                state.receipts.insert(name.to_owned(), receipts);
            }
        }
        state.home.restore(resource)?;
        Ok(true)
    }

    fn forget(state: &mut State<L>, name: &str) {
        if let Some((bytes, tick)) = state.sizes.remove(name) {
            state.resident_bytes -= bytes;
            state.lru.remove(&tick);
        }
        state.home.evict(name);
        state.receipts.remove(name);
    }

    fn trim(state: &mut State<L>) {
        for name in std::mem::take(&mut state.touched) {
            if let Some((bytes, tick)) = state.sizes.remove(&name) {
                state.resident_bytes -= bytes;
                state.lru.remove(&tick);
            }
            if let Some(resource) = state.home.get(&name) {
                if matches!(resource.state(), MessageState::Pending | MessageState::Awaiting { .. }) {
                    let bytes = resident::resource(resource, state.receipts.get(&name));
                    state.tick += 1;
                    state.resident_bytes += bytes;
                    state.sizes.insert(name.clone(), (bytes, state.tick));
                    state.lru.insert(state.tick, name);
                } else {
                    Self::forget(state, &name);
                }
            }
        }
        while state.resident_bytes.saturating_add(state.control_bytes) > state.budget {
            let Some((_, name)) = state.lru.first_key_value().map(|(tick, name)| (*tick, name.clone())) else { break };
            Self::forget(state, &name);
        }
    }

    pub fn residency_blocking(&self) -> Residency {
        let state = self.state.lock().expect("inbox writer");
        Residency {
            budget_bytes: state.budget,
            accounted_bytes: state.resident_bytes + state.control_bytes,
            value_bytes: state.resident_bytes,
            control_bytes: state.control_bytes,
            control_budget_bytes: state.budget.min(resident::CONTROL_BUDGET),
            open_messages: state.sizes.len(),
            pending_bytes: state.pending_bytes,
            pending_budget_bytes: state.pending_budget,
            pending_messages: state.inboxes.values().map(BTreeMap::len).sum(),
            oldest_state_since: state.home.resources().map(Resource::since).min(),
            newest_state_since: state.home.resources().map(Resource::since).max(),
        }
    }

    async fn read_log<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut dyn WatchReader) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        // Serialize this source handle before submitting blocking work. Waiting
        // watchers occupy futures, never blocked pool threads or the writer.
        let mut reader = self.reader.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || operation(reader.as_mut())).await.map_err(|error| Error::Storage(error.to_string()))?
    }

    async fn access<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Self, &mut State<L>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        // One blocking worker for one writer; callers await before submitting
        // blocking work, with no arbitrary 64-operation pool reservation.
        let writer = self.worker.clone().lock_owned().await;
        let backend = self.clone();
        tokio::task::spawn_blocking(move || {
            let _writer = writer;
            let mut state = backend.state.lock().expect("inbox writer");
            let previous = state.sequence;
            let result = operation(&backend, &mut state);
            Self::trim(&mut state);
            if previous != state.sequence {
                backend.changed.notify_waiters();
            }
            result
        })
        .await
        .map_err(|error| Error::Storage(error.to_string()))?
    }

    /// Synchronous setup-only registration. Runtime callers use register_async.
    /// A poisoned writer lock is fatal; recovering an uncertain writer requires
    /// reopening from the durable log rather than continuing after a panic.
    ///
    /// # Panics
    /// Panics if the writer lock is poisoned or consumer keys exceed the
    /// resident budget. Use register_async for a returned budget error.
    pub fn register_blocking(&self, consumer: Receiver, port: impl ConsumerPort + 'static) {
        let mut state = self.state.lock().expect("inbox writer");
        Self::register(&mut state, consumer, Box::new(port)).expect("consumer keys fit resident budget");
        Self::trim(&mut state);
    }

    fn register(state: &mut State<L>, consumer: Receiver, port: Box<dyn ConsumerPort>) -> Result<(), Error> {
        let extra = if state.consumers.contains_key(&consumer) { 0 } else { resident::receiver(&consumer) + 64 };
        if state.control_bytes.saturating_add(extra) > state.budget.min(resident::CONTROL_BUDGET) {
            return Err(Error::Storage("resident budget exhausted by consumer keys".into()));
        }
        state.control_bytes += extra;
        state.consumers.insert(consumer, port);
        Ok(())
    }

    fn set_binding(state: &mut State<L>, inbox: Address, binding: InboxBinding) -> Result<(), Error> {
        let previous = state.bindings.get(&inbox).map_or(0, |value| resident::binding(&inbox, &value.binding));
        let next = state.control_bytes - previous + resident::binding(&inbox, &binding);
        if next > state.budget.min(resident::CONTROL_BUDGET) {
            return Err(Error::Storage("resident budget exhausted by binding observations".into()));
        }
        state.control_bytes = next;
        let open = state.bindings.get(&inbox).map_or(0, |value| value.open);
        state.bindings.insert(inbox, InboxState { binding, open });
        state.observation_version += 1;
        Ok(())
    }

    fn open_inbox(state: &mut State<L>, inbox: &Address, now: Timestamp) {
        if let Some(value) = state.bindings.get_mut(inbox) {
            value.open += 1;
            return;
        }
        let binding = InboxBinding::Unbound { since: now };
        state.control_bytes += resident::binding(inbox, &binding);
        state.bindings.insert(inbox.clone(), InboxState { binding, open: 1 });
        state.observation_version += 1;
    }

    fn evict_pending(state: &mut State<L>, inbox: &Address) {
        if let Some(rows) = state.inboxes.remove(inbox) {
            state.pending_bytes -= resident::pending(inbox, "", true) - resident::pending(inbox, "", false)
                + rows.values().map(|name| resident::pending(inbox, name, false)).sum::<usize>();
        }
    }

    fn cache_pending(state: &mut State<L>, inbox: &Address) -> Result<(), Error> {
        Self::evict_pending(state, inbox);
        let mut charge = resident::pending(inbox, "", true) - resident::pending(inbox, "", false);
        if state.pending_bytes.saturating_add(charge) > state.pending_budget {
            return Ok(());
        }
        let mut rows = BTreeMap::new();
        let mut after = 0;
        loop {
            let keys = state.home.log_mut().pending_keys(inbox, after, 32)?;
            if keys.is_empty() {
                break;
            }
            let last_page = keys.len() < 32;
            for (seq, name) in keys {
                charge += resident::pending(inbox, &name, false);
                if state.pending_bytes.saturating_add(charge) > state.pending_budget {
                    return Ok(());
                }
                after = seq;
                rows.insert(seq, name);
            }
            if last_page {
                break;
            }
        }
        state.pending_bytes += charge;
        state.inboxes.insert(inbox.clone(), rows);
        Ok(())
    }

    fn index_pending(state: &mut State<L>, inbox: &Address, seq: u64, name: &str) {
        let Some(rows) = state.inboxes.get_mut(inbox) else {
            return;
        };
        if rows.contains_key(&seq) {
            return;
        }
        let charge = resident::pending(inbox, name, false);
        if state.pending_bytes.saturating_add(charge) > state.pending_budget {
            Self::evict_pending(state, inbox);
            return;
        }
        rows.insert(seq, name.to_owned());
        state.pending_bytes += charge;
    }

    fn next_pending(state: &mut State<L>, inbox: &Address, query: &mut PendingRead) -> Result<Option<String>, Error> {
        use std::ops::Bound::{Excluded, Unbounded};
        if let Some(rows) = state.inboxes.get(inbox) {
            let next = rows.range((Excluded(query.after), Unbounded)).next().map(|(seq, name)| (*seq, name.clone()));
            let Some((seq, name)) = next else { return Ok(None) };
            query.after = seq;
            if !Self::load(state, &name)? {
                return Err(Error::Storage("cached Pending key missing from store".into()));
            }
            return Ok(Some(name));
        }
        let Some(resource) = state.home.log_mut().pending(inbox, query.after)? else { return Ok(None) };
        Self::validate_admission(&resource)?;
        query.after = resource.spec().admitted_seq;
        let name = resource.name().to_owned();
        if state.home.get(&name).is_none() {
            let receipts = state.home.log_mut().receipts(&name)?;
            if !receipts.is_empty() {
                state.receipts.insert(name.clone(), receipts);
            }
            state.home.restore(resource)?;
        }
        state.touched.insert(name.clone());
        Ok(Some(name))
    }

    fn binding_blocked(&self, state: &mut State<L>, inbox: &Address) -> Result<bool, Error> {
        let mut query = PendingRead::default();
        while let Some(name) = Self::next_pending(state, inbox, &mut query)? {
            let blocked = state.home.get(&name).is_some_and(|resource| *resource.state() == MessageState::Pending)
                && self.world(state, Some(&name))?.run(self.observations.as_ref(), |world| {
                    world.lookup(&Ref::<Receipts>::new(&name)).is_some_and(|(_, rows)| !rows.is_empty())
                })?;
            Self::trim(state);
            if blocked {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Synchronous diagnostic snapshot; call outside runtime operations or on
    /// a blocking thread. Async runtime callers use get_async.
    ///
    /// # Panics
    /// Panics on a poisoned writer lock, a storage read/receipt error, or invalid
    /// stored admission/restore data. Use get_async for returned read errors.
    pub fn get_blocking(&self, message: &str) -> Option<Resource<Message>> {
        let mut state = self.state.lock().expect("inbox writer");
        let result = if Self::load(&mut state, message).expect("diagnostic store read") { state.home.get(message).cloned() } else { None };
        Self::trim(&mut state);
        result
    }

    pub async fn register_async(&self, consumer: Receiver, port: impl ConsumerPort + 'static) -> Result<(), Error> {
        self.access(move |_, state| Self::register(state, consumer, Box::new(port))).await
    }

    pub async fn get_async(&self, message: &str) -> Result<Option<Resource<Message>>, Error> {
        let message = message.to_owned();
        self.access(move |_, state| {
            Self::load(state, &message)?;
            Ok(state.home.get(&message).cloned())
        })
        .await
    }

    // MESSAGE_READ_SET: the Message declaration's lookup keys come from this
    // resource/spec, admission references and the explicit Drop payload below.
    // Extend this set with any new gate read; GateWorld asserts membership in
    // debug/test builds, including reads whose observed value is absent.
    fn world(&self, state: &State<L>, message: Option<&str>) -> Result<GateInputs, Error> {
        let mut world = GateInputs { names: BTreeSet::new(), receipts: None, binding: None, sent: BTreeSet::new() };
        if let Some(name) = message {
            if let Some(resource) = state.home.get(name) {
                let inbox = &resource.spec().inbox;
                world.names.insert(name.to_owned());
                world.names.insert(inbox.to_string());
                if let Expectation::Outcome { condition } = &resource.spec().expectation {
                    world.names.insert(condition.to_string());
                }
                if let Some(condition) = &resource.spec().delivery_condition {
                    world.names.insert(condition.to_string());
                }
                if matches!(resource.state(), MessageState::Pending | MessageState::Awaiting { .. }) {
                    let binding =
                        state.bindings.get(inbox).ok_or_else(|| Error::Storage("open inbox has no binding clock".into()))?.binding.clone();
                    world.binding = Some((inbox.to_string(), Version(state.observation_version), binding));
                }
                world.receipts =
                    state.receipts.get(name).map(|receipts| (name.to_owned(), Version(state.observation_version), receipts.clone()));
            }
        }
        Ok(world)
    }

    fn publish(state: &mut State<L>) -> Result<(), Error> {
        let next = state.home.log_mut().sequence()?;
        if next <= state.sequence {
            return Err(Error::Storage("home log did not advance the watch cursor".into()));
        }
        state.sequence = next;
        Ok(())
    }

    fn enqueue_locked(&self, state: &mut State<L>, spec: MessageSpec) -> Result<String, Error> {
        self.enqueue_named(state, spec, None)
    }

    fn enqueue_named(&self, state: &mut State<L>, spec: MessageSpec, name: Option<String>) -> Result<String, Error> {
        let inbox = spec.inbox.clone();
        let now = self.observations.now();
        let mut world = self.world(state, None)?;
        for reference in [&spec.in_reply_to, &spec.supersedes].into_iter().flatten() {
            world.names.insert(sent(reference).name().to_owned());
            if reference.home == state.home.writer()
                && (state.home.get(&reference.name).is_some() || state.home.log_mut().exists(&reference.name)?)
            {
                world.sent.insert(sent(reference).name().to_owned());
            }
        }
        let name = match name {
            Some(name) => name,
            None => state.home.log_mut().next_name()?,
        };
        let empty = state.bindings.get(&inbox).is_none_or(|value| value.open == 0);
        world.run(self.observations.as_ref(), |world| state.home.create(&name, spec, world, now).map(|_| ()))??;
        Self::open_inbox(state, &inbox, now);
        if empty && !state.inboxes.contains_key(&inbox) {
            let charge = resident::pending(&inbox, "", true) - resident::pending(&inbox, "", false);
            if state.pending_bytes.saturating_add(charge) <= state.pending_budget {
                state.pending_bytes += charge;
                state.inboxes.insert(inbox.clone(), BTreeMap::new());
            }
        }
        state.touched.insert(name.clone());
        // Pending has no self edge: its version remains its admission sequence
        // until it leaves this index. Numeric keys preserve arrival order.
        let admitted =
            state.home.get(&name).ok_or_else(|| Error::Storage("committed admission missing from Home".into()))?.spec().admitted_seq;
        Self::index_pending(state, &inbox, admitted, &name);
        Self::publish(state)?;
        Ok(name)
    }

    fn receive_locked(&self, state: &mut State<L>, consumer: &Receiver, message: &str, transport: Transport) -> Result<(), Error> {
        Self::load(state, message)?;
        let resource = state.home.get(message).ok_or_else(|| Error::Message(WriteError::Unknown(message.into())))?;
        match resource.state() {
            MessageState::Pending => {}
            MessageState::Awaiting { .. } => return Err(Error::Message(WriteError::Refused(MessageRefusal::WrongState))),
            MessageState::Settled { .. } | MessageState::Dropped { .. } => {
                return Err(Error::Message(WriteError::Refused(MessageRefusal::Ended)))
            }
        }
        let inbox = resource.spec().inbox.clone();
        if let Some(previous) = state.receipts.get(message).and_then(|receipts| receipts.get(consumer)) {
            return if *previous == transport {
                Ok(())
            } else {
                Err(Error::Message(WriteError::Refused(MessageRefusal::IncorrectReceipt)))
            };
        }
        let required_scope = state.home.log().consumer_scope();
        let port = state.consumers.get_mut(consumer).ok_or_else(|| Error::Consumer("consumer has no Receive port".into()))?;
        if let Some(scope) = required_scope {
            if consumer.host != scope.home || port.receipt_scope().as_ref() != Some(&scope) {
                return Err(Error::Consumer("SQLite consumer must share the inbox Store and home".into()));
            }
        }
        port.receive(message, transport, self.observations.now())?;
        state.receipts.entry(message.into()).or_default().insert(consumer.clone(), transport);
        state.observation_version += 1;
        state.home.log_mut().receipt(&inbox, message, consumer, transport)?;
        Self::publish(state)?;
        Ok(())
    }

    fn apply_locked(&self, state: &mut State<L>, message: &str, edge: impl Into<MessageEdge>) -> Result<(), Error> {
        Self::load(state, message)?;
        let was_open = state.home.get(message).is_some_and(|r| matches!(r.state(), MessageState::Pending | MessageState::Awaiting { .. }));
        let previous = state.home.get(message).map(|resource| resource.spec().admitted_seq);
        let mut world = self.world(state, Some(message))?;
        let edge = edge.into();
        if let MessageEdge::Drop(drop) = &edge {
            match &drop.reason {
                DropReason::Superseded { by } => {
                    world.names.insert(by.to_string());
                }
                DropReason::Withdrawn { authority, .. } | DropReason::ConsumerUnreachable { authority, .. } => {
                    world.names.insert(authority.to_string());
                }
                _ => {}
            }
        }
        world.run(self.observations.as_ref(), |world| state.home.apply(message, edge, world, self.observations.now()))??;
        let resource = state.home.get(message).expect("applied resource");
        let inbox = resource.spec().inbox.clone();
        let next = resource.state().clone();
        if next != MessageState::Pending {
            if let (Some(index), Some(previous)) = (state.inboxes.get_mut(&inbox), previous) {
                if let Some(name) = index.remove(&previous) {
                    state.pending_bytes -= resident::pending(&inbox, &name, false);
                }
            }
        }
        if was_open && matches!(next, MessageState::Settled { .. } | MessageState::Dropped { .. }) {
            let observation = state.bindings.get_mut(&inbox).ok_or_else(|| Error::Storage("open inbox has no binding clock".into()))?;
            observation.open -= 1;
            if observation.open == 0 {
                Self::evict_pending(state, &inbox);
                if state.bindings.get(&inbox).is_some_and(|value| matches!(value.binding, InboxBinding::Unbound { .. })) {
                    if let Some(value) = state.bindings.remove(&inbox) {
                        state.control_bytes -= resident::binding(&inbox, &value.binding);
                    }
                    state.observation_version += 1;
                }
            }
        }
        Self::publish(state)?;
        Ok(())
    }
    fn dead_letter(&self, state: &mut State<L>, message: &str) -> Result<(), Error> {
        let name = format!("{DEAD_LETTER_PREFIX}{}", message.rsplit(':').next().expect("message address"));
        if state.home.get(&name).is_some() || state.home.log_mut().exists(&name)? {
            return Ok(());
        }
        let resource = state.home.get(message).expect("dropped");
        let sender = &resource.spec().sender;
        if let Some(supervisor) = self.supervision.supervisor(sender).filter(|supervisor| supervisor != sender) {
            let mut spec = MessageSpec::new(
                self.supervision.system_sender(),
                supervisor,
                Relation::System,
                MessageBody::text("Message undeliverable").expect("small body"),
            );
            spec.references.push(MessageReference {
                address: message.parse().expect("message identity is an address"),
                revision: resource.version().0.to_string(),
                subject: true,
            });
            self.enqueue_named(state, spec, Some(name.clone()))?;
        }
        Ok(())
    }
}

#[async_trait]
impl<L: InboxLog + 'static> InboxBackend for EngineInbox<L> {
    async fn enqueue(&self, spec: MessageSpec) -> Result<String, Error> {
        self.access(move |backend, state| backend.enqueue_locked(state, spec)).await
    }
    async fn bind(&self, inbox: Address, consumer: Receiver) -> Result<(), Error> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let inbox = inbox.clone();
            let consumer = consumer.clone();
            let accepted = self
                .access(move |backend, state| {
                    if backend.binding_blocked(state, &inbox)? {
                        return Ok(false);
                    }
                    let mut binding = state
                        .bindings
                        .get(&inbox)
                        .map(|value| value.binding.clone())
                        .unwrap_or(InboxBinding::Unbound { since: backend.observations.now() });
                    match &mut binding {
                        InboxBinding::Bound { consumers } => {
                            consumers.insert(consumer);
                        }
                        InboxBinding::Unbound { .. } => {
                            binding = InboxBinding::Bound { consumers: [consumer].into() };
                        }
                    }
                    Self::set_binding(state, inbox, binding)?;
                    Ok(true)
                })
                .await?;
            if accepted {
                return Ok(());
            }
            notified.await;
        }
    }
    async fn unbind(&self, inbox: &Address, consumer: &Receiver) -> Result<(), Error> {
        let inbox = inbox.clone();
        let consumer = consumer.clone();
        self.access(move |backend, state| {
            if let Some(InboxBinding::Bound { mut consumers }) = state.bindings.get(&inbox).map(|value| value.binding.clone()) {
                consumers.remove(&consumer);
                let binding = if consumers.is_empty() {
                    InboxBinding::Unbound { since: backend.observations.now() }
                } else {
                    InboxBinding::Bound { consumers }
                };
                if matches!(binding, InboxBinding::Unbound { .. }) && state.bindings.get(&inbox).is_some_and(|value| value.open == 0) {
                    if let Some(value) = state.bindings.remove(&inbox) {
                        state.control_bytes -= resident::binding(&inbox, &value.binding);
                    }
                } else {
                    Self::set_binding(state, inbox, binding)?;
                }
            }
            Ok(())
        })
        .await
    }
    async fn pending(&self, inbox: &Address, consumer: &Receiver, rules: &Rules) -> Result<PendingStream, Error> {
        let backend = self.clone();
        let inbox = inbox.clone();
        let consumer = consumer.clone();
        let rules = rules.clone();
        Ok(Box::pin(stream::unfold(
            (backend, inbox, consumer, rules, Some(PendingRead::default())),
            |(backend, inbox, consumer, rules, query)| async move {
                let mut query = query?;
                let query_inbox = inbox.clone();
                let query_consumer = consumer.clone();
                let query_rules = rules.clone();
                let result = backend
                    .access(move |backend, state| {
                        while let Some(name) = Self::next_pending(state, &query_inbox, &mut query)? {
                            let resource = state.home.get(&name).expect("pending candidate");
                            let allowed = backend.world(state, Some(&name))?.run(backend.observations.as_ref(), |world| {
                                resource.judge(world, backend.observations.now(), |h| pending_for(h, &query_consumer)).value
                            })?;
                            if allowed {
                                let mode = if name.starts_with(DEAD_LETTER_PREFIX) {
                                    Transport::Notify
                                } else {
                                    query_rules.choose(resource.spec())
                                };
                                let pending = Pending { message: name.clone(), spec: resource.spec().clone(), mode };
                                if mode != Transport::Turn {
                                    backend.receive_locked(state, &query_consumer, &name, mode)?;
                                }
                                return Ok(Some((query, pending)));
                            }
                            Self::trim(state);
                        }
                        Ok(None)
                    })
                    .await;
                match result {
                    Ok(Some((query, pending))) => Some((Ok(pending), (backend, inbox, consumer, rules, Some(query)))),
                    Ok(None) => None,
                    Err(error) => Some((Err(error), (backend, inbox, consumer, rules, None))),
                }
            },
        )))
    }
    async fn receive(&self, consumer: &Receiver, message: &str, transport: Transport) -> Result<(), Error> {
        let consumer = consumer.clone();
        let message = message.to_owned();
        self.access(move |backend, state| backend.receive_locked(state, &consumer, &message, transport)).await
    }
    async fn deliver(&self, message: &str, edge: Deliver) -> Result<(), Error> {
        let message = message.to_owned();
        self.access(move |backend, state| backend.apply_locked(state, &message, edge)).await
    }
    async fn settle(&self, message: &str, edge: Settle) -> Result<(), Error> {
        let message = message.to_owned();
        self.access(move |backend, state| backend.apply_locked(state, &message, edge)).await
    }
    async fn drop(&self, message: &str, reason: DropReason) -> Result<(), Error> {
        let message = message.to_owned();
        self.access(move |backend, state| {
            // A retry after the Message commit but a failed reaction completes
            // the same deterministic dead letter without dropping twice.
            Self::load(state, &message)?;
            let retry = reason == DropReason::Undeliverable
                && state.home.get(&message).is_some_and(|resource| resource.state().drop_reason() == Some(&reason));
            if retry {
                backend.dead_letter(state, &message)?;
                return Err(Error::Message(WriteError::Refused(MessageRefusal::Ended)));
            }
            backend.apply_locked(state, &message, DropMessage { reason: reason.clone() })?;
            if reason == DropReason::Undeliverable {
                backend.dead_letter(state, &message)?;
            }
            Ok(())
        })
        .await
    }
    async fn watch(&self, inbox: &Address, after: Option<Cursor>) -> Result<Watch, Error> {
        let after = self
            .read_log(move |reader| {
                let latest = reader.sequence()?;
                let cursor = after.unwrap_or(Cursor(latest));
                if cursor.0 > latest {
                    return Err(Error::CursorAhead);
                }
                Ok(cursor)
            })
            .await?;
        let backend = self.clone();
        let inbox = inbox.clone();
        Ok(Box::pin(stream::unfold(
            (backend, inbox, after, VecDeque::<Change>::new(), false),
            |(backend, inbox, mut cursor, mut buffered, done)| async move {
                if done {
                    return None;
                }
                loop {
                    if let Some(next) = buffered.pop_front() {
                        return Some((Ok(next), (backend, inbox, cursor, buffered, false)));
                    }
                    let notified = backend.changed.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let page = backend.read_log(move |reader| reader.changes_since(cursor)).await;
                    match page {
                        Ok(page) => {
                            let advanced = page.through > cursor;
                            cursor = page.through;
                            buffered =
                                page.changes.into_iter().filter(|(address, _)| address == &inbox).map(|(_, change)| change).collect();
                            if advanced {
                                continue;
                            }
                            notified.await;
                        }
                        Err(error) => {
                            if matches!(error, Error::CursorExpired) {
                                tracing::warn!(cursor = cursor.0, "inbox source log cursor expired");
                            }
                            return Some((Err(error), (backend.clone(), inbox, cursor, buffered, true)));
                        }
                    }
                }
            },
        )))
    }
}

#[cfg(all(test, debug_assertions))]
mod read_set_tests {
    use super::*;

    // Behaviour: adding a gate lookup without declaring its snapshot key fails
    // loudly in debug/test builds instead of silently substituting absence.
    #[test]
    #[should_panic(expected = "undeclared Message gate read")]
    fn undeclared_gate_read_is_rejected() {
        struct Evidence;
        impl Observations for Evidence {
            fn now(&self) -> Timestamp {
                Timestamp(0)
            }
            fn with_snapshot(&self, read: &mut dyn FnMut(&MemoryMessageWorld)) {
                read(&MemoryMessageWorld::default());
            }
        }
        let inputs = GateInputs { names: BTreeSet::new(), receipts: None, binding: None, sent: BTreeSet::new() };
        inputs
            .run(&Evidence, |world| {
                let _ = world.lookup(&Ref::<Condition>::new("undeclared"));
            })
            .expect("snapshot");
    }
}
