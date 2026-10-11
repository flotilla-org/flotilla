use super::*;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{ChangeRequest, ChangeRequestSpec, Message, MessageRelation, MessageSpec, Vessel, VesselSpec};
use flotilla_store::StoreExt;

fn delta(before: &BTreeMap<String, usize>, after: BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    after
        .into_iter()
        .map(|(kind, count)| {
            let count = count - before.get(&kind).copied().unwrap_or_default();
            (kind, count)
        })
        .filter(|(_, count)| *count != 0)
        .collect()
}

async fn create_vessel(backend: &ResourceBackend) {
    backend
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder().name("vessel".into()).build(),
            &VesselSpec {
                convoy_ref: "crew".into(),
                vessel_name: "work".into(),
                placement_policy_ref: "test".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
}

fn command_context() -> CrewCommandContext {
    CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".into()),
        convoy: Some("crew".into()),
        vessel_ref: Some("vessel".into()),
        role: Some("coder".into()),
    }
}

// #3042: unrelated convoy/message/PR rows must not change either decoded-object
// counts or store operations for a request. Empty and populated stores run the
// same real service scenarios, rather than measuring execution time.
#[tokio::test]
async fn interactive_crew_reads_ignore_unrelated_resources() {
    for request in ["routing", "orientation", "complete", "teardown", "withdraw", "pending"] {
        let mut counts = Vec::new();
        for unrelated in [0, 32] {
            let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
            create_vessel(&backend).await;
            for n in 0..unrelated {
                let name = format!("unrelated-{n}");
                backend
                    .using::<ResourceConvoy>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &ConvoySpec::builder().workflow_ref("workflow".into()).role("other".into()).build(),
                    )
                    .await
                    .expect("unrelated convoy");
                backend
                    .using::<Message>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.clone()).build(),
                        &MessageSpec::builder()
                            .sender("flotilla/other".into())
                            .receiver("flotilla/other-convoy/work/coder".into())
                            .relation(MessageRelation::Supervisor)
                            .body("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated message");
                backend
                    .using::<ResourceTerminalSession>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("terminal-{n}")).build(),
                        &TerminalSessionSpec::builder()
                            .env_ref("other".into())
                            .role("coder".into())
                            .source(TerminalSessionSource::Tool { command: "sh".into() })
                            .cwd("/other".into())
                            .pool("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated terminal");
                backend
                    .using::<flotilla_resources::Artifact>("flotilla")
                    .create(
                        &InputMeta::builder().name(format!("artifact-{n}")).build(),
                        &flotilla_resources::ArtifactSpec::builder()
                            .convoy("other".into())
                            .producer("other".into())
                            .kind("explainer".into())
                            .subject("other".into())
                            .digest("digest".into())
                            .size(0)
                            .media_type("text/markdown".into())
                            .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                            .build(),
                    )
                    .await
                    .expect("unrelated artifact");
                backend
                    .using::<ChangeRequest>("flotilla")
                    .create(
                        &InputMeta::builder().name(name).build(),
                        &ChangeRequestSpec::builder()
                            .service("github".into())
                            .scope("other/repo".into())
                            .number(n + 100)
                            .observing_authority("other".into())
                            .build(),
                    )
                    .await
                    .expect("unrelated PR");
            }
            if request == "pending" {
                let sessions = backend.using::<ResourceTerminalSession>("flotilla");
                let session = sessions.get("session").await.expect("session");
                sessions
                    .update_status(
                        "session",
                        &session.metadata.resource_version,
                        &flotilla_resources::TerminalSessionStatus {
                            completion_pending: Some(flotilla_resources::CrewCompletionPending {
                                message: None,
                                disposition: None,
                                decision_ledger_ref: None,
                                force: false,
                                principal_ref: None,
                                attempted_at: chrono::Utc::now(),
                                authority: "host".into(),
                                last_error: "waiting".into(),
                            }),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("pending status");
            }
            let context = CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".into()),
                convoy: Some("crew".into()),
                vessel_ref: Some("vessel".into()),
                role: Some("coder".into()),
            };
            let before_objects = backend.read_counts();
            let before_calls = backend.read_calls();
            match request {
                "routing" => {
                    assert_eq!(
                        crew.resolve_crew_routing_context(&context).await.expect("routing").session_name.as_deref(),
                        Some("session")
                    );
                }
                "orientation" => {
                    assert_eq!(crew.crew_list_internal(&context).await.expect("orientation").members.len(), 1);
                }
                "complete" => {
                    crew.complete(&context, None, None, Some("https://example.test/ledger".into()), false, None).await.expect("completion");
                }
                "teardown" => {
                    crew.teardown("flotilla", "crew", false).await.expect("teardown");
                }
                "withdraw" => {
                    assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw"), None);
                }
                "pending" => {
                    assert_eq!(crew.pending_crew_completions().await.expect("pending").len(), 1);
                }
                _ => unreachable!(),
            }
            counts.push((delta(&before_objects, backend.read_counts()), delta(&before_calls, backend.read_calls())));
        }
        assert_eq!(counts[0], counts[1], "{request}: unrelated resources cannot increase reads or decoding");
        if request == "routing" {
            assert_eq!(counts[0].1.get("TerminalSession"), Some(&1));
        }
        if request == "complete" {
            assert_eq!(counts[0].1.get("ChangeRequest"), Some(&1));
            assert_eq!(counts[0].1.get("Artifact"), Some(&1));
        }
    }
}

