//! The ADR 0064 engine's storage port over `flotilla-schema-sqlite` (#3317).
//!
//! [`StoreLog`] is the engine's [`Log`] over one [`Store`]: a create is one
//! `insert` and an edge one `apply_edge`, each in its own write transaction.
//! [`MessageLog`] adds Message admission, assigned inside the create's
//! transaction. Receipts are served as a [`ReceiptLog`] from `Read::about`.

pub mod archive;
pub mod follow;
pub mod plain;
pub mod watch;

use std::sync::Arc;

use flotilla_schema::{
    Cbor, ChangeItem, Event, Identity, Kind, KindRegistry, KindSpec, Labelled, NewChange, RegistryError, StateData, Timestamp,
};
use flotilla_schema_sqlite::{AboutCursor, ChangeId, Read, Stamp, StoreError, StoreGuard, StoreHandle, Write};
use flotilla_state_machines::{
    machines::{
        agent_session::AgentSession,
        awaiting_reply::AwaitingReply,
        consumer::Consumer,
        convoy,
        message::{Message, ReceiptLog},
        person::Person,
        vessel,
    },
    EdgeEventOf, EdgePayload, InputRead, Label, Log, LogError, Machine, Resource, StoredState, StoredWrite, Version,
};

/// The kinds the engine's first layers moved: Message, and the minimal
/// AgentSession and the Person its receipts are written on. Codes are the production
/// registry's (`flotilla_state_machines::kinds`), never reassigned here.
pub struct MachineRegistry {
    pub kinds: KindRegistry,
    pub diagrams: std::collections::BTreeMap<String, String>,
}
/// One registration pairs each decided kind with its machine rendering.
pub fn machine_registry() -> Result<MachineRegistry, RegistryError> {
    let mut kinds = KindRegistry::new();
    let mut diagrams = std::collections::BTreeMap::new();
    macro_rules! register {
        ($machine:ty) => {{
            kinds.register::<Resource<$machine>>()?;
            let set = kinds.iter().find(|set| set.code == Resource::<$machine>::CODE).expect("registered kind");
            diagrams.insert(set.kind.clone(), <$machine>::definition().mermaid(flotilla_state_machines::Diagram::Full));
        }};
    }
    register!(Message);
    register!(AgentSession);
    register!(Person);
    // A service's receipts: first, each host's checkout inspector (#3108).
    register!(flotilla_state_machines::machines::service::Service);
    register!(convoy::Convoy);
    register!(AwaitingReply);
    Ok(MachineRegistry { kinds, diagrams })
}
pub fn registry() -> Result<KindRegistry, RegistryError> {
    Ok(machine_registry()?.kinds)
}

/// The kinds the Convoy stack moves (#3587): Convoy, with its seats,
/// promises, submissions and duties as its records, and AwaitingReply. Both
/// codes are the production registry's, and both are in [`machine_registry`]
/// since the write path (layer 6) and the send path (layer 7). The benchmark
/// opens a store with both.
pub fn convoy_registry() -> Result<KindRegistry, RegistryError> {
    let mut kinds = KindRegistry::new();
    kinds.register::<Resource<convoy::Convoy>>()?;
    kinds.register::<Resource<AwaitingReply>>()?;
    Ok(kinds)
}

/// Drops the tables of fields the engine no longer stores, with their readable views, from a
/// store the previous generation wrote (ADR 0047 N→N+1): the Convoy's quota (#3108), which
/// admission always granted empty and nothing reads. Opening ignores a table no kind declares,
/// so this only discards the retired rows. Run it before the store opens. Remove it one fleet
/// roll after the quota's removal ships, once no store holds the table.
pub fn drop_retired_tables(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    connection.execute_batch("DROP VIEW IF EXISTS \"convoy_quota_view\";\nDROP TABLE IF EXISTS \"convoy_quota\";")
}

/// Registers the engine's crew kind (#3629 layer 6): Vessel, the
/// placement lease, under its production code.
pub fn register_crew(kinds: &mut KindRegistry) -> Result<(), RegistryError> {
    kinds.register::<Resource<vessel::Vessel>>()?;
    Ok(())
}

/// The machine diagram of each kind [`register_crew`] registers, by kind name.
pub fn crew_diagrams() -> std::collections::BTreeMap<String, String> {
    [(Resource::<vessel::Vessel>::KIND.to_owned(), vessel::Vessel::definition().mermaid(flotilla_state_machines::Diagram::Full))].into()
}

fn refused(error: impl std::error::Error + Send + Sync + 'static) -> LogError {
    LogError::new(error)
}

