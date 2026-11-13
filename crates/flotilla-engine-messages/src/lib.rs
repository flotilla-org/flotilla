//! Message on the engine behind the `[primitives]` switch (#3317).
//!
//! [`MessagePrimitives`] is the new path's composition at one home: the
//! decided store, the engine inbox over it ([`SqliteInbox`]), the minimal
//! AgentSession records receipts are written on, and the observations the
//! Message gates read. Producers keep building their intents; the receiver
//! home's admission ([`MessagePrimitives::admit`]) maps an intent onto the
//! engine spec and enqueues it, so routing stays where the receiver-home
//! router already puts it. Reads of old-store state go through the named
//! ports here ([`OldStoreRecords`]), so each kind that moves later swaps
//! one implementation.

pub mod attention;
pub mod attentions;
pub mod awaiting;
pub mod convert;
pub mod ports;
pub mod records;
pub mod roles;
pub mod sweep;
pub mod view;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use self::{
    convert::{Conversion, ReferenceAddresses},
    roles::{designated_fleet_roles, FleetRoles},
    view::{old_spec, old_view, record_name, ViewLookups},
};
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_engine_store::StoreLog;
use flotilla_inbox::{sqlite::SqliteInbox, ConsumerLog, ConsumerPort, InboxBackend, Observations, ReceiptScope, Supervision};
use flotilla_messaging::MessageResolution;
use flotilla_resources::{
    address::{artifact_address, record_name as address_record_name, repository_address},
    Artifact, Convoy, ForgeSpec, Message as OldMessage, MessageReference as OldReference, MessageSpec as OldSpec, Project,
    ReadResourceObject, Repository, ResourceError, ResourceObject, ResourceProvenance, TerminalSession, TerminalSessionSource,
};
use flotilla_schema::{CreatedAt, Identity, IndexQuery, Kind as StoredKind, Timestamp};
use flotilla_schema_sqlite::{stream::Service, StoreError};
use flotilla_state_machines::{
    machines::{
        agent_session::{AgentSession, MemoryAgentSessionWorld, Receive},
        consumer::{ConsumerKind, Receiver},
        message::{
            Deliver, DropReason, MemoryMessageWorld, Message, MessageBody, MessageLabel, MessageRef, MessageReference, MessageSpec,
            MessageState, Relation, RequestBody, Supersedes,
        },
        person::{self, Person},
        service::{self as service_machine, Service as ServiceMachine},
    },
    Home, Resource, Transport,
};
use flotilla_store::{ResourceBackend, WeakResourceBackend};

/// The world the Message gates read at this home: a snapshot the delivery
/// adapter and admission keep current, and the home clock.
pub struct GateWorld {
    world: Mutex<MemoryMessageWorld>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl GateWorld {
    pub fn new(clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        Self { world: Mutex::new(MemoryMessageWorld::default()), clock }
    }

    /// Edit the observed world; the next gate reads the result.
    pub fn update<T>(&self, edit: impl FnOnce(&mut MemoryMessageWorld) -> T) -> T {
        edit(&mut self.world.lock().expect("gate world"))
    }
}

impl Observations for GateWorld {
    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }
    fn with_snapshot(&self, read: &mut dyn FnMut(&MemoryMessageWorld)) {
        read(&self.world.lock().expect("gate world"));
    }
}

/// Dead letters go to the sender's supervisor. A fleet role and a crew
/// alike have none declared on this path yet, as the old store dead-letters
/// only on an operator's batch failure, so no dead letter is sent.
struct NoSupervisors;

impl Supervision for NoSupervisors {
    fn supervisor(&self, _: &Address) -> Option<Address> {
        None
    }
    fn system_sender(&self) -> Address {
        // The inbox asks only when sending a dead letter to a supervisor, and none is declared;
        // declaring supervisors names the sender too.
        unreachable!("no supervisor is declared, so no dead letter is sent")
    }
}

pub use flotilla_resources::agent_activity::SCREEN_CAUSE;

/// How long a hook's activity stands over a screen read that disagrees.
pub const HOOK_STANDS_FOR: std::time::Duration = std::time::Duration::from_secs(120);

/// The minimal AgentSession records at this home (#3317): one per crew
/// launch, named by the launch (`<session>#<n>`), holding the receipts.
pub struct Sessions {
    home: Mutex<Home<AgentSession, StoreLog>>,
}

impl Sessions {
    /// Creates the session's record, Live and detached, unless it exists.
    pub fn ensure(&self, name: &str, now: Timestamp) -> Result<bool, String> {
        let mut home = self.home.lock().expect("session home");
        if home.get(name).is_some() {
            return Ok(false);
        }
        home.create(name, (), &MemoryAgentSessionWorld::default(), now).map_err(|error| format!("AgentSession `{name}`: {error:?}"))?;
        Ok(true)
    }

    /// Ends a session whose terminal is gone, as observed.
    pub fn end(
        &self,
        name: &str,
        reason: flotilla_state_machines::machines::agent_session::EndReason,
        now: Timestamp,
    ) -> Result<(), String> {
        use flotilla_state_machines::machines::agent_session::{End, SessionEnd};
        let mut home = self.home.lock().expect("session home");
        if home.get(name).is_none_or(|session| session.state().end_reason().is_some()) {
            return Ok(());
        }
        let mut world = MemoryAgentSessionWorld::default();
        let observed = flotilla_state_machines::Ref::<SessionEnd>::new(name);
        world.ends.put(&observed, reason);
        home.apply(name, End { reason, observed }, &world, now).map(|_| ()).map_err(|error| format!("AgentSession `{name}`: {error:?}"))
    }

    /// A keyed, in-memory hint for avoiding duplicate hook I/O. Observe rechecks
    /// under the writer lock, since another hook can win after this read.
    pub fn needs_observation(
        &self,
        name: &str,
        activity: flotilla_state_machines::machines::agent_session::Activity,
    ) -> Result<bool, String> {
        let home = self.home.lock().map_err(|_| "session home lock poisoned".to_string())?;
        Ok(home.get(name).is_none_or(|session| {
            session.state().end_reason().is_none() && session.state().observation().is_none_or(|old| old.activity != activity)
        }))
    }

    /// Records only a real hook activity change; repeated and late hooks write nothing.
    pub fn observe(
        &self,
        name: &str,
        activity: flotilla_state_machines::machines::agent_session::Activity,
        cause: String,
        now: Timestamp,
    ) -> Result<bool, String> {
        use flotilla_state_machines::{
            machines::agent_session::{AgentSessionRefusal, Observe},
            WriteError,
        };
        let mut home = self.home.lock().map_err(|_| "session home lock poisoned".to_string())?;
        // A hook may precede the delivery adapter's first consumer registration.
        // Materialize the same minimal record here, named by the validated launch;
        // the terminal and delivery adapter still own its lifecycle, not the hook.
        if home.get(name).is_none() {
            home.create(name, (), &MemoryAgentSessionWorld::default(), now).map_err(|error| format!("AgentSession `{name}`: {error:?}"))?;
        }
        match home.apply(name, Observe { activity, cause }, &MemoryAgentSessionWorld::default(), now) {
            Ok(_) => Ok(true),
            Err(WriteError::Refused(AgentSessionRefusal::UnchangedActivity | AgentSessionRefusal::AlreadyEnded)) => Ok(false),
            Err(error) => Err(format!("AgentSession `{name}`: {error:?}")),
        }
    }

    /// Records activity read from the agent's screen, with cause [`SCREEN_CAUSE`].
    /// A hook reports the agent itself, so its observation stands over a screen
    /// read while it is younger than [`HOOK_STANDS_FOR`]; a screen read still
    /// records an idle agent working again, and an agent waiting on a person,
    /// which some harnesses' hooks (Codex) never report.
    pub fn observe_screen(
        &self,
        name: &str,
        activity: flotilla_state_machines::machines::agent_session::Activity,
        now: Timestamp,
    ) -> Result<bool, String> {
        use flotilla_state_machines::machines::agent_session::Activity;
        {
            let home = self.home.lock().map_err(|_| "session home lock poisoned".to_string())?;
            if let Some(old) = home.get(name).and_then(|session| session.state().observation()) {
                let from_hook = old.activity_cause != SCREEN_CAUSE;
                let fresh = now.since(old.activity_since) < HOOK_STANDS_FOR.as_micros() as i64;
                let screen_wins = activity == Activity::NeedsInput || (old.activity == Activity::Idle && activity == Activity::Working);
                if from_hook && fresh && !screen_wins {
                    return Ok(false);
                }
            }
        }
        self.observe(name, activity, SCREEN_CAUSE.to_owned(), now)
    }

    /// One keyed read of a session; absence means no such record.
    ///
    /// # Panics
    /// A poisoned home is an invalid writer invariant, not an absent record.
    /// This existing read API fails fast; hook observers instead use the fallible
    /// `needs_observation` and `observe` methods so attention handling can continue.
    pub fn get(&self, name: &str) -> Option<Resource<AgentSession>> {
        self.home.lock().expect("session home").get(name).cloned()
    }
}

/// The inbox's consumer port for one session: `Receive` on its own record.
struct SessionReceipts {
    sessions: Arc<Sessions>,
    name: String,
    scope: Option<ReceiptScope>,
}

impl ConsumerPort for SessionReceipts {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        self.scope.clone()
    }
    fn receive(&mut self, message: &str, transport: Transport, now: Timestamp) -> Result<(), flotilla_inbox::Error> {
        let mut home = self.sessions.home.lock().expect("session home");
        home.apply(&self.name, Receive { message: message.into(), transport }, &MemoryAgentSessionWorld::default(), now)
            .map(|_| ())
            .map_err(|error| flotilla_inbox::Error::Consumer(format!("{error:?}")))
    }
}

/// The one person consumer at the roll (#3938): bound to every person role's
/// inbox of every project and of the fleet. People, teams and identity come
/// with the principals model (#3457); any operator acts as this consumer.
pub const PERSON: &str = "operator";

/// The person consumers' records at this home (#3938), holding their receipts:
/// an acknowledgement or an answer, never a surface loading the message.
pub struct People {
    home: Mutex<Home<Person, StoreLog>>,
}

impl People {
    /// Creates the person's record, Active, unless it exists.
    pub fn ensure(&self, name: &str, now: Timestamp) -> Result<bool, String> {
        let mut home = self.home.lock().expect("person home");
        if home.get(name).is_some() {
            return Ok(false);
        }
        home.create(name, (), &(), now).map_err(|error| format!("Person `{name}`: {error:?}"))?;
        Ok(true)
    }

    /// One keyed read of a person's record; absence means no such record.
    pub fn get(&self, name: &str) -> Option<Resource<Person>> {
        self.home.lock().expect("person home").get(name).cloned()
    }
}

/// The inbox's consumer port for one person: `Receive` on their own record.
struct PersonReceipts {
    people: Arc<People>,
    name: String,
    scope: Option<ReceiptScope>,
}

impl ConsumerPort for PersonReceipts {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        self.scope.clone()
    }
    fn receive(&mut self, message: &str, transport: Transport, now: Timestamp) -> Result<(), flotilla_inbox::Error> {
        let mut home = self.people.home.lock().expect("person home");
        home.apply(&self.name, person::Receive { message: message.into(), transport }, &(), now)
            .map(|_| ())
            .map_err(|error| flotilla_inbox::Error::Consumer(format!("{error:?}")))
    }
}

/// The records of this home's services (#3108), the daemon code that takes messages from an
/// inbox, holding their receipts: one per service, created when it first registers and kept
/// across restarts. Daemon code that consumes no inbox has none.
pub struct Services {
    home: Mutex<Home<ServiceMachine, StoreLog>>,
}

impl Services {
    /// Creates the service's record, Running, unless it exists.
    pub fn ensure(&self, name: &str, now: Timestamp) -> Result<bool, String> {
        let mut home = self.home.lock().expect("service home");
        if home.get(name).is_some() {
            return Ok(false);
        }
        home.create(name, (), &(), now).map_err(|error| format!("Service `{name}`: {error:?}"))?;
        Ok(true)
    }

    /// One keyed read of a service's record; absence means no such record.
    pub fn get(&self, name: &str) -> Option<Resource<ServiceMachine>> {
        self.home.lock().expect("service home").get(name).cloned()
    }
}

/// What a gate's request to a service (an inspection, #3108) has come to, read from the
/// request's own Message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestAnswer {
    /// Not yet taken: the gate waits for the next change to the message.
    Waiting,
    /// The service took it: its receipt is written, after the records it wrote for it.
    Answered { receiver: Receiver },
    /// It will never be answered: dropped as undeliverable (the inbox stayed unbound), expired,
    /// superseded or withdrawn. The gate fails, never proceeds on older evidence.
    Failed { reason: String },
}

impl RequestAnswer {
    /// The answer `state` gives to a request sent to a service.
    pub fn of(state: &MessageState) -> RequestAnswer {
        match state {
            MessageState::Pending => RequestAnswer::Waiting,
            MessageState::Dropped { reason, .. } => RequestAnswer::Failed { reason: format!("the request was dropped: {reason:?}") },
            MessageState::Awaiting { receiver, .. } | MessageState::Settled { receiver: Some(receiver), .. } => {
                RequestAnswer::Answered { receiver: receiver.clone() }
            }
            MessageState::Settled { receiver: None, .. } => RequestAnswer::Failed { reason: "the request settled with no receiver".into() },
        }
    }
}

