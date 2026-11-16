use flotilla_store_memory::InMemoryBackend;
use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_protocol::{CommandAction, HostName, NodeId};
use flotilla_resources::{Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, ResourceError};
use flotilla_store::ResourceBackend;

use super::support::{designate_fleet, install_engine_messages, test_meta};
use crate::in_process::InProcessDaemon;
use crate::testkits::discovery::fake_discovery;
use flotilla_resources::{resolve_convoy_candidate_indices, ConvoyAddressIdentity};

// #2767: address-taking commands share this resolver. Terminal history must
// never displace a live governor; an exact record ID still addresses history.
#[test]
fn convoy_address_prefers_live_over_two_terminal_namesakes() {
    // Exhaustive finite contract: both project-scoped/projectless addresses and
    // all six input orders. Terminal phases share this boolean resolver input.
    for project in [None, Some("p")] {
        let scoped_address = format!("governor@{}", project.unwrap_or_default());
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let names = ["old-a", "old-b", "live"];
            let identities = order.map(|index| ConvoyAddressIdentity {
                record_name: names[index],
                role: Some("governor"),
                project,
                terminal: index < 2,
            });
            for address in [scoped_address.as_str(), "governor"] {
                let indices = resolve_convoy_candidate_indices(&identities, address).expect("unique live governor");
                assert_eq!(indices.iter().map(|index| identities[*index].record_name).collect::<Vec<_>>(), ["live"]);
            }
            let indices = resolve_convoy_candidate_indices(&identities, "old-a").expect("explicit history ID");
            assert_eq!(indices.iter().map(|index| identities[*index].record_name).collect::<Vec<_>>(), ["old-a"]);

            // A second live match must refuse and name both resource IDs.
            let mut ambiguous = Vec::from(identities);
            ambiguous.push(ConvoyAddressIdentity { record_name: "another-live", role: Some("governor"), project, terminal: false });
            let error = resolve_convoy_candidate_indices(&ambiguous, &scoped_address).expect_err("ambiguous live address");
            // Refusal must identify the live candidates, without pinning its prose.
            let named_records = error.split(|c: char| !c.is_ascii_alphanumeric() && c != '-').collect::<BTreeSet<_>>();
            for record in ["another-live", "live"] {
                assert!(named_records.contains(record), "missing {record} in refusal: {error}");
            }
            for record in ["old-a", "old-b"] {
                assert!(!named_records.contains(record), "terminal history is not an ambiguous live candidate: {error}");
            }
        }
    }
    assert!(resolve_convoy_candidate_indices(&[], "governor@p").expect("empty candidates").is_empty());
}

// Message mutations use receiver-side admission, preserving idempotent IDs and
// superseding pending intent rather than appending independent terminal inputs.
#[tokio::test]
async fn resource_message_mutations_use_durable_inbox_admission() {
    use flotilla_resources::{Message, MessagePhase};
    let temp = tempfile::tempdir().expect("config directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-test\"\n").expect("machine identity");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut document = serde_json::json!({
        "apiVersion": "flotilla.work/v1", "kind": "Message", "metadata": {"name": "first"},
        "spec": {"sender": "flotilla/checks", "receiver": "flotilla/convoy/work/coder", "relation": "system", "body": "checks settled"}
    });
    let first = daemon.apply_intent_document("flotilla", document.clone()).await.expect("admit");
    let retry = daemon.apply_intent_document("flotilla", document.clone()).await.expect("retry");
    assert_eq!(first.value, retry.value);
    document["metadata"]["name"] = "next".into();
    document["spec"]["supersedes"] = "first".into();
    daemon.apply_intent_document("flotilla", document).await.expect("successor");
    let first = backend.using::<Message>("flotilla").get("first").await.expect("read first");
    assert_eq!(first.status.expect("status").phase, MessagePhase::Superseded);
}

// A receiver's admitted convoy supplies a home before its terminal is present;
// ordinary ResourceApply routing follows that origin rather than the sender or
// an explicitly requested unrelated node.
#[tokio::test]
async fn new_message_mutations_route_to_the_absent_receivers_home() {
    use crate::command_target::TargetHost;
    let temp = tempfile::tempdir().expect("config directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-test\"\n").expect("machine identity");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("sender"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let remote = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("receiver"));
    let remote_convoys = remote.using::<ResourceConvoy>("flotilla");
    remote_convoys
        .create(&test_meta("convoy"), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
        .await
        .expect("receiver convoy");
    backend
        .replica_writer::<ResourceConvoy>(NodeId::new("receiver"), "flotilla")
        .replace(&remote_convoys.list().await.expect("receiver snapshot"), chrono::Utc::now())
        .await
        .expect("replicate");
    let action = CommandAction::ResourceApply {
        namespace: "flotilla".into(),
        document: serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Message", "metadata": {"name": "first"},
            "spec": {"sender": "flotilla/checks", "receiver": "flotilla/convoy/work/coder", "relation": "system", "body": "checks settled"}
        }),
    };
    let target = daemon.resolve_command_target(&action, Some(&NodeId::new("unrelated"))).await.expect("route");
    assert_eq!(target.host, TargetHost::Node(NodeId::new("receiver")));
    assert!(backend.using::<flotilla_resources::Message>("flotilla").list().await.expect("sender messages").items.is_empty());
}