// A panicking write drops rusqlite's rollback-on-drop transaction and its
// Pending overlay (IDs and watermarks). Shared caches contain only committed
// IDs; a partially promoted commit leaves valid entries or cache misses. Reads
// cache only committed IDs, and identity removal only invalidates cache entries.
// No resource state is cached in Store, so there is nothing to rebuild here.
// Recover as the provisioning home wrappers do above this log.
fn lock_store(store: &StoreHandle) -> StoreGuard<'_> {
    store.lock().unwrap_or_else(|poisoned| {
        let store = poisoned.into_inner();
        tracing::warn!(home = store.home(), "recovering poisoned decided-store lock");
        store
    })
}

/// One home's engine log over a shared [`Store`]. Every kind the store
/// registers is written through the same handle, so a consumer's `Receive`
/// and the Message it names commit to one store and home.
#[derive(Clone)]
pub struct StoreLog {
    store: Arc<StoreHandle>,
    home: String,
    receipts: ReceiptLog,
}

impl StoreLog {
    /// A log homed at the store's own home, accepting AgentSession, Person and Service receipts.
    pub fn new(store: Arc<StoreHandle>) -> Self {
        let home = lock_store(&store).home().to_owned();
        Self { store, home, receipts: ReceiptLog::new() }
    }

    /// Accept `Receive` receipts from consumer kind `M` too.
    pub fn consumer<M: Consumer>(mut self) -> Self
    where
        M::Spec: KindSpec,
        M::Edge: Cbor,
    {
        self.receipts = self.receipts.consumer::<M>();
        self
    }

    pub fn store(&self) -> &Arc<StoreHandle> {
        &self.store
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    /// An empty receipt log that decodes this log's consumer kinds.
    pub fn receipt_kinds(&self) -> ReceiptLog {
        self.receipts.clone()
    }

    /// The receipts about each of `messages` homed here, from `Read::about`:
    /// every entry naming the message as its master, paged in time order.
    pub fn receipt_log<'a>(&self, read: &mut Read<'_>, messages: impl IntoIterator<Item = &'a str>) -> Result<ReceiptLog, ReadError> {
        let mut log = self.receipts.clone();
        for message in messages {
            let Some(id) = read.resolve(Resource::<Message>::CODE, message, &self.home)? else { continue };
            let entries = about(read, id)?;
            log.insert(message, &entries).map_err(|error| ReadError::Decode(error.to_string()))?;
        }
        Ok(log)
    }
}

/// A home over `log` holding every resource of `M`'s kind homed at the log's home, as the store
/// holds them, so a restart keeps writing where it left off.
pub fn open_home<M>(log: StoreLog) -> Result<flotilla_state_machines::Home<M, StoreLog>, String>
where
    M: Machine,
    Resource<M>: Kind,
    StoreLog: Log<M>,
{
    let store = log.clone();
    open_home_with(&store, log)
}

/// [`open_home`] writing through `log`, a log built over `store` for a kind whose writes take
/// more than the store log's (an Environment's Realise and its launch's facts).
pub fn open_home_with<M, L>(store: &StoreLog, log: L) -> Result<flotilla_state_machines::Home<M, L>, String>
where
    M: Machine,
    Resource<M>: Kind,
    L: Log<M>,
{
    let home = store.home().to_owned();
    let stored = {
        let mut store = lock_store(store.store());
        let mut read = store.read().map_err(|error| error.to_string())?;
        let mut stored = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<M>>(&home, after, 64).map_err(|error| error.to_string())?;
            for id in page.ids {
                if let Some(loaded) = read.load::<Resource<M>>(id).map_err(|error| error.to_string())? {
                    stored.push(loaded.value.with_identity(&loaded.name, loaded.version));
                }
            }
            after = page.next;
            if after.is_none() {
                break stored;
            }
        }
    };
    let mut opened =
        flotilla_state_machines::Home::with_log(home.clone(), log).map_err(|errors| format!("{} home: {errors:?}", M::NAME))?;
    for resource in stored {
        opened.restore(resource).map_err(|error| format!("{} home: {error:?}", M::NAME))?;
    }
    Ok(opened)
}

/// Every entry about resource `id`, paged through `Read::about`.
pub fn about(read: &mut Read<'_>, id: flotilla_schema_sqlite::ResourceId) -> Result<Vec<Event>, StoreError> {
    let mut entries = Vec::new();
    let mut after: Option<AboutCursor> = None;
    loop {
        let page = read.about(id, after.as_ref(), 32)?;
        entries.extend(page.events);
        after = page.next;
        if after.is_none() {
            return Ok(entries);
        }
    }
}