/// The inbox's consumer port for one service: `Receive` on its own record.
struct ServiceReceipts {
    services: Arc<Services>,
    name: String,
    scope: Option<ReceiptScope>,
}

impl ConsumerPort for ServiceReceipts {
    fn receipt_scope(&self) -> Option<ReceiptScope> {
        self.scope.clone()
    }
    fn receive(&mut self, message: &str, transport: Transport, now: Timestamp) -> Result<(), flotilla_inbox::Error> {
        let mut home = self.services.home.lock().expect("service home");
        home.apply(&self.name, service_machine::Receive { message: message.into(), transport }, &(), now)
            .map(|_| ())
            .map_err(|error| flotilla_inbox::Error::Consumer(format!("{error:?}")))
    }
}

/// Every stored record of machine `M` homed at `home`.
fn stored<M>(service: &Service, home: &str) -> Result<Vec<Resource<M>>, String>
where
    M: flotilla_state_machines::Machine,
    Resource<M>: StoredKind,
{
    service
        .with_store(|store| {
            let mut read = store.read()?;
            let mut records = Vec::new();
            let mut after = None;
            loop {
                let page = read.scan::<Resource<M>>(home, after, 64)?;
                for id in page.ids {
                    if let Some(loaded) = read.load::<Resource<M>>(id)? {
                        records.push(loaded.value);
                    }
                }
                after = page.next;
                if after.is_none() {
                    return Ok(records);
                }
            }
        })
        .map_err(|error| error.to_string())
}

/// The new path at one home.
pub struct MessagePrimitives {
    home: String,
    service: Arc<Service>,
    inbox: SqliteInbox,
    sessions: Arc<Sessions>,
    people: Arc<People>,
    services: Arc<Services>,
    awaiting: Arc<awaiting::AwaitingReplies>,
    world: Arc<GateWorld>,
    /// Weak: the backend holds this path as its Message port and attentions.
    backend: WeakResourceBackend,
    consumers: Mutex<BTreeSet<Receiver>>,
    receipts: StoreLog,
    charter_pass: Arc<tokio::sync::Mutex<()>>,
    charter_admission: Arc<tokio::sync::Mutex<()>>,
    bindings: Arc<dyn ports::RoleBindings>,
    conditions: Arc<dyn ports::ConditionReads>,
    home_reads: std::sync::OnceLock<Arc<dyn ports::HomeReads>>,
    /// The engine's vessels and terminal sessions, through which a convoy role's session is
    /// read; without them it is read by its labels.
    lifecycle: std::sync::OnceLock<Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>>,
    /// Signalled after an attention here is raised, cleared or acknowledged, and after another
    /// home's Messages are applied.
    attention_changes: tokio::sync::broadcast::Sender<()>,
    /// Signalled after a person acknowledges an attention here.
    acknowledgements: tokio::sync::broadcast::Sender<()>,
    /// One follower of the Message log, shared by every reader woken by its commits.
    changes: flotilla_engine_store::watch::SharedChanges,
    /// Message rows the reads loaded and converted, since this path opened.
    loaded: std::sync::atomic::AtomicU64,
    converted: std::sync::atomic::AtomicU64,
}

/// What this path's Message reads have cost since it opened: rows loaded from
/// the store, and rows converted to the old shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowsRead {
    pub loaded: u64,
    pub converted: u64,
}

