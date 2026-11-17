//! The engine Message path for tests (#3108): a home's [`MessagePrimitives`] over a temporary
//! decided store, installed on the resource backend as its attentions and its Message records the
//! way the composition root installs it, and the designated fleet whose roles its delivery reads.

use std::sync::{Arc, RwLock};

use flotilla_engine_messages::{records::MessageRecords, roles::FleetRoles, MessagePrimitives};
use flotilla_inbox::InboxBackend;
use flotilla_messaging::{EngineMessages, Intent};
use flotilla_resources::{
    attentions::Attentions, Clock, Convoy, ConvoySpec, ConvoyStatus, CrewSource, CrewSpec, FleetDesignation, FleetDesignationSpec,
    InputMeta, Message as OldMessage, MessageRelation, Project, ProjectSpec, ReadResourceObject, ResourceError, Selector, SystemClock,
    VesselRequirement, WorkflowSnapshot, FLEET_DESIGNATION_NAME,
};
use flotilla_schema::{Address, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{machines::message::Deliver, Transport};
use flotilla_store::ResourceBackend;

/// A Message path held with the backend it reads. The path holds its backend weakly, since an
/// installed path is held by that backend; the composition root holds both, and so does this.
pub struct Served<T> {
    path: T,
    _backend: ResourceBackend,
}

impl<T> Served<T> {
    pub fn new(path: T, backend: ResourceBackend) -> Self {
        Self { path, _backend: backend }
    }
}

impl<T> std::ops::Deref for Served<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.path
    }
}

/// One home's engine Message path; the decided store lives as long as this value.
pub struct EngineMessagesHome {
    primitives: Arc<MessagePrimitives>,
    service: Arc<Service>,
    namespace: String,
    backend: ResourceBackend,
    _dir: Option<tempfile::TempDir>,
}

impl EngineMessagesHome {
    /// Opens `home`'s Message path on the system clock and installs it on `backend`, serving
    /// Message under `namespace`.
    pub fn open(home: &str, namespace: &str, backend: &ResourceBackend) -> Self {
        Self::open_with_clock(home, namespace, backend, Arc::new(SystemClock))
    }

    /// [`Self::open`] on `clock`.
    pub fn open_with_clock(home: &str, namespace: &str, backend: &ResourceBackend, clock: Arc<dyn Clock>) -> Self {
        let dir = tempfile::tempdir().expect("decided store directory");
        let registry = flotilla_engine_store::registry().expect("engine kind registry");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("decided store"));
        let mut opened = Self::over_with_clock(service, namespace, backend, clock);
        opened._dir = Some(dir);
        opened
    }

    /// The Message path over `service`, a decided store the test holds (one with the kinds of
    /// other engine readers beside it, say), installed on `backend` as [`Self::open`] installs it.
    pub fn over(service: Arc<Service>, namespace: &str, backend: &ResourceBackend) -> Self {
        Self::over_with_clock(service, namespace, backend, Arc::new(SystemClock))
    }

    /// [`Self::over`], its messages stamped by `clock`.
    pub fn over_with_clock(service: Arc<Service>, namespace: &str, backend: &ResourceBackend, clock: Arc<dyn Clock>) -> Self {
        let primitives =
            Arc::new(MessagePrimitives::open(service.home(), Arc::clone(&service), backend.clone(), clock).expect("engine Message path"));
        backend.install_attentions(Arc::clone(&primitives) as Arc<dyn Attentions>);
        backend.install_engine_records(Arc::new(MessageRecords::new(Arc::clone(&primitives), Arc::new(RwLock::new(namespace.to_owned())))));
        Self { primitives, service, namespace: namespace.to_owned(), backend: backend.clone(), _dir: None }
    }

    pub fn primitives(&self) -> &Arc<MessagePrimitives> {
        &self.primitives
    }

    /// The decided store the path writes.
    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    /// Records the message `record` sent to `receiver` (an old address) as a turn crew `session`
    /// confirmed, the way the daemon's delivery records one: the session registered and bound to
    /// the message's inbox, then its receipt, then its deliver.
    pub async fn deliver_turn(&self, receiver: &str, record: &str, session: &str) {
        deliver_turn(&self.primitives, &self.namespace, receiver, record, session, chrono::Utc::now()).await;
    }

    /// Every message this home holds, in the old shape the crew reads: listed by the generic
    /// resource verb, each then read in the inbox it names.
    pub async fn views(&self) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
        views(&self.primitives, &self.backend, &self.namespace).await
    }

    /// The path as the crew's Message port.
    pub fn messages(&self) -> Arc<dyn EngineMessages> {
        Arc::clone(&self.primitives) as Arc<dyn EngineMessages>
    }
}