/// A read the port could not serve.
#[derive(Debug)]
pub enum ReadError {
    Store(StoreError),
    /// A `Receive` entry whose payload does not decode.
    Decode(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::Decode(error) => write!(f, "receipt entry does not decode: {error}"),
        }
    }
}
impl std::error::Error for ReadError {}
impl From<StoreError> for ReadError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl StoreLog {
    /// The change a home's write opens (#4024): its writer and time, and as its items the records
    /// its reads named. Which reads name records is the open question on #4024; until it is ruled,
    /// none is an item.
    fn change(&self, writer: &str, at: Timestamp, reads: &[InputRead]) -> NewChange {
        let items: Vec<ChangeItem> = reads.iter().filter_map(|_| None).collect();
        NewChange::new(writer, at).with_items(items)
    }

    /// Creates `resource` under a change whose items are `items`: the records its requester read
    /// to make it, each at the version read (#4024 ruling 1), such as a Checkout's holder Convoy.
    /// A launch is numbered by the home sequence its create takes, in this transaction
    /// ([`Resource::numbered`]); the resource as committed is returned.
    pub fn create_with_items<M>(
        &self,
        writer: &str,
        at: Timestamp,
        resource: Resource<M>,
        items: Vec<ChangeItem>,
    ) -> Result<Resource<M>, LogError>
    where
        M: Machine,
        M::Spec: KindSpec<State = M::State>,
        M::State: StateData + Labelled<Label = Label<M>>,
        M::Write: StoredWrite<Resource<M>>,
        EdgePayload<M::Edge>: Cbor,
        M::Edge: Cbor,
    {
        self.insert_created(&NewChange::new(writer, at).with_items(items), resource)
    }

    fn insert_created<M>(&self, change: &NewChange, resource: Resource<M>) -> Result<Resource<M>, LogError>
    where
        M: Machine,
        M::Spec: KindSpec<State = M::State>,
        M::State: StateData + Labelled<Label = Label<M>>,
        M::Write: StoredWrite<Resource<M>>,
        EdgePayload<M::Edge>: Cbor,
        M::Edge: Cbor,
    {
        let mut store = lock_store(&self.store);
        let mut write = store.write().map_err(refused)?;
        let change = write.open_change(change).map_err(refused)?;
        // The insert below takes the next home sequence: the launch's number and its version.
        let next = write.last_sequence(&self.home).map_err(refused)? + 1;
        let resource = resource.numbered(next);
        let stamp = write.insert(resource.name(), &resource, change).map_err(refused)?;
        // The resource as committed: the insert stamps every records row with the create's
        // version, so the home's copy must carry those stamps or it differs from what storage holds.
        let stored = write.load::<Resource<M>>(stamp.id).map_err(refused)?.ok_or_else(|| LogError::new("created resource not found"))?;
        write.commit().map_err(refused)?;
        Ok(stored.value)
    }
}

impl<M> Log<M> for StoreLog
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    M::Write: StoredWrite<Resource<M>>,
    EdgePayload<M::Edge>: Cbor,
    M::Edge: Cbor,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError> {
        if M::launch(resource.spec()).is_some() {
            return Err(LogError::new("a launch is created through Log::create_resource, which numbers it"));
        }
        let change = self.change(writer, at, reads);
        Ok(self.insert_created(&change, resource.clone())?.version())
    }

    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<M>,
        reads: &[InputRead],
    ) -> Result<Resource<M>, LogError> {
        let change = self.change(writer, at, reads);
        self.insert_created(&change, resource)
    }

    /// An empty change: it takes the next home sequence, and replicas receive it as a change
    /// with no entries, so the home's prefix stays whole.
    fn reserve(&mut self, writer: &str, at: Timestamp) -> Result<u64, LogError> {
        let mut store = lock_store(&self.store);
        let mut write = store.write().map_err(refused)?;
        let change = write.open_change(&NewChange::new(writer, at)).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(change.0)
    }

    /// Refuses a number that is not an empty change at this home: one never given, or given to
    /// another write.
    fn create_reserved(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<M>,
        n: u64,
        reads: &[InputRead],
    ) -> Result<Resource<M>, LogError> {
        let change = self.change(writer, at, reads);
        let mut store = lock_store(&self.store);
        let mut write = store.write().map_err(refused)?;
        let reserved = n > 0
            && write.change(&self.home, n).map_err(refused)?.is_some()
            && write.changes_since(&self.home, n - 1, 1).map_err(refused)?.first().is_none_or(|entry| entry.home_seq != n);
        if !reserved {
            return Err(LogError::new(format!("launch number {n} is not a reservation at {}", self.home)));
        }
        let change = write.open_change(&change).map_err(refused)?;
        let resource = resource.numbered(n);
        let stamp = write.insert(resource.name(), &resource, change).map_err(refused)?;
        let stored = write.load::<Resource<M>>(stamp.id).map_err(refused)?.ok_or_else(|| LogError::new("created resource not found"))?;
        write.commit().map_err(refused)?;
        Ok(stored.value)
    }

    fn edge(&mut self, writer: &str, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        self.edge_with_writes(writer, previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        writer: &str,
        previous: &StoredState<M>,
        next: &Resource<M>,
        event: &EdgeEventOf<M>,
        writes: &[M::Write],
    ) -> Result<Version, LogError> {
        self.edge_and(writer, previous, next, event, writes, |_, _| Ok(()))
    }
}

