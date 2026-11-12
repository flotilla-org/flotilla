//! The engine Message path for tests (#3108): a home's [`MessagePrimitives`] over a temporary
//! decided store, installed on the resource backend as its attentions and its Message records the
//! way the composition root installs it, and the designated fleet whose roles its delivery reads.

use std::sync::{Arc, RwLock};

use flotilla_engine_messages::{records::MessageRecords, MessagePrimitives};
use flotilla_inbox::InboxBackend;
use flotilla_messaging::EngineMessages;
use flotilla_resources::{
    attentions::Attentions, Clock, FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec, SystemClock,
    FLEET_DESIGNATION_NAME,
};
use flotilla_schema::Timestamp;
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
    _dir: tempfile::TempDir,
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
        let primitives =
            Arc::new(MessagePrimitives::open(home, Arc::clone(&service), backend.clone(), clock).expect("engine Message path"));
        backend.install_attentions(Arc::clone(&primitives) as Arc<dyn Attentions>);
        backend.install_engine_records(Arc::new(MessageRecords::new(Arc::clone(&primitives), Arc::new(RwLock::new(namespace.to_owned())))));
        Self { primitives, service, _dir: dir }
    }

    pub fn primitives(&self) -> &Arc<MessagePrimitives> {
        &self.primitives
    }

    /// The decided store the path writes.
    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    /// Records the message `record` names as a turn crew `session` confirmed, the way the daemon's
    /// delivery records one: the session registered and bound to the message's inbox, then its
    /// receipt, then its deliver.
    pub async fn deliver_turn(&self, record: &str, session: &str) {
        deliver_turn(&self.primitives, record, session, chrono::Utc::now()).await;
    }

    /// The path as the crew's Message port.
    pub fn messages(&self) -> Arc<dyn EngineMessages> {
        Arc::clone(&self.primitives) as Arc<dyn EngineMessages>
    }
}

/// [`EngineMessagesHome::deliver_turn`] on any home's Message path, such as a daemon's, the
/// session registered `at`.
pub async fn deliver_turn(primitives: &MessagePrimitives, record: &str, session: &str, at: chrono::DateTime<chrono::Utc>) {
    let (id, _) = primitives.locate(record).expect("locate").unwrap_or_else(|| panic!("no message `{record}`"));
    let messages = primitives.messages().expect("messages");
    let (_, message) = messages.iter().find(|(_, message)| message.name() == id).expect("the located message");
    let consumer = primitives.register(session, Timestamp::from_datetime(&at)).await.expect("register the session");
    let inbox = primitives.inbox();
    inbox.bind(message.spec().inbox.clone(), consumer.clone()).await.expect("bind the session");
    inbox.receive(&consumer, &id, Transport::Turn).await.expect("receipt");
    inbox.deliver(&id, Deliver { receiver: consumer, transport: Transport::Turn }).await.expect("deliver");
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
