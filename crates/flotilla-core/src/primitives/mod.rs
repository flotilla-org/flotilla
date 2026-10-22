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

pub mod awaiting;
pub mod convert;
pub mod convoy_engine;
pub mod convoy_map;
pub mod convoys;
pub mod ensures;
pub mod import;
pub mod ports;
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
    view::{old_view, record_name, ViewLookups},
};
use flotilla_address::{Address, Kind as AddressKind};
use flotilla_engine_store::StoreLog;
use flotilla_inbox::{sqlite::SqliteInbox, ConsumerLog, ConsumerPort, InboxBackend, Observations, ReceiptScope, Supervision};
use flotilla_resources::{
    address::{artifact_address, record_name as address_record_name, repository_address},
    Artifact, Convoy, ForgeSpec, Message as OldMessage, MessageReference as OldReference, MessageSpec as OldSpec, ReadResourceObject,
    Repository, ResourceError, ResourceObject, ResourceProvenance, TerminalSession,
};
use flotilla_schema::{Kind as StoredKind, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::{
        agent_session::{AgentSession, MemoryAgentSessionWorld, Receive},
        consumer::{ConsumerKind, Receiver},
        message::{DropReason, MemoryMessageWorld, Message, MessageRef, Supersedes},
    },
    Home, Resource, Transport,
};
use flotilla_store::ResourceBackend;

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
/// only on an operator's batch failure, so no dead letter is sent. Were one
/// sent, the system would send it as turn-rules of the designated project.
struct NoSupervisors {
    system: Arc<Mutex<Option<Address>>>,
    fallback: Address,
}

impl Supervision for NoSupervisors {
    fn supervisor(&self, _: &Address) -> Option<Address> {
        None
    }
    fn system_sender(&self) -> Address {
        // Unreachable while no supervisor is declared; never a panic if that changes.
        self.system.lock().expect("system sender").clone().unwrap_or_else(|| {
            tracing::error!("a dead letter before the fleet roles were read sends as this namespace's turn-rules");
            self.fallback.clone()
        })
    }
}

/// The default namespace's turn-rules role, for a dead letter sent before
/// any designation was read; unreachable while no supervisor is declared.
fn fallback_system_sender() -> Address {
    Address::project(crate::in_process::DEFAULT_PROVISIONING_NAMESPACE)
        .and_then(|project| project.child(AddressKind::Role, "turn-rules"))
        .expect("a valid default namespace")
}

/// The minimal AgentSession records at this home (#3317): one per live crew
/// session, named by its TerminalSession record, holding the receipts.
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
        // Materialize the same minimal record here, named by the validated terminal;
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