impl MessagePrimitives {
    /// Opens the new path over `service`, homed at `home` (this daemon's
    /// node id, the home every follower names it by).
    pub fn open(
        home: &str,
        service: Arc<Service>,
        backend: ResourceBackend,
        clock: Arc<dyn flotilla_resources::Clock>,
    ) -> Result<Self, String> {
        let store = service.store();
        let log = flotilla_engine_store::MessageLog::new(store);
        let receipts = log.consumers();
        let world = Arc::new(GateWorld::new(Arc::clone(&clock)));
        let inbox = SqliteInbox::open(home, log, world.clone(), Arc::new(NoSupervisors)).map_err(|error| error.to_string())?;
        let sessions = Arc::new(Sessions {
            home: Mutex::new(Home::with_log(home, receipts.clone()).map_err(|error| format!("AgentSession home: {error:?}"))?),
        });
        let people = Arc::new(People {
            home: Mutex::new(Home::with_log(home, receipts.clone()).map_err(|error| format!("Person home: {error:?}"))?),
        });
        let services = Arc::new(Services {
            home: Mutex::new(Home::with_log(home, receipts.clone()).map_err(|error| format!("Service home: {error:?}"))?),
        });
        // Consumer records persist; reopen them so a restart keeps their receipts.
        for session in stored::<AgentSession>(&service, home)? {
            sessions.home.lock().expect("session home").restore(session).map_err(|error| format!("{error:?}"))?;
        }
        for person in stored::<Person>(&service, home)? {
            people.home.lock().expect("person home").restore(person).map_err(|error| format!("{error:?}"))?;
        }
        for record in stored::<ServiceMachine>(&service, home)? {
            services.home.lock().expect("service home").restore(record).map_err(|error| format!("{error:?}"))?;
        }
        let awaiting = Arc::new(awaiting::AwaitingReplies::open(home, &service)?);
        // Attentions homed elsewhere arrive with their homes' entries; their readers read again then.
        let attention_changes = tokio::sync::broadcast::channel(16).0;
        let replicated = attention_changes.clone();
        service.on_applied(Arc::new(move |kinds| {
            if kinds.contains(&<Resource<Message> as StoredKind>::CODE) {
                let _ = replicated.send(());
            }
        }));
        let changes =
            flotilla_engine_store::watch::SharedChanges::new(Arc::clone(&service), [<Resource<Message> as StoredKind>::CODE], "messages");
        Ok(Self {
            home: home.to_owned(),
            changes,
            service,
            inbox,
            sessions,
            people,
            services,
            awaiting,
            world,
            consumers: Mutex::new(BTreeSet::new()),
            receipts,
            charter_pass: Arc::default(),
            charter_admission: Arc::default(),
            bindings: Arc::new(ports::OldRoleRouting { backend: backend.downgrade() }),
            conditions: Arc::new(ports::OldStoreConditions { backend: backend.downgrade() }),
            home_reads: std::sync::OnceLock::new(),
            lifecycle: std::sync::OnceLock::new(),
            attention_changes,
            acknowledgements: tokio::sync::broadcast::channel(16).0,
            backend: backend.downgrade(),
            loaded: std::sync::atomic::AtomicU64::new(0),
            converted: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The rows this path's Message reads have loaded and converted.
    pub fn rows_read(&self) -> RowsRead {
        RowsRead {
            loaded: self.loaded.load(std::sync::atomic::Ordering::Relaxed),
            converted: self.converted.load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn count_loaded(&self) {
        self.loaded.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn count_converted(&self, rows: usize) {
        self.converted.fetch_add(rows as u64, std::sync::atomic::Ordering::Relaxed);
    }

    fn backend(&self) -> Result<ResourceBackend, String> {
        ports::upgrade(&self.backend)
    }

    pub fn home(&self) -> &str {
        &self.home
    }
    pub fn inbox(&self) -> &SqliteInbox {
        &self.inbox
    }
    pub fn sessions(&self) -> &Arc<Sessions> {
        &self.sessions
    }
    /// This home's person consumer records (#3938).
    pub fn people(&self) -> &Arc<People> {
        &self.people
    }
    /// This home's services' records (#3108).
    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }
    /// This home's AwaitingReply records (#3587 layer 7).
    pub fn awaiting(&self) -> &Arc<awaiting::AwaitingReplies> {
        &self.awaiting
    }
    pub fn world(&self) -> &Arc<GateWorld> {
        &self.world
    }
    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }
    /// A tick for each commit after this call that writes a Message, this home's or one applied
    /// from another home's log, read from the log rather than by listing the messages.
    pub fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, ResourceError> {
        self.changes.subscribe().map_err(|error| ResourceError::other(format!("Message changes: {error}")))
    }
    /// The bindings port: who holds each role inbox, from today's role routing.
    pub fn bindings(&self) -> &Arc<dyn ports::RoleBindings> {
        &self.bindings
    }
    /// The conditions port: condition values over old-store records.
    pub fn conditions(&self) -> &Arc<dyn ports::ConditionReads> {
        &self.conditions
    }
    /// Installs the routed read for replica misses; the daemon's server owns
    /// the peers' record sockets.
    pub fn install_home_reads(&self, reads: Arc<dyn ports::HomeReads>) -> Result<(), String> {
        self.home_reads.set(reads).map_err(|_| "home reads are already installed".to_owned())
    }

    /// Installs the engine's vessel and terminal session reads; the composition root owns them.
    pub fn install_lifecycle_reads(&self, reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>) -> Result<(), String> {
        self.lifecycle.set(reads).map_err(|_| "lifecycle reads are already installed".to_owned())
    }

    /// Runs `spell` on the old-store records it names, each read by its key: a lookup
    /// `records` does not hold asks for its record, the asked records are read, and `spell`
    /// runs again, until it asks for nothing it has not had. `records` keeps what was read, so
    /// a later spelling over the same records reads only what is new to it.
    pub async fn spell<T>(&self, records: &mut OldStoreRecords, mut spell: impl FnMut(&OldStoreRecords) -> T) -> Result<T, ResourceError> {
        loop {
            let spelled = spell(records);
            let asked = records.take_asked();
            if asked.is_empty() {
                return Ok(spelled);
            }
            self.read_asked(records, asked).await?;
        }
    }

    /// `resources`, each with when it was created, in the old shape, each read on the records it
    /// names.
    pub async fn old_views(
        &self,
        namespace: &str,
        roles: &FleetRoles,
        resources: &[(CreatedAt, Resource<Message>)],
    ) -> Result<Vec<Result<ResourceObject<OldMessage>, String>>, ResourceError> {
        let mut records = OldStoreRecords::empty(namespace);
        let views = self
            .spell(&mut records, |records| {
                resources.iter().map(|(created, resource)| old_view(resource, *created, namespace, roles, records)).collect()
            })
            .await?;
        self.count_converted(resources.len());
        Ok(views)
    }

    /// When each of `messages`, held with its home, was created, read by its identity.
    fn created_of(&self, messages: &[(String, Resource<Message>)]) -> Result<Vec<CreatedAt>, ResourceError> {
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut created = Vec::with_capacity(messages.len());
                for (home, message) in messages {
                    let identity = Identity { kind: Resource::<Message>::CODE, name: message.name().to_owned(), home: home.clone() };
                    let id = read.resolve(identity.kind, &identity.name, home)?.ok_or(StoreError::NotCreated { identity })?;
                    created.push(read.creation_time(id)?);
                }
                Ok(created)
            })
            .map_err(|error| ResourceError::other(error.to_string()))
    }

    /// The records the messages `ids` held here name, read by key, for spelling them later.
    pub async fn records_for(&self, namespace: &str, roles: &FleetRoles, ids: &[String]) -> Result<Arc<OldStoreRecords>, ResourceError> {
        let mut resources = Vec::new();
        for id in ids {
            resources.extend(self.inbox.get_async(id).await.map_err(|error| ResourceError::other(error.to_string()))?);
        }
        let mut records = OldStoreRecords::empty(namespace);
        self.spell(&mut records, |records| {
            resources.iter().for_each(|resource| {
                let _ = old_spec(resource, namespace, roles, records);
            })
        })
        .await?;
        Ok(Arc::new(records))
    }

    /// Reads what `asked` names into `records`, each record by its key. A convoy role's session
    /// is read through its convoy's snapshot, the vessel launched for that work and the
    /// sessions of that vessel; the sessions of a role whose convoy names no vessel for it are
    /// read by their labels, as is every role's where the engine's reads are not installed.
    async fn read_asked(&self, records: &mut OldStoreRecords, mut asked: Wanted) -> Result<(), ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        let namespace = records.namespace.clone();
        // A seat is read through its convoy, so the convoy is read first.
        asked.convoys.extend(asked.seats.iter().map(|(convoy, _)| convoy.clone()).filter(|convoy| !records.read.convoys.contains(convoy)));
        if asked.forges {
            records.forges =
                backend.definitions::<flotilla_resources::Forge>(&namespace).list().await?.into_iter().map(|forge| forge.spec).collect();
        }
        for name in &asked.convoys {
            let copies = match backend.including_replicas::<Convoy>(&namespace).get_all(name).await {
                Ok(copies) => copies.items,
                Err(ResourceError::NotFound { .. }) => Vec::new(),
                Err(error) => return Err(error),
            };
            if copies.iter().any(|copy| !copy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal())) {
                records.live_convoys.insert(name.clone());
            }
            // This home's copy, else the first listed.
            let held =
                copies.iter().position(|copy| matches!(copy.provenance, ResourceProvenance::Local)).or((!copies.is_empty()).then_some(0));
            if let Some(copy) = held.map(|index| copies[index].clone()) {
                let home = match &copy.provenance {
                    ResourceProvenance::Local => self.home.clone(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
                };
                records.convoy_homes.insert(name.clone(), home);
                records.convoys.insert(name.clone(), copy.object);
            }
        }
        for name in &asked.terminals {
            if let Some(terminal) = found(backend.including_replicas::<TerminalSession>(&namespace).get(name).await)? {
                records.terminals.insert(name.clone(), terminal.object);
            }
        }
        for name in &asked.artifacts {
            if let Some(artifact) = found(backend.including_replicas::<Artifact>(&namespace).get(name).await)? {
                records.artifacts.insert(name.clone(), artifact.object);
            }
        }
        for name in &asked.repositories {
            if let Some(repository) = found(backend.including_replicas::<Repository>(&namespace).get(name).await)? {
                records.repositories.insert(name.clone(), repository.object);
            }
        }
        if asked.every_repository {
            for repository in backend.including_replicas::<Repository>(&namespace).list().await?.items {
                records.repositories.insert(repository.object.metadata.name.clone(), repository.object);
            }
        }
        for name in &asked.projects {
            if let Some(project) = found(backend.definitions::<Project>(&namespace).get(name).await)? {
                records.projects.insert(name.clone(), project);
            }
        }
        let mut labelled = asked.labelled.clone();
        for (convoy, role) in &asked.seats {
            match self.seat_sessions(records, convoy, role) {
                Some(sessions) => {
                    for session in sessions {
                        if let Some(terminal) = found(backend.including_replicas::<TerminalSession>(&namespace).get(&session).await)? {
                            records.terminals.insert(session, terminal.object);
                        }
                    }
                }
                None => {
                    labelled.insert((convoy.clone(), role.clone()));
                }
            }
        }
        for (convoy, role) in &labelled {
            let labels = BTreeMap::from([
                (flotilla_resources::CONVOY_LABEL.to_owned(), convoy.clone()),
                (flotilla_resources::ROLE_LABEL.to_owned(), role.clone()),
            ]);
            for terminal in backend.including_replicas::<TerminalSession>(&namespace).list_matching_labels(&labels).await?.items {
                records.terminals.insert(terminal.object.metadata.name.clone(), terminal.object);
            }
        }
        asked.labelled.extend(labelled);
        records.read.absorb(asked);
        Ok(())
    }

    /// The sessions of the vessel launched for the work holding `role` in `convoy`, newest
    /// launch at each home this store holds, since the vessel is homed where it was placed:
    /// through the convoy's snapshot and the engine's indexed vessel and terminal reads, one
    /// index read per home. `None` when those reads are not installed or the convoy's snapshot
    /// names no work for the role, and the sessions are read by their labels.
    fn seat_sessions(&self, records: &OldStoreRecords, convoy: &str, role: &str) -> Option<Vec<String>> {
        let lifecycle = self.lifecycle.get()?;
        let (vessels, terminals) = (lifecycle.vessels().ok()?, lifecycle.terminals().ok()?);
        let work = records.convoys.get(convoy).and_then(|held| snapshot_vessel(held, role))?;
        let convoy_home = records.convoy_homes.get(convoy)?;
        let launch_of = flotilla_resources::vessel_resource_name(flotilla_resources::convoy::launch_of(convoy), &work);
        let mut homes = BTreeSet::from([self.home.clone(), convoy_home.clone()]);
        homes.extend(self.service.homes());
        let mut sessions = Vec::new();
        for home in &homes {
            let vessel = match vessels.newest(convoy, convoy_home, &launch_of, Some(home)) {
                Ok(vessel) => vessel,
                Err(error) => {
                    tracing::debug!(%convoy, %role, %error, "a convoy role's vessel could not be read");
                    return None;
                }
            };
            let Some(vessel) = vessel else { continue };
            match terminals.of_vessel(&vessel.name) {
                Ok(launches) => sessions
                    .extend(launches.into_iter().filter(|launch| launch.role == role).map(|launch| launch.session_name().to_owned())),
                Err(error) => {
                    tracing::debug!(%convoy, %role, %error, "a convoy role's sessions could not be read");
                    return None;
                }
            }
        }
        Some(sessions)
    }

    /// The consumer identity of a crew session's record at this home.
    pub fn consumer(&self, session: &str) -> Receiver {
        Receiver { kind: ConsumerKind::AgentSession, id: session.to_owned(), host: self.home.clone() }
    }

    /// Registers a crew session as a consumer: its record exists, and the
    /// inbox writes its receipts there.
    pub async fn register(&self, session: &str, now: Timestamp) -> Result<Receiver, String> {
        let sessions = Arc::clone(&self.sessions);
        let name = session.to_owned();
        // A SQLite write: off the async workers.
        tokio::task::spawn_blocking(move || sessions.ensure(&name, now)).await.map_err(|error| error.to_string())??;
        let consumer = self.consumer(session);
        if self.consumers.lock().expect("consumers").insert(consumer.clone()) {
            let port =
                SessionReceipts { sessions: Arc::clone(&self.sessions), name: session.to_owned(), scope: self.receipts.receipt_scope() };
            if let Err(error) = self.inbox.register_async(consumer.clone(), port).await {
                self.consumers.lock().expect("consumers").remove(&consumer);
                return Err(error.to_string());
            }
        }
        Ok(consumer)
    }

    /// The person consumer's identity at this home (#3938).
    pub fn person(&self) -> Receiver {
        Receiver { kind: ConsumerKind::Person, id: PERSON.to_owned(), host: self.home.clone() }
    }

    /// Registers the person consumer: its record exists, and the inbox writes
    /// its receipts there.
    pub async fn register_person(&self, now: Timestamp) -> Result<Receiver, String> {
        let consumer = self.person();
        // Registered consumers already have their record; skip the blocking hop.
        if self.consumers.lock().expect("consumers").contains(&consumer) {
            return Ok(consumer);
        }
        let people = Arc::clone(&self.people);
        // A SQLite write: off the async workers.
        tokio::task::spawn_blocking(move || people.ensure(PERSON, now)).await.map_err(|error| error.to_string())??;
        if self.consumers.lock().expect("consumers").insert(consumer.clone()) {
            let port = PersonReceipts { people: Arc::clone(&self.people), name: PERSON.to_owned(), scope: self.receipts.receipt_scope() };
            if let Err(error) = self.inbox.register_async(consumer.clone(), port).await {
                self.consumers.lock().expect("consumers").remove(&consumer);
                return Err(error.to_string());
            }
        }
        Ok(consumer)
    }

    /// The consumer identity of service `name` at this home (#3108).
    pub fn service_receiver(&self, name: &str) -> Receiver {
        Receiver { kind: ConsumerKind::Service, id: name.to_owned(), host: self.home.clone() }
    }

    /// Registers service `name` as a consumer (#3108): its record exists, and the inbox writes
    /// its receipts there. It fills roles through ordinary bindings; other homes address the
    /// role, never the record.
    pub async fn register_service(&self, name: &str, now: Timestamp) -> Result<Receiver, String> {
        let consumer = self.service_receiver(name);
        // Registered consumers already have their record; skip the blocking hop.
        if self.consumers.lock().expect("consumers").contains(&consumer) {
            return Ok(consumer);
        }
        let services = Arc::clone(&self.services);
        let owned = name.to_owned();
        // A SQLite write: off the async workers.
        tokio::task::spawn_blocking(move || services.ensure(&owned, now)).await.map_err(|error| error.to_string())??;
        if self.consumers.lock().expect("consumers").insert(consumer.clone()) {
            let port =
                ServiceReceipts { services: Arc::clone(&self.services), name: name.to_owned(), scope: self.receipts.receipt_scope() };
            if let Err(error) = self.inbox.register_async(consumer.clone(), port).await {
                self.consumers.lock().expect("consumers").remove(&consumer);
                return Err(error.to_string());
            }
        }
        Ok(consumer)
    }

    /// What request `message`, homed here, sent to a service has come to (#3108). A request
    /// that is gone has failed.
    pub async fn request_answer(&self, message: &str) -> Result<RequestAnswer, String> {
        Ok(match self.inbox.get_async(message).await.map_err(|error| error.to_string())? {
            Some(message) => RequestAnswer::of(message.state()),
            None => RequestAnswer::Failed { reason: format!("request `{message}` is gone") },
        })
    }

    /// Asks the checkout inspector at `home` to inspect checkout `checkout` afresh, reading its
    /// branch's remote ref, and waits for its answer (#3108): a CheckoutInspection request to
    /// `host:<home>/role:checkout-inspector` from the teardown role, about
    /// `host:<home>/checkout:<checkout>`. Answered, the inspection is on the checkout's launch.
    /// It waits on the inbox's changes only; a request that will never be answered fails.
    pub async fn inspect_checkout(&self, namespace: &str, home: &str, checkout: &str) -> Result<(), String> {
        let subject = Address::root(AddressKind::Host, home)
            .and_then(|host| host.child(AddressKind::Checkout, checkout))
            .map_err(|error| format!("checkout {checkout} at {home} has no address: {error}"))?;
        let body = RequestBody::CheckoutInspection { remote: true };
        self.ask(namespace, roles::checkout_inspector(home)?, subject, body)
            .await
            .map_err(|reason| format!("checkout {checkout}'s inspection at {home} failed: {reason}"))
    }

    /// Asks this host's forge observer to look for a change request from `branch` of
    /// `repository`, and waits for its answer (#3108): a ChangeRequestBranch request to
    /// `host:<home>/role:forge-observer` from the teardown role, about the repository. Answered,
    /// what it found is on the change requests' records, read through their `by_branch` index.
    /// While the FleetDesignation is missing or invalid the request waits for it, as message
    /// delivery does: a checkout's branch validation then goes on once it is designated.
    pub async fn observe_branch(&self, namespace: &str, repository: &Address, branch: &str) -> Result<(), String> {
        let looked_up = async {
            self.designated_roles(namespace).await?;
            let body = RequestBody::ChangeRequestBranch { branch: branch.to_owned() };
            self.ask(namespace, roles::forge_observer(&self.home)?, repository.clone(), body).await
        };
        looked_up.await.map_err(|reason| format!("branch {branch} of {repository} could not be looked up: {reason}"))
    }

    /// The fleet roles once the FleetDesignation is designated: while it is missing or invalid,
    /// this waits on its record's changes.
    async fn designated_roles(&self, namespace: &str) -> Result<FleetRoles, String> {
        if let Ok(roles) = self.roles_or_wait(namespace).await? {
            return Ok(roles);
        }
        let designations = flotilla_store::WatchView::<flotilla_resources::FleetDesignation>::start(self.backend()?, namespace)
            .await
            .map_err(|error| format!("watch the FleetDesignation: {error}"))?;
        let mut changes = designations.subscribe();
        loop {
            // Read after subscribing, so a designation made in between is not missed.
            if let Ok(roles) = self.roles_or_wait(namespace).await? {
                return Ok(roles);
            }
            changes.changed().await.map_err(|_| "the FleetDesignation watch ended".to_owned())?;
        }
    }

    /// Sends `body` about `subject` to the service inbox `inbox` as the teardown role, and
    /// waits for its answer on the inbox's changes only. The error is why it was not answered.
    async fn ask(&self, namespace: &str, inbox: Address, subject: Address, body: RequestBody) -> Result<(), String> {
        use futures::StreamExt;
        let roles = self.roles(namespace).await?;
        let mut spec = MessageSpec::new(
            roles.address("teardown").ok_or("the teardown role is not declared")?,
            inbox.clone(),
            Relation::System,
            flotilla_schema::Frozen::new(MessageBody::Request(body)).map_err(|error| error.to_string())?,
        );
        spec.references.push(MessageReference { address: subject, revision: "1".into(), subject: true });
        // The watch is opened before the request, so its answer cannot be missed.
        let mut watch = self.inbox.watch(&inbox, None).await.map_err(|error| error.to_string())?;
        let message = self.inbox.enqueue(spec).await.map_err(|error| error.to_string())?;
        loop {
            match self.request_answer(&message).await? {
                RequestAnswer::Answered { .. } => return Ok(()),
                RequestAnswer::Failed { reason } => return Err(reason),
                RequestAnswer::Waiting => {}
            }
            match watch.next().await {
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(format!("watch {inbox}: {error}")),
                None => {
                    return match self.request_answer(&message).await? {
                        RequestAnswer::Answered { .. } => Ok(()),
                        _ => Err(format!("the watch of {inbox} ended before the request was answered")),
                    }
                }
            }
        }
    }

    /// A service here could not do what request `message` asked (#3108): it publishes the
    /// delivery failure, the evidence an undeliverable drop needs, and drops the request so its
    /// sender's wait fails instead of reading what is not there.
    pub async fn fail_request(&self, message: &str) -> Result<(), String> {
        self.world.update(|world| world.delivery_failure.put(&flotilla_state_machines::Ref::new(message), ()));
        self.inbox.drop(message, DropReason::Undeliverable).await.map_err(|error| error.to_string())
    }

    /// Every person role's inbox in `namespace` (#3938): each person role
    /// under the fleet and under every declared project, and each role a
    /// project's charter declares a principal holds. A convoy with no project
    /// is addressed under its namespace, so the namespace counts as one. The
    /// person consumer is bound to all of them.
    pub async fn person_inboxes(&self, namespace: &str) -> Result<BTreeSet<Address>, String> {
        let roles = self.roles(namespace).await?;
        // Only the projects' charters name person roles: no other record is read.
        let declared: Vec<ResourceObject<Project>> =
            self.backend()?.definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        let records = OldStoreRecords::from_records(namespace, Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), declared);
        let mut inboxes: BTreeSet<Address> = roles::PERSON_ROLES.iter().filter_map(|role| roles.person(role.name)).collect();
        let projects: BTreeSet<&str> = records.projects().chain([namespace]).collect();
        for project in projects {
            let Ok(address) = Address::project(project) else {
                tracing::debug!(%project, "a project with no address has no person inboxes");
                continue;
            };
            let declared = records.principal_backed(project).map(|(role, _)| role);
            for role in roles::PERSON_ROLES.iter().map(|role| role.name).chain(declared) {
                match address.child(AddressKind::Role, role) {
                    Ok(inbox) => {
                        inboxes.insert(inbox);
                    }
                    Err(error) => tracing::debug!(%project, %role, %error, "a person role with no address"),
                }
            }
        }
        Ok(inboxes)
    }

