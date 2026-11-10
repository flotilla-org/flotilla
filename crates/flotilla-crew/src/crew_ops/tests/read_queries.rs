use super::*;
use chrono::Utc;
use flotilla_protocol::CrewCommandContext;
use flotilla_provisioning_testkit::CheckoutFixture;
use flotilla_resources::checkout_records::{CheckoutRecord, CheckoutRecords as _, Inspection, Landed};
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
                fulfilment_kind_ref: "test".into(),
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
                crew.complete(
                    &context,
                    None,
                    None,
                    Some("https://example.test/ledger".into()),
                    Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
                )
                .await
                .expect("completion");
            }
            "teardown" => {
                crew.teardown("flotilla", "crew", false).await.expect("teardown");
            }
            "withdraw" => {
                assert_eq!(crew.withdraw_pending_brief("flotilla", "crew").await.expect("withdraw"), None);
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
    crew.complete(
        &command_context(),
        None,
        None,
        Some("https://example.test/ledger".into()),
        Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
    )
    .await
    .expect("complete");
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
        crew.complete(
            &command_context(),
            None,
            None,
            Some("https://example.test/ledger".into()),
            Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
        )
        .await
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

// Inspection boundary fake: explicit deletion obtains newly observed checkout facts, written to
// the engine as an inspection of the checkout's home writes them.
struct FreshDeleteInspection(std::sync::atomic::AtomicUsize, Arc<CheckoutFixture>);
#[async_trait::async_trait]
impl crate::ports::CheckoutIntegrationInspector for FreshDeleteInspection {
    async fn inspect_now(&self, _: &str, checkout: &CheckoutRecord) -> Result<(), String> {
        let inspection = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // A later inspection that cannot judge the landing keeps the earlier evidence.
        let landed_evidence = (inspection == 0).then(|| Landed {
            change_request: 42,
            merged_at: Some(Utc::now()),
            target_ref: Some("main".into()),
            head_in_merged_head: true,
        });
        let landed = landed_evidence.as_ref().map(|_| true);
        self.1
            .checkouts
            .inspected(&checkout.name, Inspection { clean: Some(true), pushed: Some(true), landed, landed_evidence, ..Default::default() })
            .map(|_| ())
    }
}
async fn active_delete_fixture() -> (Arc<CrewService>, ResourceBackend, Arc<CheckoutFixture>, String, tempfile::TempDir) {
    let (mut crew, backend, _, _config) = fixture(CrewWorkPhase::Working).await;
    let engine = Arc::new(CheckoutFixture::default());
    Arc::get_mut(&mut crew).expect("unique service").lifecycle_reads = engine.reads.clone();
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let mut convoy = convoys.get("crew").await.expect("convoy");
    convoy.spec.adopted_checkout_refs.insert(flotilla_protocol::RepositoryKey("repo".into()), "checkout".into());
    let convoy = convoys.update(&InputMeta::from(&convoy.metadata), &convoy.metadata.resource_version, &convoy.spec).await.expect("adopt");
    let mut status = convoy.status.expect("status");
    status.phase = ConvoyPhase::Active;
    convoys.update_status("crew", &convoy.metadata.resource_version, &status).await.expect("active");
    // The adopted checkout is held for the convoy, realised and never inspected.
    engine.admit_for(&backend, "flotilla", "crew", "crew").await;
    let launch = engine.put_ready(
        "checkout",
        &flotilla_resources::CheckoutSpec::Observed(
            flotilla_resources::ObservedCheckoutSpec::builder()
                .r#ref("topic".into())
                .path("/checkout".into())
                .repo_ref(flotilla_protocol::RepositoryKey("repo".into()))
                .host_ref("home".into())
                .is_main(false)
                .build(),
        ),
        Some(&engine.holder("crew")),
    );
    (crew, backend, engine, launch, _config)
}

// Process/VCS boundary stand-in: a transient failure must not publish fallback evidence.
struct FailedDeleteInspection;
#[async_trait::async_trait]
impl crate::ports::CheckoutIntegrationInspector for FailedDeleteInspection {
    async fn inspect_now(&self, _: &str, _: &CheckoutRecord) -> Result<(), String> {
        Err("VCS temporarily unavailable".into())
    }
}
// An Active checkout requires a wired inspector; transient inspection failure
// refuses deletion and leaves its prior observation untouched.
#[tokio::test]
async fn active_teardown_refuses_unset_or_failing_inspector() {
    for configured in [false, true] {
        let (crew, _backend, engine, launch, _config) = active_delete_fixture().await;
        if configured {
            crew.set_checkout_inspector(Arc::new(FailedDeleteInspection));
        }
        let error = crew.teardown("flotilla", "crew", false).await.expect_err("inspection is required");
        assert!(
            error.contains(if configured {
                "fresh inspection failed for checkout checkout: VCS temporarily unavailable"
            } else {
                "inspector not configured"
            }),
            "{error}"
        );
        let checkout = engine.record(&launch);
        assert_eq!(checkout.inspection(), None, "no inspection is written for a failed one");
        assert!(checkout.is_ready(), "the checkout is preserved");
    }
}

// #3710: each manual Active-convoy delete forces one fresh checkout inspection,
// including a second request inside the normal evidence freshness horizon.
#[tokio::test]
async fn manual_active_convoy_delete_inspects_checkout_once_per_request() {
    let (crew, _backend, engine, launch, _config) = active_delete_fixture().await;
    assert!(crew.teardown("flotilla", "crew", false).await.expect_err("missing inspector").contains("inspector not configured"));
    let inspector = Arc::new(FreshDeleteInspection(0.into(), Arc::clone(&engine)));
    crew.set_checkout_inspector(inspector.clone());
    crew.teardown("flotilla", "crew", false).await.expect("fresh delete evidence");
    crew.teardown("flotilla", "crew", false).await.expect("second explicit inspection");
    assert_eq!(inspector.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(engine.record(&launch).inspection().and_then(|inspection| inspection.landed), Some(true));
}