// Resource admission qualifies a relative receiver from its sender context and
// returns the canonical delivered predecessor without creating the suppressed ID.
#[tokio::test]
async fn message_admission_qualifies_and_exposes_canonical_suppression() {
    use flotilla_resources::{Message, MessageStatusPatch, ResolvedMessageReceiver};
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-review-test\"\n").unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let mut document = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"first"},
        "spec":{"sender":"flotilla/convoy/work/reviewer","receiver":"coder","relation":"peer","body":"reply please","expectation":{"kind":"reply"},"references":[{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}],"subject":{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}}
    });
    daemon.apply_intent_document("flotilla", document.clone()).await.unwrap();
    let messages = backend.using::<Message>("flotilla");
    assert_eq!(messages.get("first").await.unwrap().spec.receiver, "flotilla/convoy/work/coder");
    flotilla_store::apply_status_patch(
        &messages,
        "first",
        &MessageStatusPatch::Delivered {
            receiver: ResolvedMessageReceiver::builder()
                .crew_id("crew".into())
                .session("session".into())
                .delivered_at(chrono::Utc::now())
                .evidence("receipt".into())
                .build(),
            at: chrono::Utc::now(),
        },
    )
    .await
    .unwrap();
    document["metadata"]["name"] = "next".into();
    let canonical = daemon.apply_intent_document("flotilla", document).await.unwrap();
    assert_eq!(canonical.value.pointer("/metadata/name").and_then(serde_json::Value::as_str), Some("first"));
    assert!(matches!(messages.get("next").await, Err(ResourceError::NotFound { .. })));
}

// Unknown homes cannot fall through to the caller's requested host. A finished
// convoy and an undeclared vessel are explicit refusals before holder lookup.
#[tokio::test]
async fn message_routing_refuses_unknown_homes_and_finished_convoys() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-review-test\"\n").unwrap();
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let document =
        |receiver: &str| serde_json::json!({"spec":{"sender":"flotilla/checks","receiver":receiver,"relation":"system","body":"wake"}});
    assert!(daemon.message_creation_origin("flotilla", &document("flotilla/undeclared")).await.unwrap_err().contains("no declared home"));
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/missing/work/coder"))
        .await
        .unwrap_err()
        .contains("no admitted convoy"));
    let declarations = backend.using::<flotilla_resources::ConvoyEnsure>("flotilla");
    declarations
        .create(
            &test_meta("waiting-governor"),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("flotilla".into())
                .role("governor".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("declared role before admission");
    assert_eq!(
        daemon.message_creation_origin("flotilla", &document("flotilla/governor")).await.expect("declared absent holder must wait"),
        Some(daemon.node_id().clone())
    );
    // The reviewer is declared at another home and reaches this daemon through that home's log.
    let decided = daemon.decided_store().expect("decided store");
    let remote = flotilla_provisioning_testkit::other_home_for(&decided, "remote-holder-home");
    let reviewer = flotilla_resources::ConvoyEnsureSpec::builder()
        .project_ref("flotilla".into())
        .role("reviewer".into())
        .repositories(Vec::new())
        .build();
    remote.definition("ConvoyEnsure", "waiting-reviewer", serde_json::to_value(&reviewer).expect("declaration")).await;
    remote.replicate_to(&decided);
    assert_eq!(
        daemon.message_creation_origin("flotilla", &document("flotilla/reviewer")).await.expect("remote declared absent holder"),
        Some(NodeId::new("remote-holder-home"))
    );
    let original = flotilla_resources::MessageSpec::builder()
        .sender("system:checks".into())
        .receiver("flotilla/governor".into())
        .relation(flotilla_resources::MessageRelation::System)
        .body("request".into())
        .build();
    daemon
        .message_inbox("flotilla")
        .await
        .accept(&test_meta("system-request"), &original, Utc::now())
        .await
        .expect("original system request");
    let reply = serde_json::json!({"spec":{"sender":"flotilla/c/work/coder","receiver":"system:checks","relation":"peer","body":"answer","in_reply_to":"system-request"}});
    assert_eq!(daemon.message_creation_origin("flotilla", &reply).await.expect("system reply home"), Some(daemon.node_id().clone()));
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys
        .create(&test_meta("finished"), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
        .await
        .unwrap();
    let status = ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() };
    convoys.update_status("finished", &convoy.metadata.resource_version, &status).await.unwrap();
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/finished/work/coder"))
        .await
        .unwrap_err()
        .contains("terminal convoy"));
    let convoy = convoys.get("finished").await.unwrap();
    let mut status = status;
    status.phase = ConvoyPhase::Active;
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: None,
        turn_delivery: Default::default(),
        stall_nudges: Default::default(),
        supervision: None,
        vessels: Vec::new(),
    });
    convoys.update_status("finished", &convoy.metadata.resource_version, &status).await.unwrap();
    assert!(daemon
        .message_creation_origin("flotilla", &document("flotilla/finished/work/coder"))
        .await
        .unwrap_err()
        .contains("undeclared vessel"));
}

