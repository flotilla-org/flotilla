//! A service consumes an inbox through its own record (#3108): the checkout inspector is
//! registered as a Service, takes a request bound to its inbox with a receipt on its record, and
//! a request to an inspector whose inbox stays unbound fails.

use std::sync::Arc;

use flotilla_core::primitives::{
    roles::{checkout_inspector, CHECKOUT_INSPECTOR},
    sweep::OpenMessages,
    MessagePrimitives, RequestAnswer,
};
use flotilla_inbox::InboxBackend;
use flotilla_resources::{FleetDesignation, FleetDesignationSpec, InputMeta, Project, ProjectSpec, SystemClock, FLEET_DESIGNATION_NAME};
use flotilla_schema::{Frozen, Timestamp};
use flotilla_schema_sqlite::stream::Service;
use flotilla_state_machines::{
    machines::{
        consumer::ConsumerKind,
        message::{Deliver, MessageBody, MessageSpec, Relation, RequestBody},
        service::ServiceState,
    },
    Transport,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const HOME: &str = "home-a";

async fn primitives(dir: &tempfile::TempDir) -> MessagePrimitives {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    backend
        .clone()
        .using::<Project>(NAMESPACE)
        .create(
            &InputMeta::builder().name("fleet".to_string()).build(),
            &ProjectSpec::builder()
                .display_name("Fleet".to_string())
                .default_workflow_ref("w".to_string())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("fleet project");
    backend
        .definitions::<FleetDesignation>(NAMESPACE)
        .create(
            &InputMeta::builder().name(FLEET_DESIGNATION_NAME.to_string()).build(),
            &FleetDesignationSpec { fleet: Some("lab".into()), project: "fleet".into(), image_cache: None },
        )
        .await
        .expect("designation");
    let service = Arc::new(
        Service::open(&dir.path().join("decided.sqlite"), HOME, flotilla_engine_store::registry().expect("registry")).expect("store"),
    );
    MessagePrimitives::open(HOME, service, backend, Arc::new(SystemClock)).expect("primitives")
}

/// A request for an inspection, from the teardown role to `host`'s inspector.
async fn inspection(primitives: &MessagePrimitives, host: &str) -> MessageSpec {
    let roles = primitives.roles(NAMESPACE).await.expect("roles");
    MessageSpec::new(
        roles.address("teardown").expect("declared"),
        checkout_inspector(host).expect("inspector inbox"),
        Relation::System,
        Frozen::new(MessageBody::Request(RequestBody::CheckoutInspection { remote: true })).expect("body"),
    )
}

fn now() -> Timestamp {
    Timestamp::from_datetime(&chrono::Utc::now())
}

// Behaviour: registering keeps one Running record per service, and a request bound to its
// inbox is answered once its receipt is on that record.
#[tokio::test]
async fn a_bound_service_answers_a_request_with_its_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let inspector = primitives.register_service(CHECKOUT_INSPECTOR, now()).await.expect("registered");
    assert_eq!(inspector.kind, ConsumerKind::Service);
    assert_eq!(inspector.host, HOME);
    assert_eq!(primitives.register_service(CHECKOUT_INSPECTOR, now()).await.expect("again"), inspector);
    let record = primitives.services().get(CHECKOUT_INSPECTOR).expect("record");
    assert_eq!(*record.state(), ServiceState::Running);

    let request = inspection(&primitives, HOME).await;
    let inbox = primitives.inbox();
    inbox.bind(request.inbox.clone(), inspector.clone()).await.expect("bind");
    let message = inbox.enqueue(request).await.expect("enqueued");
    assert_eq!(primitives.request_answer(&message).await.expect("answer"), RequestAnswer::Waiting);

    inbox.receive(&inspector, &message, Transport::Turn).await.expect("receipt");
    inbox.deliver(&message, Deliver { receiver: inspector.clone(), transport: Transport::Turn }).await.expect("deliver");
    assert_eq!(primitives.request_answer(&message).await.expect("answer"), RequestAnswer::Answered { receiver: inspector });
}

// Behaviour: a request to an inspector whose inbox stays unbound is dropped as undeliverable by
// the sweep, and its gate reads that as a failure.
#[tokio::test]
async fn a_request_to_an_unbound_inspector_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let primitives = primitives(&dir).await;
    let message = primitives.inbox().enqueue(inspection(&primitives, "home-b").await).await.expect("enqueued");
    let mut open = OpenMessages::default();
    let later = Timestamp(now().0 + 1_000_000);
    let report = primitives.sweep(NAMESPACE, later, &mut open).await.expect("sweep");
    assert_eq!(report.undeliverable, 1);
    assert!(matches!(primitives.request_answer(&message).await.expect("answer"), RequestAnswer::Failed { .. }));
    assert!(matches!(primitives.request_answer("absent").await.expect("answer"), RequestAnswer::Failed { .. }));
}