// Completion evidence prefers a local Artifact over the same name from any
// replica origin. Query ordering must not change the durable ledger digest.
#[tokio::test]
async fn completion_prefers_local_artifact_evidence() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    create_vessel(&backend).await;
    let name = flotilla_resources::artifact_record_name("crew", "coder", "decision-ledger", "crew");
    let local = backend
        .using::<flotilla_resources::Artifact>("flotilla")
        .create(
            &InputMeta::builder().name(name).build(),
            &flotilla_resources::ArtifactSpec::builder()
                .convoy("crew".into())
                .producer("coder".into())
                .kind("decision-ledger".into())
                .subject("crew".into())
                .digest("local-digest".into())
                .size(0)
                .media_type("text/markdown".into())
                .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("local artifact");
    let mut replica = local;
    replica.spec.digest = "replica-digest".into();
    backend
        .replace_replicas_typed::<flotilla_resources::Artifact>(
            &flotilla_protocol::NodeId::new("remote"),
            "flotilla",
            &flotilla_resources::ResourceList { items: vec![replica], resource_version: "1".into(), generation: None },
            chrono::Utc::now(),
            None,
        )
        .await
        .expect("replica artifact");
    crew.complete(&command_context(), None, None, Some("https://example.test/ledger".into()), false, None).await.expect("complete");
    let convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    assert_eq!(convoy.status.expect("status").crew_work["work"]["coder"].decision_ledger_digest.as_deref(), Some("local-digest"));
}

// A completion waiting for the per-convoy lock must observe the caller's
// current phase after acquiring it, rather than its earlier routing snapshot.
#[tokio::test]
async fn completion_refreshes_caller_after_lock_wait() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    create_vessel(&backend).await;
    let sessions = backend.using::<ResourceTerminalSession>("flotilla");
    let session = sessions.get("session").await.expect("session");
    let running =
        flotilla_resources::TerminalSessionStatus { phase: flotilla_resources::TerminalSessionPhase::Running, ..Default::default() };
    sessions.update_status("session", &session.metadata.resource_version, &running).await.expect("running");
    let lock = crew.convoy_message_lock("flotilla", "crew").await;
    let guard = lock.lock().await;
    let before = backend.read_calls().get("TerminalSession").copied().unwrap_or_default();
    let task = tokio::spawn(async move {
        crew.complete(&command_context(), None, None, Some("https://example.test/ledger".into()), false, None).await
    });
    // Read observation synchronizes the interleaving. The timeout is a harness
    // deadlock guard, not a request-performance assertion.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while backend.read_calls().get("TerminalSession").copied().unwrap_or_default() <= before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completion routed while lock held");
    let session = sessions.get("session").await.expect("session");
    let stopped =
        flotilla_resources::TerminalSessionStatus { phase: flotilla_resources::TerminalSessionPhase::Stopped, ..Default::default() };
    sessions.update_status("session", &session.metadata.resource_version, &stopped).await.expect("stopped during wait");
    drop(guard);
    task.await.expect("task").expect("complete");
    let convoy = backend.using::<ResourceConvoy>("flotilla").get("crew").await.expect("convoy");
    assert!(!convoy.status.expect("status").crew_work["work"]["coder"].completed_while_crew_active);
}