    /// The inbox messages to `receiver`, an old address, are admitted to. The address alone and
    /// the fleet roles name it: no other record is read.
    pub async fn inbox_for(&self, namespace: &str, receiver: &str) -> Result<Address, String> {
        let roles = self.roles(namespace).await?;
        Conversion { roles: &roles, forges: &[], records: &convert::NoRecords }.inbox(receiver)
    }

    /// The messages waiting in the person roles' inboxes (#3938), at every
    /// home this replica follows, with their homes: each is acknowledged or
    /// answered at its own home. Reading them writes no receipt.
    pub async fn person_messages(&self, namespace: &str) -> Result<Vec<(String, Resource<Message>)>, String> {
        let mut waiting = Vec::new();
        for inbox in self.person_inboxes(namespace).await? {
            waiting.extend(self.pending_in(&inbox)?);
        }
        // Each home's messages in its admission order, as one listing.
        let homes = self.service.homes();
        let place = |home: &str| homes.iter().position(|followed| followed == home);
        waiting.sort_by_key(|(home, message)| (place(home), message.spec().admitted_seq));
        Ok(waiting)
    }

    /// The message a person names by its old record name, by its exact identity in each person
    /// role's inbox: its id and home, or `None` when no such inbox holds it. A name in more than
    /// one of those inboxes is refused, naming them, rather than one picked.
    pub async fn locate_for_person(&self, namespace: &str, record: &str) -> Result<Option<(String, String)>, String> {
        let mut found = Vec::new();
        for inbox in self.person_inboxes(namespace).await? {
            if let Some(located) = self.locate_in(&inbox, record)? {
                found.push((inbox, located));
            }
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some(found.remove(0).1)),
            _ => {
                let inboxes: Vec<String> = found.iter().map(|(inbox, _)| inbox.to_string()).collect();
                Err(format!(
                    "message `{record}` is in more than one person role's inbox ({}), so which one is meant is unknown",
                    inboxes.join(", ")
                ))
            }
        }
    }

    async fn person_message(&self, namespace: &str, record: &str) -> Result<(String, String), String> {
        self.locate_for_person(namespace, record)
            .await?
            .ok_or_else(|| format!("message `{record}` is in no person role's inbox in this home's replica"))
    }

    /// The person acknowledges a message waiting in one of their role inboxes
    /// homed here (#3938): their receipt, then the message's deliver, which
    /// settles one expecting nothing and leaves one expecting a reply awaiting
    /// it. A message the person already took is acknowledged as it is.
    pub async fn acknowledge(&self, namespace: &str, record: &str) -> Result<Resource<Message>, String> {
        let (id, home) = self.person_message(namespace, record).await?;
        if home != self.home {
            return Err(format!("message `{record}` is homed at {home}; acknowledge it there"));
        }
        let message =
            self.inbox.get_async(&id).await.map_err(|error| error.to_string())?.ok_or_else(|| format!("message `{record}` is gone"))?;
        let person = self.register_person(self.world.now()).await?;
        match message.state() {
            MessageState::Pending => {}
            // Acknowledged before: answering it takes nothing more.
            taken if taken.receiver() == Some(&person) => return Ok(message),
            taken => return Err(format!("message `{record}` was already taken: {taken:?}")),
        }
        self.inbox.receive(&person, &id, Transport::Turn).await.map_err(|error| error.to_string())?;
        if let Err(error) = self.inbox.deliver(&id, Deliver { receiver: person.clone(), transport: Transport::Turn }).await {
            // The sweep delivers a committed receipt too (#4310); one that won the race to this
            // receipt has taken the message for the person already.
            let taken = self.inbox.get_async(&id).await.map_err(|error| error.to_string())?;
            if !taken.is_some_and(|message| delivered_to(message.state(), &person)) {
                return Err(error.to_string());
            }
        }
        let _ = self.attention_changes.send(());
        let _ = self.acknowledgements.send(());
        self.inbox.get_async(&id).await.map_err(|error| error.to_string())?.ok_or_else(|| format!("message `{record}` is gone"))
    }

    /// The fleet roles of `namespace`, using the designation's fleet identity.
    pub async fn roles(&self, namespace: &str) -> Result<FleetRoles, String> {
        self.roles_or_wait(namespace).await?.map_err(|wait| format!("the fleet roles are read from the FleetDesignation record: {wait}"))
    }

    /// The fleet roles, or the plain-words reason delivery waits when the FleetDesignation is missing or invalid.
    pub async fn roles_or_wait(&self, namespace: &str) -> Result<Result<FleetRoles, String>, String> {
        let roles = match designated_fleet_roles(&self.backend()?, namespace).await {
            Ok(roles) => roles,
            Err(ResourceError::NotFound { .. }) => {
                return Ok(Err(format!(
                    "FleetDesignation `{}` is missing; message delivery waits for it",
                    flotilla_resources::FLEET_DESIGNATION_NAME
                )))
            }
            Err(ResourceError::Invalid { message }) => {
                return Ok(Err(format!(
                    "FleetDesignation `{}` is invalid ({message}); message delivery waits for a valid one",
                    flotilla_resources::FLEET_DESIGNATION_NAME
                )))
            }
            Err(error) => return Err(format!("the fleet roles are read from the FleetDesignation record: {error}")),
        };
        Ok(Ok(roles))
    }

    /// Why delivery waits, when the FleetDesignation is missing or invalid.
    pub async fn delivery_wait(&self, namespace: &str) -> Result<Option<String>, String> {
        Ok(self.roles_or_wait(namespace).await?.err())
    }

    /// Admits an old-shaped intent at this home, its receiver's: maps it onto
    /// the engine spec and enqueues it as `<inbox>/message:<record name>`.
    /// A repeated name is the message already admitted.
    pub async fn admit(&self, namespace: &str, name: &str, intent: &OldSpec) -> Result<ResourceObject<OldMessage>, ResourceError> {
        Ok(self.admit_at(namespace, name, intent).await?.1)
    }

    /// [`Self::admit`], with the admitted message's engine address.
    pub(crate) async fn admit_at(
        &self,
        namespace: &str,
        name: &str,
        intent: &OldSpec,
    ) -> Result<(String, ResourceObject<OldMessage>), ResourceError> {
        // The producer's intent is still the old spec: it is held to that spec's rules first, as
        // the old store held it (an interrupt only from a supervisor, references holding the subject).
        <OldMessage as flotilla_resources::Resource>::validate_spec(
            &flotilla_resources::InputMeta::builder().name(name.to_owned()).build(),
            intent,
        )?;
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let mut records = OldStoreRecords::empty(namespace);
        let (sender, receiver) = self
            .spell(&mut records, |records| {
                let conversion = Conversion { roles: &roles, forges: &[], records };
                (conversion.sender_of(intent), conversion.receiver(intent))
            })
            .await?;
        // The messages it names, read from this replica, or at their home on a miss.
        let mut referenced = BTreeMap::new();
        // A reply's original was sent to this sender; a successor's predecessor to this receiver.
        for (old, inbox) in [(&intent.in_reply_to, sender), (&intent.supersedes, receiver)] {
            let Some(old) = old else { continue };
            let read = self.read_message(old, inbox.map_err(ResourceError::invalid)?).await.map_err(ResourceError::invalid)?;
            referenced.insert(old.clone(), read);
        }
        let spec = {
            let message = |old: &str| -> Result<MessageRef, String> {
                let read = referenced.get(old).ok_or_else(|| format!("message `{old}` was not read"))?;
                Ok(MessageRef::new(read.id.clone(), read.home.clone()))
            };
            self.spell(&mut records, |records| Conversion { roles: &roles, forges: records.forges(), records }.spec(intent, &message))
                .await?
                .map_err(ResourceError::invalid)?
        };
        // Admission's reads of the messages it names, at the versions read.
        self.world.update(|world| {
            for read in referenced.values() {
                let reference = flotilla_state_machines::machines::message::sent(&MessageRef::new(read.id.clone(), read.home.clone()));
                world.sent.put_at(&reference, (), flotilla_state_machines::Version(read.version));
            }
        });
        let supersedes = spec.supersedes.clone();
        let inbox_address = spec.inbox.clone();
        let in_reply_to = spec.in_reply_to.clone();
        let id = self.inbox.enqueue_as(spec, name).await.map_err(|error| ResourceError::invalid(error.to_string()))?;
        // Who acted is audit (#3938): a person sends as the role they hold, so the record names the
        // role and the admission trace names the principal as given, or that none was named.
        if let Some(principal) = intent.sender.strip_prefix("principal:") {
            let principal = if principal == flotilla_protocol::PrincipalRef::IMPLICIT_NAME { "none named" } else { principal };
            flotilla_tracing::queued_span(&id, "principal")
                .in_scope(|| tracing::info!(message.principal = principal, "message sent by a person"));
        }
        // A reply landing in its sender's inbox here answers the sender's record (#3587 layer 7).
        if in_reply_to.is_some() {
            let awaiting = Arc::clone(&self.awaiting);
            let reply = MessageRef::new(id.clone(), self.home.clone());
            let now = self.world.now();
            match tokio::task::spawn_blocking(move || awaiting.landed(&reply, &inbox_address, in_reply_to.as_ref(), now)).await {
                Ok(Ok(Some(record))) => tracing::debug!(%record, "a reply answered its awaiting record"),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => tracing::warn!(message = %name, %error, "answering the awaiting record failed"),
                Err(error) => tracing::warn!(message = %name, %error, "answering the awaiting record did not run"),
            }
        }
        if let Some(older) = supersedes.filter(|older| older.home == self.home) {
            // A successor that awaits a reply withdraws its predecessor's record once
            // its own is written (`await_reply`), so the sender never reads as not waiting.
            let awaits = !matches!(intent.expectation, flotilla_resources::MessageExpectation::None);
            self.supersede(&older.name, &id, !awaits).await;
        }
        let mut object = match self.view_at(namespace, &roles, id.clone(), self.home.clone()).await {
            Ok(Some(view)) => view.object,
            // The engine keeps no vessel; when no holder names it, answer with the producer's own intent.
            Ok(None) => admitted_intent(namespace, name, intent),
            Err(error) => {
                tracing::warn!(message = %name, %error, "admitted message has no old-shaped view; answering with its intent");
                admitted_intent(namespace, name, intent)
            }
        };
        // The home that admitted it: the sender records its awaiting reply against this (#3587 layer 7).
        object.metadata.annotations.insert(flotilla_store::replica::ORIGIN_ROOT_ANNOTATION.to_owned(), self.home.clone());
        Ok((id, object))
    }

    /// Records the sender's half of a send expecting a reply or an outcome
    /// (#3587 layer 7), once the message is admitted at `home`, its
    /// receiver's. The record is homed here, the sender's home, and named by
    /// the message. A reply that already landed in the sender's inbox here
    /// answers it at once. A send expecting nothing records nothing.
    pub async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &OldSpec,
        home: &str,
        kind: &str,
        about: Option<&str>,
    ) -> Result<bool, String> {
        use flotilla_resources::MessageExpectation;
        use flotilla_state_machines::machines::awaiting_reply::{Awaited, AwaitingReplySpec};
        let awaited = match intent.expectation {
            MessageExpectation::None => return Ok(false),
            MessageExpectation::Reply => Awaited::Reply,
            MessageExpectation::Outcome { .. } => Awaited::Outcome,
        };
        let roles = self.roles(namespace).await?;
        let (sender, recipient) = self
            .spell(&mut OldStoreRecords::empty(namespace), |records| {
                let conversion = Conversion { roles: &roles, forges: &[], records };
                (conversion.sender_of(intent), conversion.receiver(intent))
            })
            .await
            .map_err(|error| error.to_string())?;
        let sender = match sender {
            Ok(sender) => sender,
            // A system producer with no fleet role has no inbox to receive the
            // reply, so nothing awaits it; recording nothing is the answer.
            Err(_) if intent.sender.starts_with("system:") => return Ok(false),
            Err(error) => return Err(error),
        };
        let recipient = recipient?;
        let message = MessageRef::new(Address::message(&recipient, name).map_err(|error| error.to_string())?.as_str(), home);
        let conversion = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords };
        let subject = about.map(|about| conversion.stall_subject(about)).transpose()?;
        // The predecessor whose record this send's record replaces, wherever it is homed.
        let superseded = match &intent.supersedes {
            Some(older) => {
                let read = self.read_message(older, recipient.clone()).await?;
                Some(MessageRef::new(read.id, read.home))
            }
            None => None,
        };
        let spec = AwaitingReplySpec {
            message: message.clone(),
            sender: sender.clone(),
            recipient,
            subject,
            kind: kind.to_owned(),
            awaited,
            deadline: intent.deadline.as_ref().map(convert::timestamp),
        };
        let awaiting = Arc::clone(&self.awaiting);
        let service = Arc::clone(&self.service);
        let here = self.home.clone();
        let now = self.world.now();
        tokio::task::spawn_blocking(move || {
            // What this record replaces, withdrawn only once it is written, so the
            // seat or the sender never reads as not waiting in between. A repeat
            // withdraws again what an earlier attempt left open.
            let replace = |spec: &AwaitingReplySpec| -> Result<(), String> {
                if let (true, Some(subject)) = (spec.kind == awaiting::STALL, &spec.subject) {
                    let keep = awaiting::record_name(&spec.message);
                    awaiting.end_stalls(std::slice::from_ref(subject), Some(&keep), "replaced by a newer stall about the seat", now)?;
                }
                if let Some(older) = &superseded {
                    awaiting.withdraw(older, &spec.sender, &format!("superseded by {}", spec.message.name), now)?;
                }
                Ok(())
            };
            let recorded = awaiting.record(spec.clone(), now)?;
            replace(&spec)?;
            // A prior attempt may have persisted the record but failed before
            // reconciling its evidence. Retry while open; never reopen an ended record.
            if !awaiting.has_open_record(&message) {
                return Ok(recorded);
            }
            // A reply may have landed before the record: replies are indexed by what they answer.
            let replies = service
                .with_store(|store| {
                    let mut read = store.read()?;
                    let mut found = Vec::new();
                    let mut after = None;
                    loop {
                        let page =
                            read.page(&flotilla_state_machines::machines::message::MessageSpec::by_reply(&message), after.as_ref(), 64)?;
                        for id in page.ids {
                            if let Some(loaded) = read.load::<Resource<Message>>(id)? {
                                if loaded.home == here {
                                    found.push((MessageRef::new(loaded.name, loaded.home), loaded.value.spec().inbox.clone()));
                                }
                            }
                        }
                        after = page.next;
                        if after.is_none() {
                            return Ok(found);
                        }
                    }
                })
                .map_err(|error| error.to_string())?;
            // Each reply is its own evidence: one that fails does not stop the others.
            let failures: Vec<String> =
                replies.iter().filter_map(|(reply, inbox)| awaiting.landed(reply, inbox, Some(&message), now).err()).collect();
            match failures.is_empty() {
                true => Ok(recorded),
                false => Err(format!("recorded, but answering it from earlier replies failed: {}", failures.join("; "))),
            }
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// Withdraws this home's open stall records about crew role or convoy
    /// subjects no longer stalled. Subjects shared with `stalled` keep
    /// their records. With no open stall here it reads nothing.
    pub async fn end_stalls(&self, namespace: &str, seats: &[String], stalled: Option<&str>, reason: &str) -> Result<Vec<String>, String> {
        if seats.is_empty() || !self.awaiting.any_open_stall() {
            return Ok(Vec::new());
        }
        let roles = self.roles(namespace).await?;
        let conversion = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords };
        let kept = stalled.map(|seat| conversion.stall_subject(seat)).transpose()?;
        let mut subjects = Vec::new();
        for seat in seats {
            let subject = conversion.stall_subject(seat)?;
            if Some(&subject) != kept.as_ref() && !subjects.contains(&subject) {
                subjects.push(subject);
            }
        }
        let (awaiting, reason, now) = (Arc::clone(&self.awaiting), reason.to_owned(), self.world.now());
        tokio::task::spawn_blocking(move || awaiting.end_stalls(&subjects, None, &reason, now)).await.map_err(|error| error.to_string())?
    }

    /// The message `record` in `inbox`, in the old shape: from this replica by its exact
    /// identity, or read at its home on a miss and marked remote with its age.
    pub async fn read_view(
        &self,
        namespace: &str,
        record: &str,
        inbox: Address,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        if let Some(view) = self.view_in(namespace, &roles, Ok(inbox.clone()), record).await? {
            return Ok(Some(view));
        }
        let Some(reads) = self.home_reads.get() else { return Ok(None) };
        let id = Address::message(&inbox, record).map_err(|error| ResourceError::invalid(error.to_string()))?;
        let Some(read) = reads.find(Resource::<Message>::CODE, id.as_str()).await.map_err(ResourceError::other)? else { return Ok(None) };
        let Some(snapshot) = read.value else { return Ok(None) };
        let home = snapshot.home.clone();
        let created = snapshot.created_at.ok_or_else(|| ResourceError::other(format!("message {id} at {home} has no creation time")))?;
        let resource = snapshot.into_value::<Resource<Message>>().map_err(|error| ResourceError::decode(error.to_string()))?;
        tracing::info!(message = %id, %home, remote = read.remote, age_micros = read.age_micros, "replica miss read at the message's home");
        let object = self.view_of(created, &resource, namespace, &roles).await?.map_err(ResourceError::invalid)?;
        let synced = chrono::Utc::now() - chrono::Duration::microseconds(read.age_micros);
        Ok(Some(ReadResourceObject {
            object,
            provenance: ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new(home), last_synced_at: synced },
        }))
    }

    /// The message `record` in `inbox`, where it was sent, which names it at its home: from this
    /// replica by its exact identity, or, on a miss, read at its home through
    /// [`ports::HomeReads`] and marked remote with its age.
    pub async fn read_message(&self, record: &str, inbox: Address) -> Result<MessageRead, String> {
        self.find_message(record, inbox).await?.ok_or_else(|| format!("message `{record}` is at no reachable home"))
    }

    /// [`Self::read_message`], with None when every home answered that it
    /// holds no such message; an error when one could not be asked.
    async fn find_message(&self, record: &str, inbox: Address) -> Result<Option<MessageRead>, String> {
        if let Some((id, home, version)) = self.locate_in_versioned(&inbox, record)? {
            return Ok(Some(MessageRead { id, home, version, remote: false, age_micros: 0 }));
        }
        let Some(reads) = self.home_reads.get() else {
            return Err(format!("message `{record}` is not in this home's replica, and no home read can reach it"));
        };
        let id = Address::message(&inbox, record).map_err(|error| error.to_string())?.as_str().to_owned();
        let read = reads.find(Resource::<Message>::CODE, &id).await?;
        Ok(read.and_then(|read| read.value.map(|value| (value, read.remote, read.age_micros))).map(|(value, remote, age_micros)| {
            tracing::info!(message = %id, home = %value.home, remote, age_micros, "replica miss read at the message's home");
            MessageRead { id, home: value.home, version: value.version, remote, age_micros }
        }))
    }

    /// Where the message `intent` replies to stands, read as its admission
    /// reads it: in this replica, or at its home on a miss.
    pub async fn resolve_in_reply_to(&self, namespace: &str, intent: &OldSpec) -> MessageResolution {
        let Some(original) = &intent.in_reply_to else { return MessageResolution::Absent };
        let inbox = async {
            let roles = self.roles(namespace).await?;
            self.spell(&mut OldStoreRecords::empty(namespace), |records| {
                Conversion { roles: &roles, forges: &[], records }.sender_of(intent)
            })
            .await
            .map_err(|error| error.to_string())?
        };
        let found = match inbox.await {
            Ok(inbox) => self.find_message(original, inbox).await,
            Err(error) => Err(error),
        };
        match found {
            Ok(Some(_)) => MessageResolution::Found,
            Ok(None) => MessageResolution::Absent,
            Err(error) => MessageResolution::Unknown(error),
        }
    }

    /// Drops `older` as superseded by `newer`, both homed here, once its
    /// successor is admitted, and with `withdraw` its sender's record here.
    /// Already ended is no error.
    pub async fn supersede(&self, older: &str, newer: &str, withdraw: bool) {
        let Ok(by) = newer.parse::<Address>() else { return };
        self.world
            .update(|world| world.supersession.put(&flotilla_state_machines::Ref::new(newer), Supersedes { older: older.to_owned() }));
        if let Err(error) = self.inbox.drop(older, DropReason::Superseded { by: by.clone() }).await {
            // The sweep follows the successor in the log and retries.
            tracing::warn!(%older, %newer, %error, "superseded message did not drop yet");
            return;
        }
        // Its sender's record here, if it awaited a reply, waits no longer (#3587 layer 7).
        if withdraw {
            self.withdraw_awaiting(older, &by, &format!("superseded by {newer}")).await;
        }
    }

    /// Withdraws the awaiting record of a message dropped here, when its
    /// sender's record is homed here too.
    async fn withdraw_awaiting(&self, message: &str, by: &Address, reason: &str) {
        let awaiting = Arc::clone(&self.awaiting);
        let (message, by, reason, now) = (MessageRef::new(message, self.home.clone()), by.clone(), reason.to_owned(), self.world.now());
        match tokio::task::spawn_blocking(move || awaiting.withdraw(&message, &by, &reason, now)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "withdrawing the awaiting record failed"),
            Err(error) => tracing::warn!(%error, "withdrawing the awaiting record did not run"),
        }
    }

    /// An operator withdraws a queued message homed here: dropped as
    /// withdrawn by turn-rules on the convoy's authority, which the
    /// operator's command is.
    pub async fn withdraw(&self, namespace: &str, receiver: &str, record: &str, convoy: &str) -> Result<(), String> {
        let roles = self.roles(namespace).await?;
        let inbox = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords }.inbox(receiver)?;
        let (id, home) = self.locate_in(&inbox, record)?.ok_or_else(|| format!("message `{record}` is not in this home's replica"))?;
        if home != self.home {
            return Err(format!("message `{record}` is homed at {home}; withdraw it there"));
        }
        let by = roles.address("turn-rules").ok_or("turn-rules is not declared")?;
        // The convoy's own record names its project: one keyed read.
        let project = match self.backend()?.including_replicas::<Convoy>(namespace).get(convoy).await {
            Ok(read) => read.object.spec.project_ref,
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let authority = convoy_address(convoy, project.as_deref())?;
        self.world.update(|world| world.authority.put(&flotilla_state_machines::Ref::new(authority.as_str()), true));
        self.inbox.drop(&id, DropReason::Withdrawn { by: by.clone(), authority }).await.map_err(|error| error.to_string())?;
        self.withdraw_awaiting(&id, &by, "withdrawn by an operator").await;
        Ok(())
    }

    /// The exact message `record` under `inbox`, at whichever followed home holds it: its id and
    /// home.
    pub fn locate_in(&self, inbox: &Address, record: &str) -> Result<Option<(String, String)>, String> {
        Ok(self.locate_in_versioned(inbox, record)?.map(|(id, home, _)| (id, home)))
    }

    /// [`Self::locate_in`], with the version this replica holds.
    pub fn locate_in_versioned(&self, inbox: &Address, record: &str) -> Result<Option<(String, String, u64)>, String> {
        let id = Address::message(inbox, record).map_err(|error| error.to_string())?.as_str().to_owned();
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                for home in self.service.homes() {
                    let Some(stored) = read.resolve(Resource::<Message>::CODE, &id, &home)? else { continue };
                    if let Some((_, version)) = read.identity(stored)?.filter(|(_, version)| held(*version)) {
                        return Ok(Some((id.clone(), home, version)));
                    }
                }
                Ok(None)
            })
            .map_err(|error| error.to_string())
    }

    /// The messages of `inbox` in `label`, in admission order, at every followed home, with
    /// their homes: read through the inbox's index, loading only those rows.
    pub fn in_inbox(&self, inbox: &Address, label: MessageLabel) -> Result<Vec<(String, Resource<Message>)>, String> {
        let mut out = Vec::new();
        self.walk(&MessageSpec::by_inbox(inbox, &label), false, |home, message| {
            out.push((home, message));
            true
        })?;
        Ok(out)
    }

    /// The pending messages in `inbox`, in admission order, with their homes.
    pub fn pending_in(&self, inbox: &Address) -> Result<Vec<(String, Resource<Message>)>, String> {
        self.in_inbox(inbox, MessageLabel::Pending)
    }

    /// Every message in `inbox` in each state, a state at a time, each in admission order.
    pub fn inbox_messages(&self, inbox: &Address) -> Result<Vec<(String, Resource<Message>)>, String> {
        let mut all = Vec::new();
        for label in [MessageLabel::Pending, MessageLabel::Awaiting, MessageLabel::Settled, MessageLabel::Dropped] {
            all.extend(self.in_inbox(inbox, label)?);
        }
        Ok(all)
    }

    /// The open messages in `inbox`, Pending then Awaiting, each in admission order.
    pub fn open_in(&self, inbox: &Address) -> Result<Vec<(String, Resource<Message>)>, String> {
        let mut open = self.in_inbox(inbox, MessageLabel::Pending)?;
        open.extend(self.in_inbox(inbox, MessageLabel::Awaiting)?);
        Ok(open)
    }

    /// The last message of `inbox` admitted among those in `label`, read from the top of its
    /// index.
    pub fn newest_in(&self, inbox: &Address, label: MessageLabel) -> Result<Option<(String, Resource<Message>)>, String> {
        let mut newest = None;
        self.walk(&MessageSpec::by_inbox(inbox, &label), true, |home, message| {
            newest = Some((home, message));
            false
        })?;
        Ok(newest)
    }

    /// The message of `inbox` delivered last and not dropped since, read down its delivered
    /// index past any dropped after delivery.
    pub fn newest_delivered(&self, inbox: &Address) -> Result<Option<(String, Resource<Message>)>, String> {
        let mut newest = None;
        self.walk(&MessageSpec::delivered_by_inbox(inbox), true, |home, message| {
            if matches!(message.state(), MessageState::Dropped { .. }) {
                return true;
            }
            newest = Some((home, message));
            false
        })?;
        Ok(newest)
    }

    /// The messages of `inbox` settled at or after `since`, latest first, read down its settled
    /// index until one settled before.
    pub fn settled_since(&self, inbox: &Address, since: Timestamp) -> Result<Vec<(String, Resource<Message>)>, String> {
        let mut settled = Vec::new();
        self.walk(&MessageSpec::settled_by_inbox(inbox), true, |home, message| {
            if message.since() < since {
                return false;
            }
            settled.push((home, message));
            true
        })?;
        Ok(settled)
    }

    /// Walks `query`'s messages, ascending or from the top when `down`, handing each to `take`
    /// with its home until it answers false or the index ends. Each row it reaches is loaded and
    /// counted; none beyond.
    fn walk<K>(&self, query: &IndexQuery<K>, down: bool, mut take: impl FnMut(String, Resource<Message>) -> bool) -> Result<(), String> {
        const PAGE: u32 = 32;
        self.service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut cursor = None;
                loop {
                    let page = match down {
                        false => read.page(query, cursor.as_ref(), PAGE)?,
                        true => read.page_back(query, cursor.as_ref(), PAGE)?,
                    };
                    for id in page.ids {
                        self.count_loaded();
                        let Some(loaded) = read.load::<Resource<Message>>(id)? else { continue };
                        if !take(loaded.home, loaded.value) {
                            return Ok(());
                        }
                    }
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => return Ok(()),
                    }
                }
            })
            .map_err(|error| error.to_string())
    }

    fn provenance(&self, home: &str) -> ResourceProvenance {
        if home == self.home {
            ResourceProvenance::Local
        } else {
            let applied = self.service.watermark(home).map(|watermark| watermark.applied_through).unwrap_or(Timestamp(0));
            ResourceProvenance::Replica {
                origin_root: flotilla_protocol::NodeId::new(home),
                last_synced_at: applied.to_datetime().unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH),
            }
        }
    }

    /// Until when messages to `receiver`, an old crew address, hold its delivery: those admitted
    /// at this home and still Pending in the receiver's inbox each hold for one delivery hold
    /// from admission, and the latest of those ends is returned; None when none holds now. A
    /// turn still undelivered after its hold no longer holds its recipient; the stall judge
    /// reports a quiet crew.
    pub async fn delivery_held_until(&self, namespace: &str, receiver: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
        // Nothing is admitted until the fleet is designated.
        let Ok(roles) = self.roles_or_wait(namespace).await? else { return Ok(None) };
        let inbox = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords }.inbox(receiver)?;
        let hold = flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.num_microseconds().expect("a bounded hold");
        let now = self.world.now();
        Ok(self
            .pending_in(&inbox)?
            .iter()
            .map(|(_, message)| message.since().0.saturating_add(hold))
            .filter(|end| *end > now.0)
            .max()
            .and_then(|end| flotilla_schema::Timestamp(end).to_datetime()))
    }

    /// The generic resource verbs' listing (`flotilla get message`, through [`records`]): every
    /// message this home holds, its own and those it follows, as the record the engine stores,
    /// with no lookup ([`view::Stored`]). Walked one home's page at a time and converted page by
    /// page; the one Message read proportional to the table, so it serves only that verb.
    pub(crate) fn verb_listing(&self, namespace: &str) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        const PAGE: u32 = 64;
        let mut out = Vec::new();
        for home in self.service.homes() {
            let mut after = None;
            loop {
                let page = self
                    .service
                    .with_store(|store| {
                        let mut read = store.read()?;
                        let page = read.scan::<Resource<Message>>(&home, after, PAGE)?;
                        let mut loaded = Vec::with_capacity(page.ids.len());
                        for id in page.ids {
                            self.count_loaded();
                            if let Some(message) = read.load::<Resource<Message>>(id)? {
                                loaded.push((read.creation_time(id)?, message.value));
                            }
                        }
                        Ok((loaded, page.next))
                    })
                    .map_err(|error| ResourceError::other(error.to_string()))?;
                let (messages, next) = page;
                for (created, resource) in messages {
                    let object = view::stored_view(&resource, created, namespace).map_err(ResourceError::invalid)?;
                    out.push(ReadResourceObject { object, provenance: self.provenance(&home) });
                }
                match next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        Ok(out)
    }

    /// One message as [`Self::verb_listing`] shows it. Its full address is resolved by its exact
    /// identity at the followed homes; a bare record name names no inbox, so only the verb's
    /// listing can find it.
    pub(crate) fn verb_get(&self, namespace: &str, name: &str) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        if record_name(name).is_none() {
            return Ok(self.verb_listing(namespace)?.into_iter().find(|read| read.object.metadata.name == name));
        }
        for home in self.service.homes() {
            if let Some((home, created, resource)) = self.load_at(name.to_owned(), home)? {
                let object = view::stored_view(&resource, created, namespace).map_err(ResourceError::invalid)?;
                return Ok(Some(ReadResourceObject { object, provenance: self.provenance(&home) }));
            }
        }
        Ok(None)
    }

    /// The open messages to `receiver`, as [`Self::verb_listing`] shows them: Pending then Awaiting,
    /// read through the inbox's index. The listing spells each receiver as its inbox address, so a
    /// receiver that is no address has none.
    pub(crate) fn verb_open_to(&self, namespace: &str, receiver: &str) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let Ok(inbox) = receiver.parse::<Address>() else { return Ok(Vec::new()) };
        let rows = self.open_in(&inbox).map_err(ResourceError::other)?;
        let created = self.created_of(&rows)?;
        rows.into_iter()
            .zip(created)
            .map(|((home, resource), created)| {
                let object = view::stored_view(&resource, created, namespace).map_err(ResourceError::invalid)?;
                Ok(ReadResourceObject { object, provenance: self.provenance(&home) })
            })
            .collect()
    }

    /// The message `id` homed at `home`, with its home and when it was created.
    fn load_at(&self, id: String, home: String) -> Result<Option<(String, CreatedAt, Resource<Message>)>, ResourceError> {
        let resource = self
            .service
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(stored) = read.resolve(Resource::<Message>::CODE, &id, &home)? else { return Ok(None) };
                self.count_loaded();
                let Some(loaded) = read.load::<Resource<Message>>(stored)? else { return Ok(None) };
                Ok(Some((read.creation_time(stored)?, loaded.value)))
            })
            .map_err(|error| ResourceError::other(error.to_string()))?;
        Ok(resource.map(|(created, resource)| (home, created, resource)))
    }

    /// The message `id` homed at `home`, in the old shape.
    async fn view_at(
        &self,
        namespace: &str,
        roles: &FleetRoles,
        id: String,
        home: String,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        let Some((home, created, resource)) = self.load_at(id, home)? else { return Ok(None) };
        let object = self.view_of(created, &resource, namespace, roles).await?.map_err(ResourceError::invalid)?;
        Ok(Some(ReadResourceObject { object, provenance: self.provenance(&home) }))
    }

    /// The message `record` in `receiver`'s inbox (an old address), read by its exact identity.
    pub async fn view_to(
        &self,
        namespace: &str,
        receiver: &str,
        record: &str,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let inbox = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords }.inbox(receiver);
        self.view_in(namespace, &roles, inbox, record).await
    }

    /// The message a person names by its old record name, in the old shape: read by its exact
    /// identity in each person role's inbox, refused when more than one holds the name.
    pub async fn view_for_person(&self, namespace: &str, record: &str) -> Result<ReadResourceObject<OldMessage>, String> {
        let (id, home) = self.person_message(namespace, record).await?;
        let roles = self.roles(namespace).await?;
        self.view_at(namespace, &roles, id, home)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("message `{record}` is not in this home's replica"))
    }

    /// The message `record` in the inbox `intent` is admitted to, read by its exact identity.
    pub async fn view_for(
        &self,
        namespace: &str,
        intent: &OldSpec,
        record: &str,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let inbox = self
            .spell(&mut OldStoreRecords::empty(namespace), |records| Conversion { roles: &roles, forges: &[], records }.receiver(intent))
            .await?;
        self.view_in(namespace, &roles, inbox, record).await
    }

    async fn view_in(
        &self,
        namespace: &str,
        roles: &FleetRoles,
        inbox: Result<Address, String>,
        record: &str,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        // No message is admitted to a receiver that converts to no inbox.
        let inbox = match inbox {
            Ok(inbox) => inbox,
            Err(error) => {
                tracing::debug!(%record, %error, "a receiver with no inbox holds no message");
                return Ok(None);
            }
        };
        let Some((id, home)) = self.locate_in(&inbox, record).map_err(ResourceError::other)? else { return Ok(None) };
        self.view_at(namespace, roles, id, home).await
    }

    /// The open messages to `receivers` (old addresses) in the old shape, read through each
    /// inbox's index. A message the records it names cannot spell is left out.
    pub async fn open_to(&self, namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let (roles, inboxes) = self.inboxes_of(namespace, receivers).await?;
        let mut rows = Vec::new();
        for inbox in &inboxes {
            rows.extend(self.open_in(inbox).map_err(ResourceError::other)?);
        }
        self.spelled_to(rows, namespace, &roles, receivers).await
    }

    /// What the stall judge reads of crew seats' inboxes, in the old shape: for each of
    /// `receivers` (old crew addresses), its open messages, the last one admitted among those
    /// settled, and the one delivered last and not dropped since; then each `named` message,
    /// a (receiver, record) pair, by its exact identity. Each is read through an index or by
    /// key, so the read grows with the seats, not the inboxes' history. A message the records
    /// it names cannot spell is left out.
    pub async fn seat_messages(
        &self,
        namespace: &str,
        receivers: &[String],
        named: &[(String, String)],
    ) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let (roles, inboxes) = self.inboxes_of(namespace, receivers).await?;
        let conversion = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords };
        let mut rows = BTreeMap::new();
        let mut keep = |(home, message): (String, Resource<Message>)| {
            rows.entry(message.name().to_owned()).or_insert((home, message));
        };
        for inbox in &inboxes {
            self.open_in(inbox).map_err(ResourceError::other)?.into_iter().for_each(&mut keep);
            self.newest_in(inbox, MessageLabel::Settled).map_err(ResourceError::other)?.into_iter().for_each(&mut keep);
            self.newest_delivered(inbox).map_err(ResourceError::other)?.into_iter().for_each(&mut keep);
        }
        for (receiver, record) in named {
            let inbox = match conversion.inbox(receiver) {
                Ok(inbox) => inbox,
                Err(error) => {
                    tracing::debug!(%receiver, %record, %error, "a receiver with no inbox holds no message");
                    continue;
                }
            };
            if let Some((id, home)) = self.locate_in(&inbox, record).map_err(ResourceError::other)? {
                self.load_at(id, home)?.map(|(home, _, message)| (home, message)).into_iter().for_each(&mut keep);
            }
        }
        let receivers: Vec<String> = receivers.iter().cloned().chain(named.iter().map(|(receiver, _)| receiver.clone())).collect();
        self.spelled_to(rows.into_values().collect(), namespace, &roles, &receivers).await
    }

    /// The pending messages to `receivers` (old addresses) in the old shape, read through each
    /// inbox's pending index. A message the records it names cannot spell is left out.
    pub async fn pending_to(&self, namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let (roles, inboxes) = self.inboxes_of(namespace, receivers).await?;
        let mut rows = Vec::new();
        for inbox in &inboxes {
            rows.extend(self.pending_in(inbox).map_err(ResourceError::other)?);
        }
        self.spelled_to(rows, namespace, &roles, receivers).await
    }

    /// The inboxes old receivers' messages are in. A person's role depends on the message's
    /// context, so a principal is read in every person role's inbox.
    async fn inboxes_of(&self, namespace: &str, receivers: &[String]) -> Result<(FleetRoles, BTreeSet<Address>), ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let conversion = Conversion { roles: &roles, forges: &[], records: &convert::NoRecords };
        let mut inboxes = BTreeSet::new();
        for receiver in receivers {
            if receiver.starts_with("principal:") {
                inboxes.extend(self.person_inboxes(namespace).await.map_err(ResourceError::other)?);
                continue;
            }
            match conversion.inbox(receiver) {
                Ok(inbox) => {
                    inboxes.insert(inbox);
                }
                Err(error) => tracing::debug!(%receiver, %error, "a receiver with no inbox holds no message"),
            }
        }
        Ok((roles, inboxes))
    }

    /// `rows` in the old shape, those spelled to one of `receivers`: a convoy role's inbox is
    /// spelled with the vessel holding it. The records read are those the rows name.
    async fn spelled_to(
        &self,
        rows: Vec<(String, Resource<Message>)>,
        namespace: &str,
        roles: &FleetRoles,
        receivers: &[String],
    ) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let created = self.created_of(&rows)?;
        let messages: Vec<(CreatedAt, Resource<Message>)> =
            created.into_iter().zip(&rows).map(|(created, (_, resource))| (created, resource.clone())).collect();
        let views = self.old_views(namespace, roles, &messages).await?;
        Ok(rows
            .into_iter()
            .zip(views)
            .filter_map(|((home, resource), view)| match view {
                Ok(object) if receivers.contains(&object.spec.receiver) => {
                    Some(ReadResourceObject { object, provenance: self.provenance(&home) })
                }
                Ok(_) => None,
                Err(error) => {
                    tracing::debug!(message = resource.name(), %error, "engine message has no old-shaped view");
                    None
                }
            })
            .collect())
    }

    /// An intent with its sender and receiver as [`Self::view`] spells them once
    /// admitted: each party converted as admission converts it, then spelled
    /// back, on the records the parties name.
    pub async fn spelled(&self, namespace: &str, intent: &OldSpec) -> Result<OldSpec, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        self.spell(&mut OldStoreRecords::empty(namespace), |records| -> Result<OldSpec, String> {
            let conversion = Conversion { roles: &roles, forges: &[], records };
            let sender = view::old_address(&conversion.sender_of(intent)?, &roles, records)?;
            let receiver = view::old_address(&conversion.receiver(intent)?, &roles, records)?;
            Ok(OldSpec { sender, receiver, ..intent.clone() })
        })
        .await?
        .map_err(ResourceError::invalid)
    }

    /// A message created at `created` in the old shape, on the records it names, read now.
    async fn view_of(
        &self,
        created: CreatedAt,
        resource: &Resource<Message>,
        namespace: &str,
        roles: &FleetRoles,
    ) -> Result<Result<ResourceObject<OldMessage>, String>, ResourceError> {
        Ok(self.old_views(namespace, roles, &[(created, resource.clone())]).await?.pop().expect("one view per message"))
    }
}