/// [`EngineMessagesHome::views`] on any home's Message path in `namespace`, such as a daemon's,
/// listed through `backend`'s generic resource verb.
pub async fn views(
    primitives: &MessagePrimitives,
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<Vec<ReadResourceObject<OldMessage>>, ResourceError> {
    let listed = backend.using::<OldMessage>(namespace).list().await?.items;
    let mut views = Vec::with_capacity(listed.len());
    for stored in listed {
        let inbox: Address = stored.spec.receiver.parse().map_err(|error| ResourceError::invalid(format!("{error}")))?;
        views.extend(primitives.read_view(namespace, &stored.metadata.name, inbox).await?);
    }
    Ok(views)
}

/// [`EngineMessagesHome::deliver_turn`] on any home's Message path in `namespace`, such as a
/// daemon's, the session registered `at`. `receiver` is an old address, or the inbox's engine
/// address as the generic verbs show it.
pub async fn deliver_turn(
    primitives: &MessagePrimitives,
    namespace: &str,
    receiver: &str,
    record: &str,
    session: &str,
    at: chrono::DateTime<chrono::Utc>,
) {
    let address = match receiver.parse::<Address>() {
        Ok(inbox) => inbox,
        Err(_) => primitives.inbox_for(namespace, receiver).await.expect("the receiver's inbox"),
    };
    let (id, _) = primitives.locate_in(&address, record).expect("locate").unwrap_or_else(|| panic!("no message `{record}`"));
    let consumer = primitives.register(session, Timestamp::from_datetime(&at)).await.expect("register the session");
    let inbox = primitives.inbox();
    inbox.bind(address, consumer.clone()).await.expect("bind the session");
    inbox.receive(&consumer, &id, Transport::Turn).await.expect("receipt");
    inbox.deliver(&id, Deliver { receiver: consumer, transport: Transport::Turn }).await.expect("deliver");
}

/// The inbox `party` names in fleet `lab`, which [`designate_fleet`] designates: a crew seat
/// (`P/C/V/R` or `P/R`) or a `system:` producer as [`FleetRoles::inbox`] reads it, else an engine
/// address as written. A person is written as the role address they hold. A seat's project
/// segment holds no `:`, though a standing launch's convoy segment does.
pub fn party(party: &str) -> Address {
    let project = party.split('/').next().unwrap_or_default();
    if party.starts_with("system:") || !project.contains(':') {
        return FleetRoles::declared("lab").and_then(|roles| roles.inbox(party)).unwrap_or_else(|error| panic!("{party}: {error}"));
    }
    party.parse().unwrap_or_else(|error| panic!("{party}: {error}"))
}

/// A message `body` from `sender` to `receiver`, each read by [`party`], as `relation`,
/// expecting nothing.
pub fn intent(sender: &str, receiver: &str, relation: MessageRelation, body: impl Into<String>) -> Intent {
    Intent::new(party(sender), party(receiver), relation, body)
}

/// Creates convoy `name` with `spec` in `namespace`, its frozen workflow one vessel, `work`,
/// crewed by an agent seat for each of `roles`: the snapshot every engine convoy holds from its
/// admission, through which a seat's vessel is spelled.
pub async fn crewed_convoy(backend: &ResourceBackend, namespace: &str, name: &str, spec: &ConvoySpec, roles: &[&str]) {
    let convoys = backend.using::<Convoy>(namespace);
    let convoy = convoys.create(&InputMeta::builder().name(name.into()).build(), spec).await.expect("convoy");
    let crew = roles
        .iter()
        .map(|role| {
            CrewSpec::builder()
                .role((*role).to_owned())
                .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                .build()
        })
        .collect();
    let status = ConvoyStatus {
        workflow_snapshot: Some(WorkflowSnapshot {
            cascade: None,
            vessels: vec![VesselRequirement::builder().name("work".to_owned()).crew(crew).build()],
            exit: None,
            turn_delivery: Default::default(),
            stall_nudges: Default::default(),
            supervision: None,
        }),
        ..ConvoyStatus::default()
    };
    convoys.update_status(name, &convoy.metadata.resource_version, &status).await.expect("convoy snapshot");
}

/// Declares Project `fleet` in `namespace` and designates it, so the fleet roles the system
/// sends as resolve.
pub async fn designate_fleet(backend: &ResourceBackend, namespace: &str) {
    backend
        .definitions::<Project>(namespace)
        .apply(
            &InputMeta::builder().name("fleet".into()).build(),
            &ProjectSpec::builder().display_name("Fleet".into()).default_workflow_ref("work".into()).repositories(Vec::new()).build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>(namespace)
        .apply(
            &InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build(),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("fleet designation");
}