impl StoreLog {
    /// [`Log::edge_with_writes`], with `also` writing other records in the edge's own
    /// transaction and change: they commit with the edge or not at all. A log for a kind whose
    /// edge creates a record beside it (an Environment's Realise and its launch's facts) is built
    /// on this.
    pub fn edge_and<M>(
        &mut self,
        writer: &str,
        previous: &StoredState<M>,
        next: &Resource<M>,
        event: &EdgeEventOf<M>,
        writes: &[M::Write],
        also: impl FnOnce(&mut Write<'_>, ChangeId) -> Result<(), StoreError>,
    ) -> Result<Version, LogError>
    where
        M: Machine,
        M::Spec: KindSpec<State = M::State>,
        M::State: StateData + Labelled<Label = Label<M>>,
        M::Write: StoredWrite<Resource<M>>,
        EdgePayload<M::Edge>: Cbor,
        M::Edge: Cbor,
    {
        // An edge's master is a Message homed beside its consumer (ADR 0064).
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: self.home.clone() };
        let edge = next.stored_edge_about(previous, event, master).map_err(refused)?;
        let edge = writes.iter().try_fold(edge, |edge, write| write.store(edge)).map_err(refused)?;
        let mut store = lock_store(&self.store);
        let mut write = store.write().map_err(refused)?;
        let id =
            write.resolve(Resource::<M>::CODE, next.name(), &self.home).map_err(refused)?.ok_or_else(|| LogError::new("not stored"))?;
        let change = write.open_change(&self.change(writer, event.at, &event.arguments.reads)).map_err(refused)?;
        let stamp = write.apply_edge(Stamp { id, version: next.version().0 }, edge, change).map_err(refused)?;
        also(&mut write, change).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }
}

/// The Message log: admission is assigned in its create transaction, as the
/// home sequence that create takes, so Pending order is the home's order.
#[derive(Clone)]
pub struct MessageLog {
    inner: StoreLog,
}

impl std::ops::Deref for MessageLog {
    type Target = StoreLog;
    fn deref(&self) -> &StoreLog {
        &self.inner
    }
}

impl MessageLog {
    pub fn new(store: Arc<StoreHandle>) -> Self {
        Self { inner: StoreLog::new(store) }
    }

    /// The consumer log sharing this Message log's store and home.
    pub fn consumers(&self) -> StoreLog {
        self.inner.clone()
    }
}

impl Log<Message> for MessageLog {
    fn create(&mut self, _: &str, _: Timestamp, _: &Resource<Message>, _: &[InputRead]) -> Result<Version, LogError> {
        Err(LogError::new("Message creation requires engine Home to call Log::create_resource"))
    }

    fn create_resource(
        &mut self,
        writer: &str,
        at: Timestamp,
        resource: Resource<Message>,
        reads: &[InputRead],
    ) -> Result<Resource<Message>, LogError> {
        let change = self.inner.change(writer, at, reads);
        let mut store = lock_store(&self.inner.store);
        let mut write = store.write().map_err(refused)?;
        let change = write.open_change(&change).map_err(refused)?;
        let mut spec = resource.spec().clone();
        spec.admitted_seq = write.last_sequence(&self.inner.home).map_err(refused)? + 1;
        let resource = resource.with_spec(spec);
        let stamp = write.insert(resource.name(), &resource, change).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), Version(stamp.version)))
    }

    fn edge(
        &mut self,
        writer: &str,
        previous: &StoredState<Message>,
        next: &Resource<Message>,
        event: &EdgeEventOf<Message>,
    ) -> Result<Version, LogError> {
        <StoreLog as Log<Message>>::edge(&mut self.inner, writer, previous, next, event)
    }
}

#[cfg(test)]
mod machine_registry_tests {
    #[test]
    fn every_registered_kind_has_its_machine_diagram() {
        let registry = super::machine_registry().unwrap();
        assert_eq!(registry.diagrams.len(), registry.kinds.iter().count());
        for kind in registry.kinds.iter() {
            assert!(registry.diagrams[&kind.kind].contains("stateDiagram"));
        }
    }
}