/// A message an admission read: its id, its home, the version read, and,
/// for a read routed to its home, that it was remote and how old it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRead {
    pub id: String,
    pub home: String,
    pub version: u64,
    pub remote: bool,
    pub age_micros: i64,
}

/// An admitted intent in the old shape, waiting for its consumer.
fn admitted_intent(namespace: &str, name: &str, intent: &OldSpec) -> ResourceObject<OldMessage> {
    let now = chrono::Utc::now();
    ResourceObject {
        metadata: flotilla_resources::ObjectMeta {
            name: name.to_owned(),
            namespace: namespace.to_owned(),
            resource_version: String::new(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: now,
            merge: None,
        },
        spec: intent.clone(),
        status: Some(flotilla_resources::MessageStatus {
            phase: flotilla_resources::MessagePhase::Deliverable,
            since: now,
            reason: Some("waiting for its consumer".into()),
            ..Default::default()
        }),
    }
}

/// Crew code's port onto this path (`flotilla_messaging::EngineMessages`).
#[async_trait::async_trait]
impl flotilla_messaging::EngineMessages for MessagePrimitives {
    async fn view_to(
        &self,
        namespace: &str,
        receiver: &str,
        record: &str,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::view_to(self, namespace, receiver, record).await
    }
    async fn view_for(
        &self,
        namespace: &str,
        intent: &OldSpec,
        record: &str,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::view_for(self, namespace, intent, record).await
    }
    async fn open_to(&self, namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::open_to(self, namespace, receivers).await
    }
    async fn seat_messages(
        &self,
        namespace: &str,
        receivers: &[String],
        named: &[(String, String)],
    ) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::seat_messages(self, namespace, receivers, named).await
    }
    async fn pending_to(&self, namespace: &str, receivers: &[String]) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::pending_to(self, namespace, receivers).await
    }
    async fn admit(&self, namespace: &str, name: &str, intent: &OldSpec) -> Result<ResourceObject<OldMessage>, ResourceError> {
        MessagePrimitives::admit(self, namespace, name, intent).await
    }
    async fn spelled(&self, namespace: &str, intent: &OldSpec) -> Result<OldSpec, ResourceError> {
        MessagePrimitives::spelled(self, namespace, intent).await
    }
    async fn resolve_in_reply_to(&self, namespace: &str, intent: &OldSpec) -> MessageResolution {
        MessagePrimitives::resolve_in_reply_to(self, namespace, intent).await
    }
    async fn withdraw(&self, namespace: &str, receiver: &str, record: &str, convoy: &str) -> Result<(), String> {
        MessagePrimitives::withdraw(self, namespace, receiver, record, convoy).await
    }
    fn home(&self) -> String {
        self.home.clone()
    }
    async fn await_reply(
        &self,
        namespace: &str,
        name: &str,
        intent: &OldSpec,
        home: &str,
        kind: &str,
        about: Option<&str>,
    ) -> Result<bool, String> {
        MessagePrimitives::await_reply(self, namespace, name, intent, home, kind, about).await
    }
    async fn end_stalls(&self, namespace: &str, seats: &[String], stalled: Option<&str>, reason: &str) -> Result<Vec<String>, String> {
        MessagePrimitives::end_stalls(self, namespace, seats, stalled, reason).await
    }
    async fn delivery_held_until(&self, namespace: &str, receiver: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
        MessagePrimitives::delivery_held_until(self, namespace, receiver).await
    }
    async fn delivery_wait(&self, namespace: &str) -> Result<Option<String>, String> {
        MessagePrimitives::delivery_wait(self, namespace).await
    }
    fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, ResourceError> {
        MessagePrimitives::changes(self)
    }
    fn charter_notices(self: Arc<Self>, namespace: &str) -> Box<dyn flotilla_messaging::CharterNotices> {
        Box::new(EngineCharterNotices { primitives: self, namespace: namespace.to_owned() })
    }
}

