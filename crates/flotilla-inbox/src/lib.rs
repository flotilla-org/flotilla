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
use flotilla_state_machines::{machines::message::*, Home, Log, MemoryLog, Ref, Resource, Source, Transport, Version, WriteError};
use futures::{stream, Stream};
use tokio::sync::{Notify, Semaphore};

pub mod sqlite;

/// Opaque canonical address assigned by the backend; usable as a subject.
pub type MessageId = String;
/// Maximum retained changes across all inboxes (about 6.5 seconds at 10,000/s).
/// Older cursors fail explicitly; subscribers must establish a fresh baseline.
pub const WATCH_HISTORY: usize = 65_536;
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
    /// The bounded memory journal no longer holds this cursor; take a new baseline.
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
    fn snapshot(&self) -> MemoryMessageWorld;
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

/// The storage seam for the two engine-backed implementations. Foreign inboxes
/// implement InboxBackend directly and need not use this engine storage port.
pub struct Recovery {
    pub messages: Vec<Resource<Message>>,
    pub receipts: BTreeMap<String, BTreeMap<Receiver, Transport>>,
    pub changes: Vec<(Address, Change)>,
    pub sequence: u64,
    pub expired: Cursor,
}

pub trait InboxLog: Log<Message> + Send {
    fn consumer_scope(&self) -> Option<ReceiptScope> {
        None
    }
    fn recover(&mut self) -> Result<Recovery, Error> {
        Ok(Recovery { messages: Vec::new(), receipts: BTreeMap::new(), changes: Vec::new(), sequence: 0, expired: Cursor(0) })
    }
    fn sequence(&mut self) -> Result<Option<u64>, Error> {
        Ok(None)
    }

    fn next_name(&mut self) -> Result<String, Error>;
}

pub struct MemoryInboxLog {
    log: MemoryLog<Message>,
    names: u64,
}

impl Default for MemoryInboxLog {
    fn default() -> Self {
        Self { log: MemoryLog::new(), names: 0 }
    }
}

impl Log<Message> for MemoryInboxLog {
    fn create(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: &Resource<Message>,
        reads: &[flotilla_state_machines::InputRead],
    ) -> Result<flotilla_state_machines::Version, flotilla_state_machines::LogError> {
        self.log.create(writer, at, resource, reads)
    }
    fn edge(
        &mut self,
        previous: &flotilla_state_machines::StoredState<Message>,
        next: &Resource<Message>,
        event: &flotilla_state_machines::EdgeEventOf<Message>,
    ) -> Result<flotilla_state_machines::Version, flotilla_state_machines::LogError> {
        self.log.edge(previous, next, event)
    }
}
impl InboxLog for MemoryInboxLog {
    fn next_name(&mut self) -> Result<String, Error> {
        self.names += 1;
        Ok(format!("project:inbox/role:message-{}", self.names))
    }
}

struct GateWorld {
    base: MemoryMessageWorld,
    receipts: Option<(String, Version, BTreeMap<Receiver, Transport>)>,
    binding: Option<(String, Version, InboxBinding)>,
}
impl Source<Receipts> for GateWorld {
    fn lookup(&self, reference: &Ref<Receipts>) -> Option<(Version, &BTreeMap<Receiver, Transport>)> {
        match &self.receipts {
            Some((name, version, receipts)) if name == reference.name() => Some((*version, receipts)),
            _ => self.base.receipts.lookup(reference),
        }
    }
}
impl Source<Binding> for GateWorld {
    fn lookup(&self, reference: &Ref<Binding>) -> Option<(Version, &InboxBinding)> {
        match &self.binding {
            Some((name, version, binding)) if name == reference.name() => Some((*version, binding)),
            _ => self.base.binding.lookup(reference),
        }
    }
}
macro_rules! observed_sources {
    ($($kind:ty => $field:ident),+ $(,)?) => { $(
        impl Source<$kind> for GateWorld {
            fn lookup(&self, reference: &Ref<$kind>) -> Option<(Version, &<$kind as flotilla_state_machines::Kind>::Value)> { self.base.$field.lookup(reference) }
        }
    )+ };
}
observed_sources! { AppliedThrough => applied_through, Reply => reply, Condition => condition,
DeliveryFailure => delivery_failure, Supersession => supersession, Authority => authority, Sent => sent }

