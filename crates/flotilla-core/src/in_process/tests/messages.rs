use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::Utc;
use flotilla_protocol::{CommandAction, HostName, NodeId};
use flotilla_resources::{
    Convoy as ResourceConvoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InMemoryBackend, ResourceBackend, ResourceError,
};

use super::support::test_meta;
use crate::config::ConfigStore;
use crate::in_process::convoy_admission::{resolve_convoy_candidate_indices, ConvoyAddressIdentity};
use crate::in_process::crew_ops::{convoy_message_address, convoy_sender_address};
use crate::in_process::{retry_resource_apply, InProcessDaemon};
use crate::testkits::discovery::fake_discovery;

// #2597: failed reads retain the resource identity and emit scoped debug diagnostics;
// successful reads never emit fallback diagnostics. Glue: exhaust the three read outcomes.
#[tokio::test]
async fn convoy_sender_lookup_diagnostics_preserve_fallbacks() {
    use crate::testkits::replay::testing::capture_logs;
    let memory = ResourceBackend::InMemory(InMemoryBackend::default());
    memory
        .using::<ResourceConvoy>("attribution")
        .create(&test_meta("supervisor"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).role("governor".to_string()).build())
        .await
        .expect("convoy");
    // Real HTTP collaborator with an invalid URL fails before any network request.
    let invalid = ResourceBackend::Http(flotilla_resources::HttpBackend::new(crate::tls::client(), "://invalid"));
    for (backend, name, expected, failure) in
        [(&memory, "supervisor", "governor", false), (&memory, "missing", "missing", true), (&invalid, "unavailable", "unavailable", true)]
    {
        // Mirror the sender lookup's replica-inclusive read to check the exact backend error.
        // Keep this read aligned if the attribution lookup path changes.
        let read = backend.including_replicas::<ResourceConvoy>("attribution").get(name).await;
        let (address, logs) = capture_logs(tracing::Level::DEBUG, convoy_sender_address(backend, "attribution", name)).await;
        assert_eq!(address, expected);
        if failure {
            let error = read.expect_err("failed read").to_string();
            assert!(logs.contains("DEBUG"), "{logs}");
            assert!(logs.contains("namespace=attribution"), "{logs}");
            assert!(logs.contains(&format!("convoy_ref={name}")), "{logs}");
            assert!(logs.contains(&format!("error={error}")), "{logs}");
            assert_eq!(logs.matches("convoy sender attribution lookup failed").count(), 1, "{logs}");
        } else {
            assert!(!logs.contains("convoy sender attribution lookup failed"), "{logs}");
        }
    }
}

// #2592: attribution uses role/project addresses, preserves legacy resource names,
// and remains available when a supervisor convoy is absent from the replica view.
// Formatting glue: these rows exhaust empty/nonempty role and absent/present project.
#[tokio::test]
async fn convoy_sender_addresses_preserve_legacy_and_missing_convoy_identity() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    for (name, role, project, expected) in [
        ("legacy", "", None, "legacy"),
        ("legacy-project", "", Some("project"), "legacy-project"),
        ("named", "graphql-budget", None, "graphql-budget"),
        ("named-project", "graphql-budget", Some("project"), "graphql-budget@project"),
    ] {
        let convoy = convoys
            .create(
                &test_meta(name),
                &ConvoySpec::builder()
                    .workflow_ref("workflow".to_string())
                    .role(role.to_string())
                    .maybe_project_ref(project.map(str::to_string))
                    .build(),
            )
            .await
            .expect("convoy");
        assert_eq!(convoy_message_address(&convoy), expected);
        assert_eq!(convoy_sender_address(&backend, "flotilla", name).await, expected);
    }
    assert_eq!(convoy_sender_address(&backend, "flotilla", "missing-governor").await, "missing-governor");
}

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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("test-host"),
        backend.clone(),
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("sender"),
        backend.clone(),
    )
    .await;
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("receiver"));
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
    )
    .await;
    let mut document = serde_json::json!({
        "apiVersion":"flotilla.work/v1", "kind":"Message", "metadata":{"name":"first"},
        "spec":{"sender":"flotilla/convoy/work/reviewer","receiver":"coder","relation":"peer","body":"reply please","expectation":{"kind":"reply"},"references":[{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}],"subject":{"kind":"change_request","service":"github","scope":"owner/repo","number":1,"revision":"head"}}
    });
    daemon.apply_intent_document("flotilla", document.clone()).await.unwrap();
    let messages = backend.using::<Message>("flotilla");
    assert_eq!(messages.get("first").await.unwrap().spec.receiver, "flotilla/convoy/work/coder");
    flotilla_resources::apply_status_patch(
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
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
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
    let remote = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("remote-holder-home"));
    remote
        .using::<flotilla_resources::ConvoyEnsure>("flotilla")
        .create(
            &test_meta("waiting-reviewer"),
            &flotilla_resources::ConvoyEnsureSpec::builder()
                .project_ref("flotilla".into())
                .role("reviewer".into())
                .repositories(Vec::new())
                .build(),
        )
        .await
        .expect("remote absent holder declaration");
    backend
        .replica_writer::<flotilla_resources::ConvoyEnsure>(NodeId::new("remote-holder-home"), "flotilla")
        .replace(&remote.using::<flotilla_resources::ConvoyEnsure>("flotilla").list().await.expect("declaration feed"), Utc::now())
        .await
        .expect("replicated declaration");
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

#[tokio::test]
async fn message_apply_retries_only_conflicts_with_a_finite_budget() {
    for kind in ["Message", "Artifact"] {
        let mut attempts = 0;
        let result = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(if attempts < 3 { Err(ResourceError::conflict("message", "concurrent status")) } else { Ok(attempts) })
        })
        .await;
        assert_eq!(result.unwrap(), 3);
        let mut attempts = 0;
        let result: Result<(), _> = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(Err(ResourceError::conflict("message", "persistent conflict")))
        })
        .await;
        assert!(matches!(result, Err(ResourceError::Conflict { .. })));
        assert_eq!(attempts, 16);
        let mut attempts = 0;
        let result: Result<(), _> = retry_resource_apply(kind, || {
            attempts += 1;
            std::future::ready(Err(ResourceError::invalid("invalid intent")))
        })
        .await;
        assert!(matches!(result, Err(ResourceError::Invalid { .. })));
        assert_eq!(attempts, 1);
    }
    let mut attempts = 0;
    let result: Result<(), _> = retry_resource_apply("Convoy", || {
        attempts += 1;
        std::future::ready(Err(ResourceError::conflict("convoy", "no replay contract")))
    })
    .await;
    assert!(result.is_err());
    assert_eq!(attempts, 1);
}