/// Charter notifications admitted on the engine path, in one namespace.
pub struct EngineCharterNotices {
    pub primitives: Arc<MessagePrimitives>,
    pub namespace: String,
}

#[async_trait::async_trait]
impl flotilla_messaging::CharterNotices for EngineCharterNotices {
    fn namespace(&self) -> &str {
        &self.namespace
    }
    fn pass(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.primitives.charter_pass)
    }
    fn admission(&self) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(&self.primitives.charter_admission)
    }
    async fn existing(&self, receiver: &str, name: &str, _: chrono::DateTime<chrono::Utc>) -> Result<bool, ResourceError> {
        // The engine admits in the create: a stored message is admitted. Nothing is admitted
        // before the fleet is designated.
        let Ok(roles) = self.primitives.roles_or_wait(&self.namespace).await.map_err(ResourceError::other)? else { return Ok(false) };
        let inbox =
            Conversion { roles: &roles, forges: &[], records: &convert::NoRecords }.inbox(receiver).map_err(ResourceError::invalid)?;
        Ok(self.primitives.locate_in(&inbox, name).map_err(ResourceError::other)?.is_some())
    }
    async fn open_to(&self, receiver: &str) -> Result<Vec<ResourceObject<OldMessage>>, ResourceError> {
        Ok(self.primitives.open_to(&self.namespace, &[receiver.to_owned()]).await?.into_iter().map(|view| view.object).collect())
    }
    async fn admit(&self, name: &str, spec: &OldSpec, _: chrono::DateTime<chrono::Utc>) -> Result<(), ResourceError> {
        self.primitives.admit(&self.namespace, name, spec).await.map(|_| ())
    }
}