struct State<L: InboxLog> {
    home: Home<Message, L>,
    bindings: BTreeMap<Address, InboxBinding>,
    receipts: BTreeMap<String, BTreeMap<Receiver, Transport>>,
    consumers: BTreeMap<Receiver, Box<dyn ConsumerPort>>,
    inboxes: BTreeMap<Address, BTreeMap<u64, String>>,
    dead_letters: BTreeSet<String>,
    journal: BTreeMap<Address, BTreeMap<Cursor, Change>>,
    retained: VecDeque<(Address, Cursor)>,
    expired: Cursor,
    sequence: u64,
    observation_version: u64,
}

/// Single home writer. The mutex covers the engine decision and its committed
/// log write. Watchers sleep on a poke and pull deltas; they hold no writer lock.
/// [Workload and choices](../../../docs/implementation-choices/inbox-contract.md) describe the bounds and expiry.
pub struct EngineInbox<L: InboxLog> {
    state: Arc<Mutex<State<L>>>,
    changed: Arc<Notify>,
    admission: Arc<Semaphore>,
    observations: Arc<dyn Observations>,
    supervision: Arc<dyn Supervision>,
}

pub type MemoryInbox = EngineInbox<MemoryInboxLog>;

impl<L: InboxLog> Clone for EngineInbox<L> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            changed: self.changed.clone(),
            admission: self.admission.clone(),
            observations: self.observations.clone(),
            supervision: self.supervision.clone(),
        }
    }
}

