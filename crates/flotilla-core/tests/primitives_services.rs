//! A service consumes an inbox through its own record (#3108): the checkout inspector is
//! registered as a Service, takes a request bound to its inbox with a receipt on its record, and
//! a request to an inspector whose inbox stays unbound fails.

use std::sync::Arc;

use flotilla_engine_messages::{
    roles::{checkout_inspector, CHECKOUT_INSPECTOR},
    sweep::OpenMessages,
    MessagePrimitives, RequestAnswer,
};
use flotilla_engine_messages_testkit::Served;
use flotilla_inbox::InboxBackend;
use flotilla_provisioning_testkit::EngineBackend;
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

const NAMESPACE: &str = "flotilla";
const HOME: &str = "home-a";

async fn primitives(dir: &tempfile::TempDir) -> Served<MessagePrimitives> {
    primitives_on(dir, Arc::new(SystemClock)).await
}

async fn primitives_on(dir: &tempfile::TempDir, clock: Arc<dyn flotilla_resources::Clock>) -> Served<MessagePrimitives> {
    let home = EngineBackend::open(HOME);
    let backend = home.backend.clone();
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
    Served::holding(MessagePrimitives::open(HOME, service, backend, clock).expect("primitives"), home)
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
    assert!(matches!(primitives.request_answer(&message).await.expect("answer"), RequestAnswer::Dropped { .. }));
    assert!(matches!(primitives.request_answer("absent").await.expect("answer"), RequestAnswer::Dropped { .. }));
}

// Behaviour (#3108): a sweep names when it next has work no change wakes: just past an open
// message's unbound threshold, and nothing once the message is dropped.
#[tokio::test]
async fn a_sweep_names_when_an_unbound_request_falls_due() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(flotilla_store_testkit::VirtualClock::new(chrono::Utc::now()));
    let primitives = primitives_on(&dir, Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>).await;
    let threshold = 60_000_000;
    let request = MessageSpec { unbound_after: threshold, ..inspection(&primitives, "home-b").await };
    let start = Timestamp::from_datetime(&flotilla_resources::Clock::now(&*clock));
    primitives.inbox().enqueue(request).await.expect("enqueued");
    let mut open = OpenMessages::default();
    let early = primitives.sweep(NAMESPACE, start, &mut open).await.expect("sweep");
    assert_eq!(early.undeliverable, 0);
    let due = early.next_due.expect("the unbound threshold is due later");
    assert_eq!(due.0, start.0 + threshold + 1, "just past the threshold");
    clock.set(due.to_datetime().expect("a time"));
    let late = primitives.sweep(NAMESPACE, due, &mut open).await.expect("sweep at the due time");
    assert_eq!(late.undeliverable, 1, "dropped at the time the sweep named");
    assert_eq!(late.next_due, None);
}