/// What spelling asked of the old-store records: each record by its key, and the two lookups no
/// key reaches, a repository by its address and a session by its convoy role's labels.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wanted {
    forges: bool,
    every_repository: bool,
    convoys: BTreeSet<String>,
    terminals: BTreeSet<String>,
    artifacts: BTreeSet<String>,
    repositories: BTreeSet<String>,
    projects: BTreeSet<String>,
    /// (convoy, role): the sessions holding a convoy role, read through its vessel.
    seats: BTreeSet<(String, String)>,
    /// (convoy, role): the sessions labelled with a convoy role whose convoy names no vessel for it.
    labelled: BTreeSet<(String, String)>,
}

impl Wanted {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn absorb(&mut self, other: Wanted) {
        self.forges |= other.forges;
        self.every_repository |= other.every_repository;
        self.convoys.extend(other.convoys);
        self.terminals.extend(other.terminals);
        self.artifacts.extend(other.artifacts);
        self.repositories.extend(other.repositories);
        self.projects.extend(other.projects);
        self.seats.extend(other.seats);
        self.labelled.extend(other.labelled);
    }
}

/// `read` with a missing record as `None`.
fn found<T>(read: Result<T, ResourceError>) -> Result<Option<T>, ResourceError> {
    match read {
        Ok(read) => Ok(Some(read)),
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The work holding `role` in `convoy`'s workflow snapshot.
fn snapshot_vessel(convoy: &ResourceObject<Convoy>, role: &str) -> Option<String> {
    let status = convoy.status.as_ref()?;
    status
        .workflow_snapshot
        .as_ref()?
        .vessels
        .iter()
        .find(|vessel| vessel.crew.iter().any(|crew| crew.role == role))
        .map(|vessel| vessel.name.clone())
}

/// The old-store facts the new path reads, through one named port (#3317
/// ruling 5): convoys (a role's vessel), crew sessions (a receipt's
/// session), and the records a reference names. When Convoy and the
/// session kinds move, this is the one implementation that changes.
///
/// Spelling reads them by key ([`MessagePrimitives::spell`]): a lookup that misses a record not
/// yet read asks for it, and the spelling runs again once it is read.
pub struct OldStoreRecords {
    pub namespace: String,
    /// The Forge catalog as read; [`Self::forges`] asks for it when it has not been.
    pub forges: Vec<ForgeSpec>,
    convoys: BTreeMap<String, ResourceObject<Convoy>>,
    /// Each held convoy's home: this home, or the home its copy was replicated from.
    convoy_homes: BTreeMap<String, String>,
    /// Convoys any read copy of which has not ended; `convoys` keeps one copy per name.
    live_convoys: BTreeSet<String>,
    terminals: BTreeMap<String, ResourceObject<TerminalSession>>,
    artifacts: BTreeMap<String, ResourceObject<Artifact>>,
    repositories: BTreeMap<String, ResourceObject<Repository>>,
    projects: BTreeMap<String, ResourceObject<Project>>,
    /// What has been read, found or not: a lookup asks only for what is beyond it.
    read: Wanted,
    /// What lookups asked for since the last read.
    asked: Mutex<Wanted>,
}

impl OldStoreRecords {
    /// No records read yet: spelling reads what it asks for.
    pub fn empty(namespace: &str) -> Self {
        Self::from_records(namespace, Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
    }

    /// What lookups asked for since the last read, beyond what was read.
    fn take_asked(&self) -> Wanted {
        std::mem::take(&mut *self.asked.lock().expect("asked records"))
    }

    fn ask(&self, edit: impl FnOnce(&mut Wanted)) {
        edit(&mut self.asked.lock().expect("asked records"));
    }

    /// The Forge catalog, asked for when it has not been read.
    pub fn forges(&self) -> &[ForgeSpec] {
        if !self.read.forges {
            self.ask(|asked| asked.forges = true);
        }
        &self.forges
    }

    pub(crate) fn convoy(&self, name: &str) -> Option<&ResourceObject<Convoy>> {
        let held = self.convoys.get(name);
        if held.is_none() && !self.read.convoys.contains(name) {
            self.ask(|asked| {
                asked.convoys.insert(name.to_owned());
            });
        }
        held
    }

    fn terminal(&self, name: &str) -> Option<&ResourceObject<TerminalSession>> {
        let held = self.terminals.get(name);
        if held.is_none() && !self.read.terminals.contains(name) {
            self.ask(|asked| {
                asked.terminals.insert(name.to_owned());
            });
        }
        held
    }

    fn artifact(&self, name: &str) -> Option<&ResourceObject<Artifact>> {
        let held = self.artifacts.get(name);
        if held.is_none() && !self.read.artifacts.contains(name) {
            self.ask(|asked| {
                asked.artifacts.insert(name.to_owned());
            });
        }
        held
    }

    fn repository_named(&self, name: &str) -> Option<&ResourceObject<Repository>> {
        let held = self.repositories.get(name);
        if held.is_none() && !self.read.repositories.contains(name) && !self.read.every_repository {
            self.ask(|asked| {
                asked.repositories.insert(name.to_owned());
            });
        }
        held
    }

    fn project(&self, name: &str) -> Option<&ResourceObject<Project>> {
        let held = self.projects.get(name);
        if held.is_none() && !self.read.projects.contains(name) {
            self.ask(|asked| {
                asked.projects.insert(name.to_owned());
            });
        }
        held
    }
    /// The port over records already read, as a validation rehearsal holds them.
    pub fn from_records(
        namespace: &str,
        forges: Vec<ForgeSpec>,
        convoys: Vec<ResourceObject<Convoy>>,
        terminals: Vec<ResourceObject<TerminalSession>>,
        artifacts: Vec<ResourceObject<Artifact>>,
        repositories: Vec<ResourceObject<Repository>>,
        projects: Vec<ResourceObject<Project>>,
    ) -> Self {
        fn by_name<R: flotilla_resources::Resource>(items: Vec<ResourceObject<R>>) -> BTreeMap<String, ResourceObject<R>> {
            items.into_iter().map(|item| (item.metadata.name.clone(), item)).collect()
        }
        Self {
            namespace: namespace.to_owned(),
            forges,
            live_convoys: live(&convoys),
            convoys: by_name(convoys),
            convoy_homes: BTreeMap::new(),
            terminals: by_name(terminals),
            artifacts: by_name(artifacts),
            repositories: by_name(repositories),
            projects: by_name(projects),
            read: Wanted::default(),
            asked: Mutex::default(),
        }
    }

    /// Every record of each kind the port reads, for the role-routing and condition ports.
    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let forges =
            backend.definitions::<flotilla_resources::Forge>(namespace).list().await?.into_iter().map(|forge| forge.spec).collect();
        fn objects<R: flotilla_resources::Resource>(items: Vec<ReadResourceObject<R>>) -> Vec<ResourceObject<R>> {
            items.into_iter().map(|item| item.object).collect()
        }
        Ok(Self::from_records(
            namespace,
            forges,
            objects(backend.including_replicas::<Convoy>(namespace).list().await?.items),
            objects(backend.including_replicas::<TerminalSession>(namespace).list().await?.items),
            objects(backend.including_replicas::<Artifact>(namespace).list().await?.items),
            objects(backend.including_replicas::<Repository>(namespace).list().await?.items),
            backend.definitions::<Project>(namespace).list().await?,
        ))
    }

    /// Every project declared in this namespace, by name.
    pub fn projects(&self) -> impl Iterator<Item = &str> {
        self.projects.keys().map(String::as_str)
    }

    /// The roles `project`'s own charter declares a principal holds, with
    /// that principal. Declarations stay local: an inherited role shape
    /// names no holder in a child project.
    pub fn principal_backed(&self, project: &str) -> impl Iterator<Item = (&str, &str)> {
        self.project(project).into_iter().flat_map(|project| {
            project.spec.role_definitions.iter().filter_map(|(role, definition)| Some((role.as_str(), definition.principal.as_deref()?)))
        })
    }

    /// The project a convoy belongs to, if it declares one.
    pub fn convoy_project(&self, name: &str) -> Option<String> {
        self.convoy(name)?.spec.project_ref.clone()
    }

    fn convoy_address(&self, name: &str) -> Result<Address, String> {
        convoy_address(name, self.convoy(name).and_then(|convoy| convoy.spec.project_ref.as_deref()))
    }

    /// A crew session's seat, the address its convoy role's messages are received at (the crew's
    /// receiver spelling, under the convoy's project or else the namespace): a capability card is
    /// about the session's seat.
    fn seat(&self, session: &str) -> Result<Address, String> {
        let terminal = self.terminal(session).ok_or_else(|| format!("session `{session}` was not read"))?;
        let TerminalSessionSource::Agent { context, .. } = &terminal.spec.source else {
            return Err(format!("session `{session}` is no crew session"));
        };
        let project = self.convoy_project(&context.convoy).unwrap_or_else(|| self.namespace.clone());
        Address::project(&project)
            .and_then(|project| project.child(AddressKind::Convoy, &context.convoy))
            .and_then(|convoy| convoy.child(AddressKind::Role, &terminal.spec.role))
            .map_err(|error| format!("session `{session}`'s seat: {error}"))
    }

    fn repository(&self, name: &str) -> Result<Address, String> {
        let forges = self.forges();
        let repository = self.repository_named(name).ok_or_else(|| format!("repository `{name}` is not declared"))?;
        let remote = repository.spec.remotes().first().ok_or_else(|| format!("repository `{name}` has no remote"))?;
        repository_address(remote, forges)
    }

    /// The repository whose first remote is at `address`: no key reaches it, so every
    /// repository is read once a spelling asks.
    fn repository_at(&self, address: &Address) -> Option<&ResourceObject<Repository>> {
        if !self.read.every_repository {
            self.ask(|asked| asked.every_repository = true);
        }
        let forges = self.forges();
        self.repositories.values().find(|repository| {
            repository.spec.remotes().first().is_some_and(|remote| repository_address(remote, forges).as_ref() == Ok(address))
        })
    }

    /// The session holding `role` in `convoy`, among the sessions read; asked for through the
    /// convoy's vessel when not read.
    fn seat_session(&self, convoy: &str, role: &str, seat: &Address) -> Option<&str> {
        let held = self.terminals.keys().find(|session| self.seat(session).as_ref() == Ok(seat)).map(String::as_str);
        let key = (convoy.to_owned(), role.to_owned());
        if held.is_none() && !self.read.seats.contains(&key) {
            self.ask(|asked| {
                asked.seats.insert(key);
            });
        }
        held
    }
}

/// Convoy `name`'s address: under `project` when it names one.
pub(crate) fn convoy_address(name: &str, project: Option<&str>) -> Result<Address, String> {
    let address = match project {
        Some(project) => Address::project(project).and_then(|project| project.child(AddressKind::Convoy, name)),
        None => Address::convoy(name),
    };
    address.map_err(|error| format!("convoy `{name}`: {error}"))
}

/// A convoy with no status has not been seen to end.
fn live(convoys: &[ResourceObject<Convoy>]) -> BTreeSet<String> {
    convoys
        .iter()
        .filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()))
        .map(|convoy| convoy.metadata.name.clone())
        .collect()
}