// #3045: sending a message is an interactive request: routing it to the
// receiver's home and admitting it at that home read the same objects however
// many unrelated convoys, holders and messages share the namespace. Daemon
// background tasks share the counted backend, so only the request's own task
// is measured.
#[tokio::test]
async fn message_send_reads_ignore_unrelated_history() {
    use flotilla_resources::{Message, MessageRelation, MessageSpec};
    use flotilla_store_testkit::{assert_flat_reads, measure_reads, ReadBudget, ReadCountsBackendExt, ReadOperation, ReadSnapshot};
    async fn send_reads(unrelated: usize) -> (ReadSnapshot, ReadSnapshot) {
        let temp = tempfile::tempdir().expect("config directory");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"message-budget-test\"\n").expect("machine identity");
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("local"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        for index in 0..=unrelated {
            let name = if index == 0 { "convoy".to_string() } else { format!("unrelated-{index}") };
            convoys
                .create(&test_meta(&name), &ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("flotilla".into()).build())
                .await
                .expect("convoy");
            if index != 0 {
                backend
                    .using::<Message>("flotilla")
                    .create(
                        &test_meta(&format!("history-{index}")),
                        &MessageSpec::builder()
                            .sender("flotilla/checks".into())
                            .receiver(format!("flotilla/{name}/work/coder"))
                            .relation(MessageRelation::System)
                            .body("unrelated".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated message");
            }
        }
        let document = serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "Message", "metadata": {"name": "send"},
            "spec": {"sender": "flotilla/checks", "receiver": "flotilla/convoy/work/coder", "relation": "system", "body": "checks settled"}
        });
        let action = CommandAction::ResourceApply { namespace: "flotilla".into(), document: document.clone() };
        let (target, routing) = measure_reads(daemon.resolve_command_target(&action, None)).await;
        target.expect("route to the receiver's home");
        let (admitted, admission) = measure_reads(daemon.apply_intent_document("flotilla", document)).await;
        admitted.expect("admit");
        let (routing, admission) = (routing.of(&backend), admission.of(&backend));
        // Routing resolves the receiver's convoy and holder by name and index;
        // admission checks the receiver's pending history and reads back its row.
        // Two missing routing gets and the admission miss are still store calls.
        routing.assert_within(
            &ReadBudget::new()
                .objects("Convoy", ReadOperation::Query, 1, 1)
                .objects("Convoy", ReadOperation::Get, 1, 1)
                .objects("Message", ReadOperation::Get, 2, 1)
                .objects("TerminalSession", ReadOperation::Query, 1, 1),
            "message routing",
        );
        admission.assert_within(
            &ReadBudget::new().objects("Message", ReadOperation::Query, 1, 1).objects("Message", ReadOperation::Get, 2, 1),
            "message admission",
        );
        (routing, admission)
    }
    assert_flat_reads([0, 40], send_reads).await;
}

// #3938: a person sends as the role they hold, so the record names the role; who acted is audit,
// and the admission trace names the principal as given, or that none was named.
#[tokio::test]
async fn a_persons_message_traces_its_principal_and_records_its_role() {
    #[derive(Clone, Default)]
    struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lines").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let lines = Lines::default();
    let writer = lines.clone();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish());
    // While one dispatcher is registered, tracing-core caches a callsite's interest from the
    // default of whichever thread reaches it first, so a concurrent test without a subscriber
    // would turn the admission trace off for the whole binary. A second registered dispatcher
    // makes the cache ask every registered subscriber.
    let _registered = tracing::Dispatch::new(tracing_subscriber::registry());
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"principal-trace-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let primitives = install_engine_messages(&daemon);
    designate_fleet(&backend).await;
    for (name, principal, traced) in [("from-nobody", "principal:implicit", "none named"), ("from-alice", "principal:alice", "alice")] {
        let intent = flotilla_resources::MessageSpec::builder()
            .sender(principal.into())
            .receiver("system:turn-rules".into())
            .relation(flotilla_resources::MessageRelation::Peer)
            .body("ruling".into())
            .build();
        primitives.admit("flotilla", name, &intent).await.expect("admitted");
        assert_eq!(stored_sender(&primitives, "system:turn-rules", name).await, "fleet:lab/role:operator", "the record names the role");
        let logged = String::from_utf8(lines.0.lock().expect("lines").clone()).expect("UTF-8");
        assert!(logged.contains(&format!("message.principal=\"{traced}\"")), "{logged}");
    }
}

