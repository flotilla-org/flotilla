//! Charter notices on the engine (#3108): each applied charter revision notifies a role's holder
//! once, the newest notice superseding the one still open, chosen from the receiver's open
//! messages without converting them.

use std::collections::BTreeMap;
use std::sync::Arc;

use flotilla_engine_messages::{EngineCharterNotices, MessagePrimitives};
use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
use flotilla_messaging::{reconcile_charter_notifications_with_receivers, CharterProseRenderer, MessageReceiverSnapshot};
use flotilla_resources::definition_provenance::CHARTER_REVISION_ANNOTATION;
use flotilla_resources::{
    Convoy, ConvoySpec, InputMeta, MessageRelation, MessageSpec, Project, ProjectSpec, RoleDefinition, Selector, TerminalBrief,
    TerminalCrewContext, TerminalSession, TerminalSessionSource, TerminalSessionSpec, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_state_machines::machines::message::{DropReason, MessageState};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;

const NAMESPACE: &str = "flotilla";
const RECEIVER: &str = "root/root-one/work/guide";

/// A project `root` declaring the `guide` role, held by a running crew of convoy `root-one`.
async fn guided(backend: &ResourceBackend) {
    backend
        .definitions::<Project>(NAMESPACE)
        .create(
            &InputMeta::builder().name("root".into()).build(),
            &ProjectSpec::builder()
                .display_name("root".into())
                .role_definitions(BTreeMap::from([("guide".into(), RoleDefinition::default())]))
                .build(),
        )
        .await
        .expect("project");
    backend
        .using::<Convoy>(NAMESPACE)
        .create(
            &InputMeta::builder().name("root-one".into()).build(),
            &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("root".into()).build(),
        )
        .await
        .expect("convoy");
    let holder = TerminalSessionSpec::builder()
        .env_ref("host-direct".into())
        .role("guide".into())
        .cwd("/workspace".into())
        .pool("cleat".into())
        .source(TerminalSessionSource::Agent {
            selector: Selector::for_capability("coding"),
            brief: TerminalBrief {
                path: ".flotilla/briefs/guide.md".into(),
                content: "guide".into(),
                artifact_digest: None,
                copies: Vec::new(),
            },
            context: Box::new(TerminalCrewContext {
                namespace: NAMESPACE.into(),
                convoy: "root-one".into(),
                vessel_ref: "root-one-work".into(),
            }),
        })
        .build();
    backend
        .using::<TerminalSession>(NAMESPACE)
        .create(
            &InputMeta::builder()
                .name("terminal-root-one".into())
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.into(), "root-one".into()),
                    (ROLE_LABEL.into(), "guide".into()),
                    (VESSEL_LABEL.into(), "work".into()),
                ]))
                .build(),
            &holder,
        )
        .await
        .expect("holder");
}

/// Applies charter `revision` to project `root` and runs one notification pass on the engine.
async fn apply_and_notify(backend: &ResourceBackend, primitives: &Arc<MessagePrimitives>, revision: &str) {
    let projects = backend.definitions::<Project>(NAMESPACE);
    let project = projects.get("root").await.expect("project");
    let mut meta = InputMeta::from(&project.metadata);
    meta.annotations.insert(CHARTER_REVISION_ANNOTATION.into(), revision.into());
    projects.apply(&meta, &project.spec).await.expect("revision");
    notify(backend, primitives).await;
}

async fn notify(backend: &ResourceBackend, primitives: &Arc<MessagePrimitives>) {
    let notices = EngineCharterNotices { primitives: Arc::clone(primitives), namespace: NAMESPACE.into() };
    let receivers = MessageReceiverSnapshot::load(backend, NAMESPACE).await.expect("receivers");
    let projects = backend.including_replicas::<Project>(NAMESPACE).list().await.expect("projects");
    reconcile_charter_notifications_with_receivers(&notices, &CharterProseRenderer, chrono::Utc::now(), backend, &receivers, &projects)
        .await
        .expect("charter pass");
}

/// The Message rows loaded and converted by a charter revision's pass, with `count` other
/// messages open to the role's holder and the previous revision's notice still open.
async fn read_by_a_charter_pass(count: usize) -> (u64, u64) {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    guided(&backend).await;
    let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    let primitives = home.primitives();
    apply_and_notify(&backend, primitives, "first").await;
    for index in 0..count {
        let other = MessageSpec::builder()
            .sender("system:turn-rules".into())
            .receiver(RECEIVER.into())
            .relation(MessageRelation::System)
            .body(format!("other {index}"))
            .build();
        primitives.admit(NAMESPACE, &format!("other-{index}"), &other).await.expect("another message");
    }
    let before = primitives.rows_read();
    apply_and_notify(&backend, primitives, "second").await;
    let after = primitives.rows_read();
    (after.loaded - before.loaded, after.converted - before.converted)
}

// Behaviour: a second revision's notice supersedes the first, and a repeated pass admits nothing.
#[tokio::test]
async fn charter_revisions_coalesce_to_the_latest_notice() {
    let backend = ResourceBackend::new(InMemoryBackend::default());
    designate_fleet(&backend, NAMESPACE).await;
    guided(&backend).await;
    let home = EngineMessagesHome::open("home-a", NAMESPACE, &backend);
    let primitives = home.primitives();
    apply_and_notify(&backend, primitives, "first").await;
    apply_and_notify(&backend, primitives, "second").await;
    notify(&backend, primitives).await;
    let inbox = primitives.inbox_for(NAMESPACE, RECEIVER).await.expect("the guide's inbox");
    let open = primitives.open_in(&inbox).expect("open messages");
    assert_eq!(open.len(), 1, "only the latest notice is open");
    let latest = &open[0].1;
    assert!(flotilla_engine_messages::view::body_text(latest.spec().body.value()).contains("charter@second"));
    let first = latest.spec().supersedes.as_ref().expect("the latest supersedes the first");
    let dropped = primitives.message(&first.name).await.expect("load").expect("the first notice");
    assert!(matches!(dropped.state(), MessageState::Dropped { reason: DropReason::Superseded { .. }, .. }), "{:?}", dropped.state());
}

// Behaviour: a charter pass picks the notice it supersedes without converting the receiver's
// open messages; the scan loads them through the inbox's index, and only admission's read-back
// converts.
#[tokio::test]
async fn a_charter_pass_picks_its_predecessor_without_converting_open_messages() {
    let (five, sixty) = (read_by_a_charter_pass(5).await, read_by_a_charter_pass(60).await);
    assert_eq!(five.1, 1, "five other messages: {five:?}");
    assert_eq!(sixty.1, 1, "sixty other messages: {sixty:?}");
    assert_eq!(sixty.0 - five.0, 55, "the scan loads each open message once: {five:?} {sixty:?}");
}
