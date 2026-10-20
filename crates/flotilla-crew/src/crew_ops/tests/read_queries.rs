use super::*;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::{ChangeRequest, ChangeRequestSpec, Message, MessageRelation, MessageSpec, Vessel, VesselSpec};
use flotilla_store::StoreExt;

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
    use flotilla_store_testkit::{assert_flat_reads, ReadBudget, ReadOperation, ReadSnapshot};
    async fn request_reads(request: &str, unrelated: u64) -> ReadSnapshot {
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
        let before = backend.read_snapshot();
        match request {
            "routing" => {
                assert_eq!(crew.resolve_crew_routing_context(&context).await.expect("routing").session_name.as_deref(), Some("session"));
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
        backend.read_snapshot().since(&before)
    }
    use ReadOperation::{Get, List, Query};
    // Each request reads named objects and indexed queries. The only
    // whole-kind list is completion's Forge definition catalog.
    let budgets = [
        ("routing", ReadBudget::new().objects("TerminalSession", Query, 1, 1)),
        (
            "orientation",
            ReadBudget::new()
                .objects("Convoy", Get, 1, 1)
                .calls("Demand", Query, 1)
                .objects("TerminalSession", Query, 2, 1)
                .objects("Vessel", Get, 2, 2),
        ),
        (
            "complete",
            ReadBudget::new()
                .calls("Artifact", Query, 1)
                .calls("ChangeRequest", Query, 1)
                .calls("Checkout", Query, 1)
                .objects("Convoy", Get, 4, 3)
                .calls("Forge", List, 2)
                .objects("TerminalSession", Query, 1, 1)
                .objects("TerminalSession", Get, 2, 2)
                .objects("Vessel", Get, 1, 1),
        ),
        ("teardown", ReadBudget::new().calls("Checkout", Query, 1).objects("Convoy", Get, 1, 1)),
        ("withdraw", ReadBudget::new().objects("Convoy", Get, 4, 3).calls("Message", Query, 1)),
        ("pending", ReadBudget::new().objects("TerminalSession", Query, 1, 1)),
    ];
    for (request, budget) in budgets {
        // Unrelated resources cannot increase reads or decoding, per kind and operation.
        let reads = assert_flat_reads([0, 32], |unrelated| request_reads(request, unrelated as u64)).await;
        reads.assert_within(&budget, request);
        if request == "complete" {
            assert_eq!(reads.calls("ChangeRequest", Query), 0);
            assert_eq!(reads.calls("Artifact", Query), 0);
        }
    }
}

// Completion evidence prefers a local Artifact over the same name from any
// replica origin. Query ordering must not change the durable ledger digest.
#[tokio::test]
async fn completion_advisory_does_not_write_claim_evidence() {
    let (crew, backend, _probe, _config) = fixture(CrewWorkPhase::Working).await;
    create_vessel(&backend).await;
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address("crew", "coder", "decision-ledger", "crew").expect("artifact address"),
    )
    .expect("record name");
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
    assert!(convoy.status.expect("status").crew_work["work"]["coder"].decision_ledger_digest.is_none());
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