impl ReferenceAddresses for OldStoreRecords {
    fn missing(&self, reference: &OldReference) -> Option<&'static str> {
        match reference {
            OldReference::ControlRecord { resource, .. } if matches!(resource.kind.as_str(), "Convoy" | "Project") => None,
            OldReference::ControlRecord { resource, .. } if resource.kind == "TerminalSession" => {
                self.terminal(&resource.name).is_none().then_some("a referenced session is gone")
            }
            OldReference::ControlRecord { .. } => Some("it references a kind with no address"),
            OldReference::Artifact { resource, .. } => self.artifact(&resource.name).is_none().then_some("a referenced artifact is gone"),
            OldReference::Commit { repository, .. } | OldReference::Ref { repository, .. } => {
                self.repository_named(&repository.name).is_none().then_some("a referenced repository is gone")
            }
            _ => None,
        }
    }
    fn convoy_project(&self, convoy: &str) -> Option<String> {
        OldStoreRecords::convoy_project(self, convoy)
    }
    fn convoy_ended(&self, convoy: &str) -> bool {
        self.convoy(convoy);
        !self.live_convoys.contains(convoy)
    }
    fn principal_roles(&self, project: &str, principal: &str) -> Vec<String> {
        self.principal_backed(project).filter(|(_, holder)| *holder == principal).map(|(role, _)| role.to_owned()).collect()
    }
    fn address(&self, reference: &OldReference) -> Result<Address, String> {
        match reference {
            OldReference::ControlRecord { resource, .. } if resource.kind == "Convoy" => self.convoy_address(&resource.name),
            OldReference::ControlRecord { resource, .. } if resource.kind == "Project" => {
                Address::project(&resource.name).map_err(|error| format!("project `{}`: {error}", resource.name))
            }
            OldReference::ControlRecord { resource, .. } if resource.kind == "TerminalSession" => self.seat(&resource.name),
            OldReference::Artifact { resource, .. } => {
                let artifact = self.artifact(&resource.name).ok_or_else(|| format!("artifact `{}` was not read", resource.name))?;
                artifact_address(&artifact.spec.convoy, &artifact.spec.producer, &artifact.spec.kind, &artifact.spec.subject)
            }
            OldReference::Commit { repository, revision } => {
                self.repository(&repository.name)?.child(AddressKind::Commit, revision).map_err(|error| error.to_string())
            }
            OldReference::Ref { repository, name, .. } => {
                self.repository(&repository.name)?.child(AddressKind::Ref, name).map_err(|error| error.to_string())
            }
            other => Err(format!("reference {other:?} names a record with no address")),
        }
    }
}

impl ViewLookups for OldStoreRecords {
    fn principal_of(&self, project: &str, role: &str) -> Option<String> {
        self.principal_backed(project).find(|(declared, _)| *declared == role).map(|(_, principal)| principal.to_owned())
    }
    /// From the convoy's snapshot; where it names no vessel for the role (the convoy is not held
    /// here, or its snapshot lacks the role), from the labels of a session holding the role.
    fn vessel(&self, convoy: &str, role: &str) -> Option<String> {
        if let Some(vessel) = self.convoy(convoy).and_then(|held| snapshot_vessel(held, role)) {
            return Some(vessel);
        }
        let labelled = self.terminals.values().find_map(|terminal| {
            let labels = &terminal.metadata.labels;
            (labels.get(flotilla_resources::CONVOY_LABEL).map(String::as_str) == Some(convoy)
                && labels.get(flotilla_resources::ROLE_LABEL).map(String::as_str) == Some(role))
            .then(|| labels.get(flotilla_resources::VESSEL_LABEL).cloned())
            .flatten()
        });
        let key = (convoy.to_owned(), role.to_owned());
        if labelled.is_none() && self.read.convoys.contains(convoy) && !self.read.labelled.contains(&key) {
            self.ask(|asked| {
                asked.labelled.insert(key);
            });
        }
        labelled
    }
    /// A receiver names its launch, read from the session's view only while it shows that launch.
    fn session(&self, receiver: &Receiver) -> Option<(String, String)> {
        let terminal = self.terminal(flotilla_resources::terminal_records::launched(&receiver.id))?;
        if flotilla_resources::terminal_records::launch_of(terminal) != Some(receiver.id.as_str()) {
            return None;
        }
        let status = terminal.status.as_ref()?;
        Some((status.crew.as_ref()?.id.clone(), status.session_id.clone()?))
    }
    fn record(&self, address: &Address, revision: &str) -> Option<OldReference> {
        let reference =
            |kind: &str, name: &str, namespace: &str| flotilla_protocol::ResourceRef::new("flotilla.work/v1", kind, namespace, name);
        let namespace = self.namespace.as_str();
        match address.kind() {
            AddressKind::Convoy => Some(OldReference::ControlRecord {
                resource: reference("Convoy", address.name(), namespace),
                revision: revision.to_owned(),
            }),
            AddressKind::Project => Some(OldReference::ControlRecord {
                resource: reference("Project", address.name(), namespace),
                revision: revision.to_owned(),
            }),
            AddressKind::Artifact => {
                let name = address_record_name(address)?;
                Some(OldReference::Artifact { resource: reference("Artifact", &name, namespace), revision: revision.to_owned() })
            }
            AddressKind::Commit | AddressKind::Ref => {
                let repository = self.repository_at(&address.parent()?)?;
                let repository = reference("Repository", &repository.metadata.name, namespace);
                Some(match address.kind() {
                    AddressKind::Commit => OldReference::Commit { repository, revision: revision.to_owned() },
                    _ => OldReference::Ref { repository, name: address.name().to_owned(), revision: revision.to_owned() },
                })
            }
            // A crew session's seat: the session holding it, the old spelling's subject. It goes
            // when crew compares engine addresses (#3108 gate task).
            AddressKind::Role if address.get(AddressKind::Convoy).is_some() => {
                let session = self.seat_session(address.get(AddressKind::Convoy).expect("checked"), address.name(), address)?;
                Some(OldReference::ControlRecord {
                    resource: reference("TerminalSession", session, namespace),
                    revision: revision.to_owned(),
                })
            }
            _ => None,
        }
    }
}

/// Whether `state` is a message delivered to `receiver`: awaiting or settled with it as receiver.
/// A receipt alone leaves a message pending with no receiver, and one dropped is not delivered.
fn delivered_to(state: &MessageState, receiver: &Receiver) -> bool {
    matches!(state, MessageState::Awaiting { receiver: delivered, .. } | MessageState::Settled { receiver: Some(delivered), .. } if delivered == receiver)
}

/// Whether a message identity at `version` is held here. Version 0 is a placeholder another
/// record's reference interned (a reply's `in_reply_to`): no event about the message has arrived.
fn held(version: u64) -> bool {
    version > 0
}

/// The composition root's slot for the new path: set once, at startup,
/// when `[primitives]` is on; empty otherwise, and nothing changes.
pub type MessagePath = Arc<std::sync::OnceLock<Arc<MessagePrimitives>>>;

#[cfg(test)]
mod tests {
    use flotilla_state_machines::machines::{consumer::ConsumerKind, message::DropReason};

    use super::*;

    // #4310: an acknowledgement whose own deliver was refused is taken only when the message was
    // delivered to the person; any other refusal stays an error.
    #[test]
    fn only_a_message_delivered_to_the_person_counts_as_taken() {
        let person = Receiver { kind: ConsumerKind::Person, id: "person".into(), host: "home".into() };
        let other = Receiver { kind: ConsumerKind::AgentSession, id: "session".into(), host: "home".into() };
        let by: Address = "project:standing/role:operator".parse().expect("address");
        let at = Timestamp(1);
        assert!(delivered_to(&MessageState::Settled { receiver: Some(person.clone()), delivered_at: Some(at) }, &person));
        assert!(delivered_to(&MessageState::Awaiting { receiver: person.clone(), delivered_at: at }, &person));
        assert!(!delivered_to(&MessageState::Pending, &person), "a receipt alone delivers nothing");
        assert!(!delivered_to(&MessageState::Settled { receiver: Some(other.clone()), delivered_at: Some(at) }, &person));
        assert!(!delivered_to(&MessageState::Settled { receiver: None, delivered_at: None }, &person));
        let withdrawn = DropReason::Withdrawn { by: by.clone(), authority: by };
        let dropped = MessageState::Dropped { reason: withdrawn, receiver: Some(person.clone()), delivered_at: Some(at), watermark: None };
        assert!(!delivered_to(&dropped, &person));
    }
}