// A principal the project's charter declares keeps its name: the record names
// the role it holds there, and the crew's view reads that role back as the principal.
#[tokio::test]
async fn a_declared_principal_reads_back_under_its_own_name() {
    use flotilla_resources::{MessageRelation, MessageSpec, Project, ProjectSpec, RoleDefinition};
    use std::collections::BTreeMap;

    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"declared-principal-test\"\n").expect("daemon config");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let primitives = install_engine_messages(&daemon);
    designate_fleet(&backend).await;
    let owner = RoleDefinition { principal: Some("principal:alice".into()), ..Default::default() };
    let charter =
        ProjectSpec::builder().display_name("Flotilla".into()).role_definitions(BTreeMap::from([("owner".into(), owner)])).build();
    backend.definitions::<Project>("flotilla").apply(&test_meta("flotilla"), &charter).await.expect("project");
    let intent = MessageSpec::builder()
        .sender("principal:alice".into())
        .receiver("flotilla/gui-approver".into())
        .relation(MessageRelation::Peer)
        .body("approved".into())
        .build();
    primitives.admit("flotilla", "from-alice", &intent).await.expect("admitted");
    assert_eq!(
        stored_sender(&primitives, "flotilla/gui-approver", "from-alice").await,
        "project:flotilla/role:owner",
        "the record names the role"
    );
    let viewed = primitives.view_to("flotilla", "flotilla/gui-approver", "from-alice").await.expect("crew view").expect("the crew's view");
    assert_eq!(viewed.object.spec.sender, "principal:alice");
}

/// The sender the engine record of `record`, admitted to `receiver` and still pending, names.
async fn stored_sender(primitives: &flotilla_engine_messages::MessagePrimitives, receiver: &str, record: &str) -> String {
    let inbox = primitives.inbox_for("flotilla", receiver).await.expect("the receiver's inbox");
    let (id, _) = primitives.locate_in(&inbox, record).expect("locate").expect("stored");
    let pending = primitives.pending_in(&inbox).expect("pending");
    let (_, stored) = pending.into_iter().find(|(_, message)| message.name() == id).expect("pending");
    stored.spec().sender.to_string()
}

// A gate on the engine path reads this home's observed issue as fresh: its values are confirmed
// when read, so the condition holds. It judges freshness by the issue observer's window, as the
// old store's gates did.
#[tokio::test]
async fn engine_conditions_read_this_homes_observations_as_fresh() {
    use flotilla_resources::{Issue, IssueSpec, IssueStatus, Observation, ObservedIssueState, ThreeValue};
    let temp = tempfile::tempdir().expect("config directory");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"condition-test\"\n").expect("machine identity");
    let backend = ResourceBackend::new(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    let primitives = install_engine_messages(&daemon);
    let address = flotilla_resources::address::stored_issue_address("github.com", "team/repo", 1, &[]).expect("issue address");
    let name = flotilla_resources::address::record_name(&address).expect("record name");
    let issues = backend.using::<Issue>("flotilla");
    let issue = issues
        .create(
            &test_meta(&name),
            &IssueSpec::builder()
                .service("github.com".into())
                .scope("team/repo".into())
                .number(1)
                .observing_authority("test-host".into())
                .build(),
        )
        .await
        .expect("issue");
    let condition = flotilla_address::Condition::resolve(&format!("{address} .state == \"closed\""), &flotilla_address::BuiltinForges)
        .expect("condition");
    let observed = Utc::now();
    let status = IssueStatus {
        title: Default::default(),
        assignees: Default::default(),
        state: Observation::known(ObservedIssueState::Closed, observed),
        labels: Observation::known(Vec::new(), observed),
        updated_at: Observation::known(observed, observed),
    };
    issues.update_status(&name, &issue.metadata.resource_version, &status).await.expect("observed");
    assert_eq!(primitives.conditions().value("flotilla", &condition).await.expect("value"), ThreeValue::True);
    assert_eq!(primitives.observation_staleness(), daemon.crew.message_observation_staleness(), "the observers' windows");
}
