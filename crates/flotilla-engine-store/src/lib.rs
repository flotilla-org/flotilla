//! The ADR 0064 engine's storage port over `flotilla-schema-sqlite` (#3317).
//!
//! [`StoreLog`] is the engine's [`Log`] over one [`Store`]: a create is one
//! `insert` and an edge one `apply_edge`, each in its own write transaction.
//! [`MessageLog`] adds Message admission, assigned inside the create's
//! transaction. Receipts are served as a [`ReceiptLog`] from `Read::about`.

use std::sync::{Arc, Mutex};

use flotilla_schema::{Cbor, Event, Identity, Kind, KindRegistry, KindSpec, Labelled, Origin, RegistryError, StateData, Timestamp};
use flotilla_schema_sqlite::{AboutCursor, Read, Stamp, Store, StoreError};
use flotilla_state_machines::{
    machines::{
        agent_session::AgentSession,
        awaiting_reply::AwaitingReply,
        consumer::Consumer,
        convoy, ensure,
        message::{Message, ReceiptLog},
        vessel,
    },
    EdgeEventOf, EdgePayload, InputRead, Label, Log, LogError, Machine, Resource, StoredState, StoredWrite, Version,
};

/// The kinds the `[primitives]` switch moves: Message, and the minimal
/// AgentSession its receipts are written on. Codes are the production
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
    register!(convoy::Convoy);
    Ok(MachineRegistry { kinds, diagrams })
}
pub fn registry() -> Result<KindRegistry, RegistryError> {
    Ok(machine_registry()?.kinds)
}

/// The kinds the Convoy stack moves (#3587): Convoy, with its seats,
/// promises, submissions and duties as its records, and AwaitingReply. Both
/// codes are the production registry's. The write path (layer 6) moved
/// Convoy into [`machine_registry`]; AwaitingReply follows with the send path
/// (layer 7). The benchmark opens a store with both.
pub fn convoy_registry() -> Result<KindRegistry, RegistryError> {
    let mut kinds = KindRegistry::new();
    kinds.register::<Resource<convoy::Convoy>>()?;
    kinds.register::<Resource<AwaitingReply>>()?;
    Ok(kinds)
}

/// Registers the engine's crew kinds the `[primitives]` switch moves (#3629 layer 6): Vessel,
/// the placement lease, and Ensure, each under its production code.
pub fn register_crew(kinds: &mut KindRegistry) -> Result<(), RegistryError> {
    kinds.register::<Resource<vessel::Vessel>>()?;
    kinds.register::<Resource<ensure::Ensure>>()?;
    Ok(())
}

/// The machine diagram of each kind [`register_crew`] registers, by kind name.
pub fn crew_diagrams() -> std::collections::BTreeMap<String, String> {
    [
        (Resource::<vessel::Vessel>::KIND.to_owned(), vessel::Vessel::definition().mermaid(flotilla_state_machines::Diagram::Full)),
        (Resource::<ensure::Ensure>::KIND.to_owned(), ensure::Ensure::definition().mermaid(flotilla_state_machines::Diagram::Full)),
    ]
    .into()
}

fn refused(error: impl std::error::Error + Send + Sync + 'static) -> LogError {
    LogError::new(error)
}

/// One home's engine log over a shared [`Store`]. Every kind the store
/// registers is written through the same handle, so a consumer's `Receive`
/// and the Message it names commit to one store and home.
#[derive(Clone)]
pub struct StoreLog {
    store: Arc<Mutex<Store>>,
    home: String,
    receipts: ReceiptLog,
}

impl StoreLog {
    /// A log homed at the store's own home, accepting AgentSession receipts.
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        let home = store.lock().expect("store writer").home().to_owned();
        Self { store, home, receipts: ReceiptLog::new() }
    }

    /// Accept `Receive` receipts from consumer kind `M` too.
    pub fn consumer<M: Consumer>(mut self) -> Self
    where
        M::Spec: KindSpec,
        EdgePayload<M::Edge>: Cbor,
    {
        self.receipts = self.receipts.consumer::<M>();
        self
    }

    pub fn store(&self) -> &Arc<Mutex<Store>> {
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
    let home = log.home().to_owned();
    let stored = {
        let mut store = log.store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

impl<M> Log<M> for StoreLog
where
    M: Machine,
    M::Spec: KindSpec<State = M::State>,
    M::State: StateData + Labelled<Label = Label<M>>,
    M::Write: StoredWrite<Resource<M>>,
    EdgePayload<M::Edge>: Cbor,
{
    fn create(&mut self, writer: &str, at: Timestamp, resource: &Resource<M>, reads: &[InputRead]) -> Result<Version, LogError> {
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut store = self.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        let stamp = write.insert(resource.name(), resource, &origin).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Version(stamp.version))
    }
    fn edge(&mut self, previous: &StoredState<M>, next: &Resource<M>, event: &EdgeEventOf<M>) -> Result<Version, LogError> {
        self.edge_with_writes(previous, next, event, &[])
    }

    fn edge_with_writes(
        &mut self,
        previous: &StoredState<M>,
        next: &Resource<M>,
        event: &EdgeEventOf<M>,
        writes: &[M::Write],
    ) -> Result<Version, LogError> {
        // An edge's master is a Message homed beside its consumer (ADR 0064).
        let master = |name: &str| Identity { kind: Resource::<Message>::CODE, name: name.to_owned(), home: self.home.clone() };
        let edge = next.stored_edge_about(previous, event, master).map_err(refused)?;
        let edge = writes.iter().try_fold(edge, |edge, write| write.store(edge)).map_err(refused)?;
        let mut store = self.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        let id =
            write.resolve(Resource::<M>::CODE, next.name(), &self.home).map_err(refused)?.ok_or_else(|| LogError::new("not stored"))?;
        let stamp = write.apply_edge(Stamp { id, version: next.version().0 }, edge).map_err(refused)?;
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
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
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
        let origin = Origin::new(writer, at).with_payload(&reads.to_vec()).map_err(refused)?;
        let mut store = self.inner.store.lock().expect("store writer");
        let mut write = store.write().map_err(refused)?;
        let mut spec = resource.spec().clone();
        spec.admitted_seq = write.last_sequence(&self.inner.home).map_err(refused)? + 1;
        let resource = resource.with_spec(spec);
        let stamp = write.insert(resource.name(), &resource, &origin).map_err(refused)?;
        write.commit().map_err(refused)?;
        Ok(Resource::stored(resource.name(), resource.spec().clone(), resource.stored_state().clone(), Version(stamp.version)))
    }

    fn edge(
        &mut self,
        previous: &StoredState<Message>,
        next: &Resource<Message>,
        event: &EdgeEventOf<Message>,
    ) -> Result<Version, LogError> {
        <StoreLog as Log<Message>>::edge(&mut self.inner, previous, next, event)
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