impl<L: InboxLog + 'static> EngineInbox<L> {
    pub fn open(writer: &str, mut log: L, observations: Arc<dyn Observations>, supervision: Arc<dyn Supervision>) -> Result<Self, Error> {
        let recovery = log.recover()?;
        let mut home = Home::with_log(writer, log).expect("Message declaration validates");
        let mut inboxes: BTreeMap<Address, BTreeMap<u64, String>> = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        for resource in recovery.messages {
            if matches!(resource.state(), MessageState::Pending) {
                inboxes.entry(resource.spec().inbox.clone()).or_default().insert(resource.version().0, resource.name().to_owned());
            }
            bindings.entry(resource.spec().inbox.clone()).or_insert(InboxBinding::Unbound { since: observations.now() });
            home.restore(resource)?;
        }
        let mut state = State {
            home,
            bindings,
            receipts: recovery.receipts,
            consumers: BTreeMap::new(),
            inboxes,
            dead_letters: BTreeSet::new(),
            journal: BTreeMap::new(),
            retained: VecDeque::new(),
            expired: recovery.expired,
            sequence: recovery.sequence,
            observation_version: recovery.sequence,
        };
        for (inbox, change) in recovery.changes {
            Self::retain(&mut state, inbox, change);
        }
        let backend = Self {
            state: Arc::new(Mutex::new(state)),
            changed: Arc::new(Notify::new()),
            admission: Arc::new(Semaphore::new(64)),
            observations,
            supervision,
        };
        {
            let mut state = backend.state.lock().expect("inbox writer");
            let dropped: Vec<_> = state
                .home
                .resources()
                .filter(|resource| resource.state().drop_reason() == Some(&DropReason::Undeliverable))
                .map(|resource| resource.name().to_owned())
                .collect();
            for message in dropped {
                backend.dead_letter(&mut state, &message)?;
            }
        }
        Ok(backend)
    }

    async fn access<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Self, &mut State<L>) -> Result<T, Error> + Send + 'static,
    ) -> Result<T, Error> {
        let permit = self.admission.clone().acquire_owned().await.map_err(|error| Error::Storage(error.to_string()))?;
        let backend = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut state = backend.state.lock().expect("inbox writer");
            let previous = state.sequence;
            let result = operation(&backend, &mut state);
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
    pub fn register_blocking(&self, consumer: Receiver, port: impl ConsumerPort + 'static) {
        self.state.lock().expect("inbox writer").consumers.insert(consumer, Box::new(port));
    }

    /// Synchronous diagnostic snapshot; call outside runtime operations or on
    /// a blocking thread. Async runtime callers use get_async.
    pub fn get_blocking(&self, message: &str) -> Option<Resource<Message>> {
        self.state.lock().expect("inbox writer").home.get(message).cloned()
    }

    pub async fn register_async(&self, consumer: Receiver, port: impl ConsumerPort + 'static) -> Result<(), Error> {
        self.access(move |_, state| {
            state.consumers.insert(consumer, Box::new(port));
            Ok(())
        })
        .await
    }

    pub async fn get_async(&self, message: &str) -> Result<Option<Resource<Message>>, Error> {
        let message = message.to_owned();
        self.access(move |_, state| Ok(state.home.get(&message).cloned())).await
    }

    fn world(&self, state: &State<L>, message: Option<&str>) -> GateWorld {
        let base = self.observations.snapshot();
        let mut world = GateWorld { base, receipts: None, binding: None };
        if let Some(name) = message {
            if let Some(resource) = state.home.get(name) {
                let inbox = &resource.spec().inbox;
                if let Some(binding) = state.bindings.get(inbox) {
                    world.binding = Some((inbox.to_string(), Version(state.observation_version), binding.clone()));
                }
                if let Some(receipts) = state.receipts.get(name) {
                    let (version, mut merged) =
                        world.base.receipts.lookup(&Ref::new(name)).map(|(version, rows)| (version, rows.clone())).unwrap_or_default();
                    merged.extend(receipts.clone());
                    world.receipts = Some((name.to_owned(), Version(version.0.saturating_add(state.observation_version)), merged));
                }
            }
        }
        world
    }

    fn publish(state: &mut State<L>, inbox: Address, message: String, kind: ChangeKind) -> Result<(), Error> {
        let next = state.home.log_mut().sequence()?.unwrap_or(state.sequence + 1);
        if next <= state.sequence {
            return Err(Error::Storage("home log did not advance the watch cursor".into()));
        }
        state.sequence = next;
        Self::retain(state, inbox, Change { cursor: Cursor(state.sequence), message, kind });
        Ok(())
    }

    fn retain(state: &mut State<L>, inbox: Address, change: Change) {
        // A global bound, independent of the number of inboxes or subscribers.
        state.retained.push_back((inbox.clone(), change.cursor));
        state.journal.entry(inbox).or_default().insert(change.cursor, change);
        if state.retained.len() > WATCH_HISTORY {
            let (inbox, cursor) = state.retained.pop_front().expect("bounded journal");
            state.journal.get_mut(&inbox).expect("indexed inbox").remove(&cursor);
            state.expired = cursor;
        }
    }

    fn enqueue_locked(&self, state: &mut State<L>, spec: MessageSpec) -> Result<String, Error> {
        self.enqueue_named(state, spec, None)
    }

    fn enqueue_named(&self, state: &mut State<L>, spec: MessageSpec, name: Option<String>) -> Result<String, Error> {
        let inbox = spec.inbox.clone();
        let now = self.observations.now();
        let mut world = self.world(state, None);
        for reference in [&spec.in_reply_to, &spec.supersedes].into_iter().flatten() {
            if reference.home == state.home.writer() && state.home.get(&reference.name).is_some() {
                world.base.sent.put(&sent(reference), ());
            }
        }
        let name = match name {
            Some(name) => name,
            None => state.home.log_mut().next_name()?,
        };
        state.home.create(&name, spec, &world, now)?;
        state.bindings.entry(inbox.clone()).or_insert(InboxBinding::Unbound { since: now });
        // Pending has no self edge: its version remains its admission sequence
        // until it leaves this index. Numeric keys preserve arrival order.
        let admitted = state.home.get(&name).expect("admitted message").version().0;
        state.inboxes.entry(inbox.clone()).or_default().insert(admitted, name.clone());
        Self::publish(state, inbox, name.clone(), ChangeKind::Message(MessageState::Pending))?;
        Ok(name)
    }

    fn receive_locked(&self, state: &mut State<L>, consumer: &Receiver, message: &str, transport: Transport) -> Result<(), Error> {
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
        Self::publish(state, inbox, message.into(), ChangeKind::Receipt { consumer: consumer.clone(), transport })?;
        Ok(())
    }

    fn apply_locked(&self, state: &mut State<L>, message: &str, edge: impl Into<MessageEdge>) -> Result<(), Error> {
        let previous = state.home.get(message).map(|resource| resource.version().0);
        let world = self.world(state, Some(message));
        state.home.apply(message, edge, &world, self.observations.now())?;
        let resource = state.home.get(message).expect("applied resource");
        let inbox = resource.spec().inbox.clone();
        let next = resource.state().clone();
        if next != MessageState::Pending {
            if let (Some(index), Some(previous)) = (state.inboxes.get_mut(&inbox), previous) {
                index.remove(&previous);
            }
        }
        Self::publish(state, inbox, message.into(), ChangeKind::Message(next))?;
        Ok(())
    }
    fn dead_letter(&self, state: &mut State<L>, message: &str) -> Result<(), Error> {
        let name = format!("project:inbox/role:dead-letter-{}", message.rsplit(':').next().expect("message address"));
        if state.home.get(&name).is_some() {
            state.dead_letters.insert(name);
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
            state.dead_letters.insert(name);
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
                    let blocked = state.inboxes.get(&inbox).into_iter().flat_map(|index| index.values()).any(|name| {
                        let world = backend.world(state, Some(name));
                        world.lookup(&Ref::<Receipts>::new(name)).is_some_and(|(_, receipts)| !receipts.is_empty())
                            && state.home.get(name).is_some_and(|resource| *resource.state() == MessageState::Pending)
                    });
                    if blocked {
                        return Ok(false);
                    }
                    let binding = state.bindings.entry(inbox).or_insert(InboxBinding::Unbound { since: backend.observations.now() });
                    match binding {
                        InboxBinding::Bound { consumers } => {
                            consumers.insert(consumer);
                        }
                        InboxBinding::Unbound { .. } => {
                            *binding = InboxBinding::Bound { consumers: [consumer].into() };
                        }
                    }
                    state.observation_version += 1;
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
            if let Some(InboxBinding::Bound { consumers }) = state.bindings.get_mut(&inbox) {
                consumers.remove(&consumer);
                if consumers.is_empty() {
                    state.bindings.insert(inbox, InboxBinding::Unbound { since: backend.observations.now() });
                }
            }
            state.observation_version += 1;
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
            (backend, inbox, consumer, rules, None::<u64>),
            |(backend, inbox, consumer, rules, mut after)| async move {
                loop {
                    let query_inbox = inbox.clone();
                    let query_consumer = consumer.clone();
                    let query_rules = rules.clone();
                    let query_after = after;
                    let result = backend
                        .access(move |backend, state| {
                            let name = state
                                .inboxes
                                .get(&query_inbox)
                                .and_then(|names| {
                                    names
                                        .range::<u64, _>((
                                            query_after.as_ref().map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                                            std::ops::Bound::Unbounded,
                                        ))
                                        .next()
                                })
                                .map(|(position, name)| (*position, name.clone()));
                            let Some((position, name)) = name else { return Ok(None) };
                            let resource = state.home.get(&name).expect("indexed message");
                            let world = backend.world(state, Some(&name));
                            if !resource.judge(&world, backend.observations.now(), |h| pending_for(h, &query_consumer)).value {
                                return Ok(Some((position, None)));
                            }
                            let mode =
                                if state.dead_letters.contains(&name) { Transport::Notify } else { query_rules.choose(resource.spec()) };
                            let pending = Pending { message: name.clone(), spec: resource.spec().clone(), mode };
                            if mode != Transport::Turn {
                                backend.receive_locked(state, &query_consumer, &name, mode)?;
                            }
                            Ok(Some((position, Some(pending))))
                        })
                        .await;
                    match result {
                        Ok(None) => return None,
                        Ok(Some((position, pending))) => {
                            after = Some(position);
                            if let Some(pending) = pending {
                                return Some((Ok(pending), (backend, inbox, consumer, rules, after)));
                            }
                        }
                        Err(error) => return Some((Err(error), (backend, inbox, consumer, rules, after))),
                    }
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
            .access(move |_, state| {
                let cursor = after.unwrap_or(Cursor(state.sequence));
                if cursor < state.expired {
                    tracing::warn!(cursor = cursor.0, expired_through = state.expired.0, "inbox watch cursor expired");
                    Err(Error::CursorExpired)
                } else if cursor.0 > state.sequence {
                    Err(Error::CursorAhead)
                } else {
                    Ok(cursor)
                }
            })
            .await?;
        let backend = self.clone();
        let inbox = inbox.clone();
        Ok(Box::pin(stream::unfold((backend, inbox, after, false), |(backend, inbox, mut cursor, done)| async move {
            if done {
                return None;
            }
            loop {
                let notified = backend.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let query_inbox = inbox.clone();
                let next = backend
                    .access(move |_, state| {
                        if cursor < state.expired {
                            tracing::warn!(cursor = cursor.0, expired_through = state.expired.0, "inbox watch cursor expired");
                            return Err(Error::CursorExpired);
                        }
                        Ok(state
                            .journal
                            .get(&query_inbox)
                            .and_then(|entries| entries.range((std::ops::Bound::Excluded(cursor), std::ops::Bound::Unbounded)).next())
                            .map(|(_, entry)| entry.clone()))
                    })
                    .await;
                match next {
                    Ok(Some(next)) => {
                        cursor = next.cursor;
                        return Some((Ok(next), (backend.clone(), inbox, cursor, false)));
                    }
                    Err(error) => return Some((Err(error), (backend.clone(), inbox, cursor, true))),
                    Ok(None) => notified.await,
                }
            }
        })))
    }
}