/// The new path at one home.
pub struct MessagePrimitives {
    home: String,
    service: Arc<Service>,
    inbox: SqliteInbox,
    sessions: Arc<Sessions>,
    convoys: Arc<convoys::Convoys>,
    convoy_engine: Arc<convoy_engine::ConvoyEngine>,
    awaiting: Arc<awaiting::AwaitingReplies>,
    world: Arc<GateWorld>,
    backend: ResourceBackend,
    system: Arc<Mutex<Option<Address>>>,
    consumers: Mutex<BTreeSet<Receiver>>,
    receipts: StoreLog,
    charter_pass: Arc<tokio::sync::Mutex<()>>,
    charter_admission: Arc<tokio::sync::Mutex<()>>,
    bindings: Arc<dyn ports::RoleBindings>,
    conditions: Arc<dyn ports::ConditionReads>,
    home_reads: std::sync::OnceLock<Arc<dyn ports::HomeReads>>,
    /// Old record names to their engine id and home, filled as messages are seen.
    located: Mutex<BTreeMap<String, (String, String)>>,
    /// Old-store records, reread at most once a second per namespace.
    records: tokio::sync::Mutex<BTreeMap<String, (std::time::Instant, Arc<OldStoreRecords>)>>,
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
        let system = Arc::new(Mutex::new(None));
        let inbox = SqliteInbox::open(
            home,
            log,
            world.clone(),
            Arc::new(NoSupervisors { system: Arc::clone(&system), fallback: fallback_system_sender() }),
        )
        .map_err(|error| error.to_string())?;
        let sessions = Arc::new(Sessions {
            home: Mutex::new(Home::with_log(home, receipts.clone()).map_err(|error| format!("AgentSession home: {error:?}"))?),
        });
        // Session records persist; reopen them so a restart keeps their receipts.
        let names = service
            .with_store(|store| {
                let mut read = store.read()?;
                let mut names = Vec::new();
                let mut after = None;
                loop {
                    let page = read.scan::<Resource<AgentSession>>(home, after, 64)?;
                    for id in page.ids {
                        if let Some(loaded) = read.load::<Resource<AgentSession>>(id)? {
                            names.push(loaded.value);
                        }
                    }
                    after = page.next;
                    if after.is_none() {
                        return Ok(names);
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        for session in names {
            sessions.home.lock().expect("session home").restore(session).map_err(|error| format!("{error:?}"))?;
        }
        let convoys = Arc::new(convoys::Convoys::open(home, &service)?);
        let convoy_engine = Arc::new(convoy_engine::ConvoyEngine::new(Arc::clone(&convoys), backend.clone(), Arc::clone(&clock)));
        let awaiting = Arc::new(awaiting::AwaitingReplies::open(home, &service)?);
        Ok(Self {
            home: home.to_owned(),
            service,
            inbox,
            sessions,
            convoys,
            convoy_engine,
            awaiting,
            world,
            system,
            consumers: Mutex::new(BTreeSet::new()),
            receipts,
            charter_pass: Arc::default(),
            charter_admission: Arc::default(),
            bindings: Arc::new(ports::OldRoleRouting { backend: backend.clone() }),
            conditions: Arc::new(ports::OldStoreConditions { backend: backend.clone() }),
            home_reads: std::sync::OnceLock::new(),
            located: Mutex::new(BTreeMap::new()),
            records: tokio::sync::Mutex::new(BTreeMap::new()),
            backend,
        })
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
    /// This home's Convoy records (#3587 layer 6).
    pub fn convoys(&self) -> &Arc<convoys::Convoys> {
        &self.convoys
    }
    /// This home's AwaitingReply records (#3587 layer 7).
    pub fn awaiting(&self) -> &Arc<awaiting::AwaitingReplies> {
        &self.awaiting
    }
    /// The engine-first Convoy writer in the old shape (#3587 layer 6).
    pub fn convoy_engine(&self) -> &Arc<convoy_engine::ConvoyEngine> {
        &self.convoy_engine
    }
    pub fn world(&self) -> &Arc<GateWorld> {
        &self.world
    }
    pub fn service(&self) -> &Arc<Service> {
        &self.service
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

    /// Old-store records the views and conversions read, through their port.
    pub async fn records(&self, namespace: &str) -> Result<Arc<OldStoreRecords>, ResourceError> {
        const FRESH_FOR: std::time::Duration = std::time::Duration::from_secs(1);
        let mut cache = self.records.lock().await;
        if let Some((read_at, records)) = cache.get(namespace).filter(|(read_at, _)| read_at.elapsed() < FRESH_FOR) {
            let _ = read_at;
            return Ok(Arc::clone(records));
        }
        let records = Arc::new(OldStoreRecords::load(&self.backend, namespace).await?);
        cache.insert(namespace.to_owned(), (std::time::Instant::now(), Arc::clone(&records)));
        Ok(records)
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

    /// The fleet roles of `namespace`'s designated project, read from the
    /// record each time: a changed designation changes these addresses.
    pub async fn roles(&self, namespace: &str) -> Result<FleetRoles, String> {
        let roles = designated_fleet_roles(&self.backend, namespace)
            .await
            .map_err(|error| format!("the fleet roles are read from the FleetDesignation record: {error}"))?;
        self.system.lock().expect("system sender").get_or_insert_with(|| roles.address("turn-rules").expect("a declared default"));
        Ok(roles)
    }

    /// Admits an old-shaped intent at this home, its receiver's: maps it onto
    /// the engine spec and enqueues it as `<inbox>/message:<record name>`.
    /// A repeated name is the message already admitted.
    pub async fn admit(&self, namespace: &str, name: &str, intent: &OldSpec) -> Result<ResourceObject<OldMessage>, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let records = self.records(namespace).await?;
        let conversion = Conversion { roles: &roles, forges: &records.forges, records: &*records };
        // The messages it names, read from this replica, or at their home on a miss.
        let mut referenced = BTreeMap::new();
        for (old, inbox) in [(&intent.in_reply_to, &intent.sender), (&intent.supersedes, &intent.receiver)] {
            let Some(old) = old else { continue };
            let read = self.read_message(old, conversion.inbox(inbox).ok()).await.map_err(ResourceError::invalid)?;
            referenced.insert(old.clone(), read);
        }
        let spec = {
            let message = |old: &str| -> Result<MessageRef, String> {
                let read = referenced.get(old).ok_or_else(|| format!("message `{old}` was not read"))?;
                Ok(MessageRef::new(read.id.clone(), read.home.clone()))
            };
            conversion.spec(intent, &message).map_err(ResourceError::invalid)?
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
        self.located.lock().expect("located").insert(name.to_owned(), (id.clone(), self.home.clone()));
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
            self.supersede(&older.name, &id).await;
        }
        let mut object = match self.view(namespace, name).await {
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
        Ok(object)
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
        let records = self.records(namespace).await.map_err(|error| error.to_string())?;
        let conversion = Conversion { roles: &roles, forges: &records.forges, records: &*records };
        let sender = conversion.inbox(&intent.sender)?;
        let recipient = conversion.inbox(&intent.receiver)?;
        let message = MessageRef::new(Address::message(&recipient, name).map_err(|error| error.to_string())?.as_str(), home);
        let subject = about.map(|about| conversion.inbox(about)).transpose()?;
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
            let recorded = awaiting.record(spec, now)?;
            // A reply may have landed before the record: replies are indexed by what they answer.
            let replies = service
                .with_store(|store| {
                    let mut read = store.read()?;
                    let page = read.page(&flotilla_state_machines::machines::message::MessageSpec::by_reply(&message), None, 64)?;
                    let mut found = Vec::new();
                    for id in page.ids {
                        if let Some(loaded) = read.load::<Resource<Message>>(id)? {
                            if loaded.home == here {
                                found.push((MessageRef::new(loaded.name, loaded.home), loaded.value.spec().inbox.clone()));
                            }
                        }
                    }
                    Ok(found)
                })
                .map_err(|error| error.to_string())?;
            for (reply, inbox) in replies {
                awaiting.landed(&reply, &inbox, Some(&message), now)?;
            }
            Ok(recorded)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// One message by its old record name, in the old shape: from this
    /// replica, or read at its home on a miss and marked remote with its age.
    pub async fn read_view(
        &self,
        namespace: &str,
        record: &str,
        inbox: Option<Address>,
    ) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        if let Some(view) = self.view(namespace, record).await? {
            return Ok(Some(view));
        }
        let (Some(reads), Some(inbox)) = (self.home_reads.get(), inbox) else { return Ok(None) };
        let id = Address::message(&inbox, record).map_err(|error| ResourceError::invalid(error.to_string()))?;
        let Some(read) = reads.find(Resource::<Message>::CODE, id.as_str()).await.map_err(ResourceError::other)? else { return Ok(None) };
        let Some(snapshot) = read.value else { return Ok(None) };
        let home = snapshot.home.clone();
        let resource = snapshot.into_value::<Resource<Message>>().map_err(|error| ResourceError::decode(error.to_string()))?;
        tracing::info!(message = %id, %home, remote = read.remote, age_micros = read.age_micros, "replica miss read at the message's home");
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let records = self.records(namespace).await?;
        let object = old_view(&resource, namespace, &roles, &*records).map_err(ResourceError::invalid)?;
        let synced = chrono::Utc::now() - chrono::Duration::microseconds(read.age_micros);
        Ok(Some(ReadResourceObject {
            object,
            provenance: ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new(home), last_synced_at: synced },
        }))
    }

    /// A message by its old record name: from this replica, or, on a miss,
    /// read at its home through [`ports::HomeReads`] and marked remote with
    /// its age. `inbox` is where it was sent, which names it at its home.
    pub async fn read_message(&self, record: &str, inbox: Option<Address>) -> Result<MessageRead, String> {
        if let Some((id, home, version)) = self.locate_versioned(record)? {
            return Ok(MessageRead { id, home, version, remote: false, age_micros: 0 });
        }
        let (Some(reads), Some(inbox)) = (self.home_reads.get(), inbox) else {
            return Err(format!("message `{record}` is not in this home's replica, and no home read can reach it"));
        };
        let id = Address::message(&inbox, record).map_err(|error| error.to_string())?.as_str().to_owned();
        let read = reads.find(Resource::<Message>::CODE, &id).await?;
        match read.and_then(|read| read.value.map(|value| (value, read.remote, read.age_micros))) {
            Some((value, remote, age_micros)) => {
                tracing::info!(message = %id, home = %value.home, remote, age_micros, "replica miss read at the message's home");
                Ok(MessageRead { id, home: value.home, version: value.version, remote, age_micros })
            }
            None => Err(format!("message `{record}` is at no reachable home")),
        }
    }

    /// Drops `older` as superseded by `newer`, both homed here, once its
    /// successor is admitted. Already ended is no error.
    pub async fn supersede(&self, older: &str, newer: &str) {
        let Ok(by) = newer.parse::<Address>() else { return };
        self.world
            .update(|world| world.supersession.put(&flotilla_state_machines::Ref::new(newer), Supersedes { older: older.to_owned() }));
        if let Err(error) = self.inbox.drop(older, DropReason::Superseded { by }).await {
            // The sweep follows the successor in the log and retries.
            tracing::warn!(%older, %newer, %error, "superseded message did not drop yet");
        }
    }

    /// An operator withdraws a queued message homed here: dropped as
    /// withdrawn by turn-rules on the convoy's authority, which the
    /// operator's command is.
    pub async fn withdraw(&self, namespace: &str, record: &str, convoy: &str) -> Result<(), String> {
        let (id, home) = self.locate(record)?.ok_or_else(|| format!("message `{record}` is not in this home's replica"))?;
        if home != self.home {
            return Err(format!("message `{record}` is homed at {home}; withdraw it there"));
        }
        let roles = self.roles(namespace).await?;
        let by = roles.address("turn-rules").ok_or("turn-rules is not declared")?;
        let records = self.records(namespace).await.map_err(|error| error.to_string())?;
        let authority = records.convoy_address(convoy)?;
        self.world.update(|world| world.authority.put(&flotilla_state_machines::Ref::new(authority.as_str()), true));
        self.inbox.drop(&id, DropReason::Withdrawn { by, authority }).await.map_err(|error| error.to_string())
    }

    /// The engine id and home of the message an old record name names, in
    /// this home's replica (every home's log is followed here).
    pub fn locate(&self, record: &str) -> Result<Option<(String, String)>, String> {
        Ok(self.locate_versioned(record)?.map(|(id, home, _)| (id, home)))
    }

    /// [`MessagePrimitives::locate`], with the version this replica holds.
    pub fn locate_versioned(&self, record: &str) -> Result<Option<(String, String, u64)>, String> {
        // A message's id and home never change: a hit is resolved by its exact identity.
        let known = self.located.lock().expect("located").get(record).cloned();
        if let Some((id, home)) = known {
            let version = self
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    let Some(stored) = read.resolve(Resource::<Message>::CODE, &id, &home)? else { return Ok(None) };
                    Ok(read.identity(stored)?.map(|(_, version)| version))
                })
                .map_err(|error| error.to_string())?;
            if let Some(version) = version {
                return Ok(Some((id, home, version)));
            }
        }
        let found = self.scan_for(record)?;
        if let Some((id, home, _)) = &found {
            self.located.lock().expect("located").insert(record.to_owned(), (id.clone(), home.clone()));
        }
        Ok(found)
    }

    /// The exact message under `inbox`, at whichever followed home holds it.
    pub fn locate_in(&self, inbox: &Address, record: &str) -> Result<Option<(String, String)>, String> {
        let id = Address::message(inbox, record).map_err(|error| error.to_string())?.as_str().to_owned();
        let found = self
            .service
            .with_store(|store| {
                let mut read = store.read()?;
                for home in self.service.homes() {
                    if read.resolve(Resource::<Message>::CODE, &id, &home)?.is_some() {
                        return Ok(Some(home));
                    }
                }
                Ok(None)
            })
            .map_err(|error| error.to_string())?;
        if let Some(home) = &found {
            self.located.lock().expect("located").insert(record.to_owned(), (id.clone(), home.clone()));
        }
        Ok(found.map(|home| (id, home)))
    }

    /// The slow path for a name never seen here: every followed home's identities.
    fn scan_for(&self, record: &str) -> Result<Option<(String, String, u64)>, String> {
        let suffix = format!("/message:{}", flotilla_address::encode(record));
        self.service
            .with_store(|store| {
                for home in self.service.homes() {
                    let mut read = store.read()?;
                    let mut after = None;
                    loop {
                        let ids = read.identity_page(&home, Resource::<Message>::CODE, after, 256)?;
                        for id in &ids {
                            if let Some((identity, version)) = read.identity(*id)? {
                                if identity.name.ends_with(&suffix) {
                                    return Ok(Some((identity.name, identity.home, version)));
                                }
                            }
                        }
                        if ids.len() < 256 {
                            break;
                        }
                        after = ids.last().copied();
                    }
                }
                Ok(None)
            })
            .map_err(|error| error.to_string())
    }

    /// Every message in this home's replica, as engine resources with their homes.
    pub fn messages(&self) -> Result<Vec<(String, Resource<Message>)>, String> {
        self.service
            .with_store(|store| {
                let mut out = Vec::new();
                for home in self.service.homes() {
                    let mut read = store.read()?;
                    let mut after = None;
                    loop {
                        let page = read.scan::<Resource<Message>>(&home, after, 64)?;
                        for id in page.ids {
                            if let Some(loaded) = read.load::<Resource<Message>>(id)? {
                                out.push((home.clone(), loaded.value));
                            }
                        }
                        after = page.next;
                        if after.is_none() {
                            break;
                        }
                    }
                }
                Ok(out)
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

    /// Every message in the replica in the old shape, for the convoy and crew
    /// code that still reads old Messages (the read half of the port).
    pub async fn views(&self, namespace: &str) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let records = self.records(namespace).await?;
        let mut views = Vec::new();
        for (home, resource) in self.messages().map_err(ResourceError::other)? {
            match old_view(&resource, namespace, &roles, &*records) {
                Ok(object) => views.push(ReadResourceObject { object, provenance: self.provenance(&home) }),
                Err(error) => tracing::debug!(message = resource.name(), %error, "engine message has no old-shaped view"),
            }
        }
        Ok(views)
    }

    /// One message by its old record name, in the old shape.
    pub async fn view(&self, namespace: &str, record: &str) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        let Some((id, home)) = self.locate(record).map_err(ResourceError::other)? else { return Ok(None) };
        let resource = self
            .service
            .with_store(|store| {
                let mut read = store.read()?;
                let Some(stored) = read.resolve(Resource::<Message>::CODE, &id, &home)? else { return Ok(None) };
                Ok(read.load::<Resource<Message>>(stored)?.map(|loaded| loaded.value))
            })
            .map_err(|error| ResourceError::other(error.to_string()))?;
        let Some(resource) = resource else { return Ok(None) };
        let roles = self.roles(namespace).await.map_err(ResourceError::invalid)?;
        let records = self.records(namespace).await?;
        let object = old_view(&resource, namespace, &roles, &*records).map_err(ResourceError::invalid)?;
        Ok(Some(ReadResourceObject { object, provenance: self.provenance(&home) }))
    }

    /// Whether `record` names a message here at all.
    pub fn knows(&self, record: &str) -> bool {
        record_name(record).is_some() || self.locate(record).ok().flatten().is_some()
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

/// Crew code's port onto this path (`flotilla_crew::ports::EngineMessages`).
#[async_trait::async_trait]
impl flotilla_crew::ports::EngineMessages for MessagePrimitives {
    async fn view(&self, namespace: &str, record: &str) -> Result<Option<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::view(self, namespace, record).await
    }
    async fn views(&self, namespace: &str) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        MessagePrimitives::views(self, namespace).await
    }
    async fn admit(&self, namespace: &str, name: &str, intent: &OldSpec) -> Result<ResourceObject<OldMessage>, ResourceError> {
        MessagePrimitives::admit(self, namespace, name, intent).await
    }
    async fn withdraw(&self, namespace: &str, record: &str, convoy: &str) -> Result<(), String> {
        MessagePrimitives::withdraw(self, namespace, record, convoy).await
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
    async fn existing(&self, name: &str, _: chrono::DateTime<chrono::Utc>) -> Result<bool, ResourceError> {
        // The engine admits in the create: a stored message is admitted.
        Ok(self.primitives.locate(name).map_err(ResourceError::other)?.is_some())
    }
    async fn open_to(&self, receiver: &str) -> Result<Vec<ResourceObject<OldMessage>>, ResourceError> {
        Ok(self
            .primitives
            .views(&self.namespace)
            .await?
            .into_iter()
            .map(|view| view.object)
            .filter(|message| message.spec.receiver == receiver && message.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .collect())
    }
    async fn admit(&self, name: &str, spec: &OldSpec, _: chrono::DateTime<chrono::Utc>) -> Result<(), ResourceError> {
        self.primitives.admit(&self.namespace, name, spec).await.map(|_| ())
    }
}

/// The old-store facts the new path reads, through one named port (#3317
/// ruling 5): convoys (a role's vessel), crew sessions (a receipt's
/// session), and the records a reference names. When Convoy and the
/// session kinds move, this is the one implementation that changes.
pub struct OldStoreRecords {
    pub namespace: String,
    pub forges: Vec<ForgeSpec>,
    convoys: BTreeMap<String, ResourceObject<Convoy>>,
    terminals: BTreeMap<String, ResourceObject<TerminalSession>>,
    artifacts: BTreeMap<String, ResourceObject<Artifact>>,
    repositories: Vec<ResourceObject<Repository>>,
}

impl OldStoreRecords {
    /// The port over records already read, as a validation rehearsal holds them.
    pub fn from_records(
        namespace: &str,
        forges: Vec<ForgeSpec>,
        convoys: Vec<ResourceObject<Convoy>>,
        terminals: Vec<ResourceObject<TerminalSession>>,
        artifacts: Vec<ResourceObject<Artifact>>,
        repositories: Vec<ResourceObject<Repository>>,
    ) -> Self {
        fn by_name<R: flotilla_resources::Resource>(items: Vec<ResourceObject<R>>) -> BTreeMap<String, ResourceObject<R>> {
            items.into_iter().map(|item| (item.metadata.name.clone(), item)).collect()
        }
        Self {
            namespace: namespace.to_owned(),
            forges,
            convoys: by_name(convoys),
            terminals: by_name(terminals),
            artifacts: by_name(artifacts),
            repositories,
        }
    }

    pub async fn load(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let forges =
            backend.definitions::<flotilla_resources::Forge>(namespace).list().await?.into_iter().map(|forge| forge.spec).collect();
        let convoys = backend
            .including_replicas::<Convoy>(namespace)
            .list()
            .await?
            .items
            .into_iter()
            .map(|convoy| (convoy.object.metadata.name.clone(), convoy.object))
            .collect();
        let terminals = backend
            .including_replicas::<TerminalSession>(namespace)
            .list()
            .await?
            .items
            .into_iter()
            .map(|terminal| (terminal.object.metadata.name.clone(), terminal.object))
            .collect();
        let artifacts = backend
            .including_replicas::<Artifact>(namespace)
            .list()
            .await?
            .items
            .into_iter()
            .map(|artifact| (artifact.object.metadata.name.clone(), artifact.object))
            .collect();
        let repositories = backend
            .including_replicas::<Repository>(namespace)
            .list()
            .await?
            .items
            .into_iter()
            .map(|repository| repository.object)
            .collect();
        Ok(Self { namespace: namespace.to_owned(), forges, convoys, terminals, artifacts, repositories })
    }

    /// The project a convoy belongs to, if it declares one.
    pub fn convoy_project(&self, name: &str) -> Option<String> {
        self.convoys.get(name)?.spec.project_ref.clone()
    }

    fn convoy_address(&self, name: &str) -> Result<Address, String> {
        let convoy = self.convoys.get(name);
        let address = match convoy.and_then(|convoy| convoy.spec.project_ref.as_deref()) {
            Some(project) => Address::project(project).and_then(|project| project.child(AddressKind::Convoy, name)),
            None => Address::convoy(name),
        };
        address.map_err(|error| format!("convoy `{name}`: {error}"))
    }

    fn repository(&self, name: &str) -> Result<Address, String> {
        let repository = self
            .repositories
            .iter()
            .find(|repository| repository.metadata.name == name)
            .ok_or_else(|| format!("repository `{name}` is not declared"))?;
        let remote = repository.spec.remotes().first().ok_or_else(|| format!("repository `{name}` has no remote"))?;
        repository_address(remote, &self.forges)
    }
}

impl ReferenceAddresses for OldStoreRecords {
    fn address(&self, reference: &OldReference) -> Result<Address, String> {
        match reference {
            OldReference::ControlRecord { resource, .. } if resource.kind == "Convoy" => self.convoy_address(&resource.name),
            OldReference::Artifact { resource, .. } => {
                let artifact = self.artifacts.get(&resource.name).ok_or_else(|| format!("artifact `{}` was not read", resource.name))?;
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
    fn vessel(&self, convoy: &str, role: &str) -> Option<String> {
        self.terminals
            .values()
            .find_map(|terminal| {
                let labels = &terminal.metadata.labels;
                (labels.get(flotilla_resources::CONVOY_LABEL).map(String::as_str) == Some(convoy)
                    && labels.get(flotilla_resources::ROLE_LABEL).map(String::as_str) == Some(role))
                .then(|| labels.get(flotilla_resources::VESSEL_LABEL).cloned())
                .flatten()
            })
            .or_else(|| {
                let status = self.convoys.get(convoy)?.status.as_ref()?;
                status
                    .workflow_snapshot
                    .as_ref()?
                    .vessels
                    .iter()
                    .find(|vessel| vessel.crew.iter().any(|crew| crew.role == role))
                    .map(|vessel| vessel.name.clone())
            })
    }
    fn session(&self, receiver: &Receiver) -> Option<(String, String)> {
        let status = self.terminals.get(&receiver.id)?.status.as_ref()?;
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
            AddressKind::Artifact => {
                let name = address_record_name(address)?;
                Some(OldReference::Artifact { resource: reference("Artifact", &name, namespace), revision: revision.to_owned() })
            }
            AddressKind::Commit | AddressKind::Ref => {
                let parent = address.parent()?;
                let repository = self.repositories.iter().find(|repository| {
                    repository.spec.remotes().first().is_some_and(|remote| repository_address(remote, &self.forges).as_ref() == Ok(&parent))
                })?;
                let repository = reference("Repository", &repository.metadata.name, namespace);
                Some(match address.kind() {
                    AddressKind::Commit => OldReference::Commit { repository, revision: revision.to_owned() },
                    _ => OldReference::Ref { repository, name: address.name().to_owned(), revision: revision.to_owned() },
                })
            }
            _ => None,
        }
    }
}

/// The composition root's slot for the new path: set once, at startup,
/// when `[primitives]` is on; empty otherwise, and nothing changes.
pub type MessagePath = Arc<std::sync::OnceLock<Arc<MessagePrimitives>>>;
