use super::*;
use chrono::Utc;
use flotilla_protocol::CrewCommandContext;
use flotilla_resources::checkout_records::{CheckoutHolder, CheckoutRecord, CheckoutRecords as _, Inspection, Landed};
use flotilla_resources::{ChangeRequest, ChangeRequestSpec, MessageRelation};
use flotilla_store::StoreExt;

/// The coder's context in the fixture convoy's vessel.
fn command_context(held: &Held) -> CrewCommandContext {
    CrewCommandContext {
        crew_id: None,
        namespace: Some("flotilla".into()),
        convoy: Some(held.launch.clone()),
        vessel_ref: Some(held.vessel.clone()),
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
        // A convoy always belongs to a declared Project.
        let (crew, backend, _probe, held) = project_fixture(CrewWorkPhase::Working).await;
        for n in 0..unrelated {
            let name = format!("unrelated-{n}");
            held.engine
                .admit(
                    &name,
                    ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("workflow".into()).role("other".into()).build(),
                    flotilla_engine_convoys_testkit::single_agent("coder"),
                )
                .await;
            let unrelated = flotilla_engine_messages_testkit::intent(
                "flotilla/other",
                "flotilla/other-convoy/work/coder",
                MessageRelation::Supervisor,
                "other",
            );
            held.messages().primitives().admit(&name, &unrelated).await.expect("unrelated message");
            held.engine.launch_session(&TerminalLaunchRequest {
                session: format!("terminal-{n}"),
                environment: "other".into(),
                vessel: None,
                role: "coder".into(),
                pool: "other".into(),
                cwd: "/other".into(),
                env: Default::default(),
                source: TerminalLaunchSource::Tool { command: "sh".into() },
            });
            backend
                .using::<flotilla_resources::Artifact>("flotilla")
                .create(
                    &InputMeta::builder().name(format!("artifact-{n}")).build(),
                    &flotilla_resources::ArtifactSpec::builder()
                        .convoy("other".into())
                        .producer("other".into())
                        .kind("explainer".into())
                        .subject("other".into())
                        .digest(flotilla_resources::artifact::blob_digest_of(b"unrelated"))
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
        let context = command_context(&held);
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
                    Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
                )
                .await
                .expect("completion");
            }
            "teardown" => {
                crew.teardown("flotilla", &held.launch, false).await.expect("teardown");
            }
            "withdraw" => {
                assert_eq!(crew.withdraw_pending_brief("flotilla", &held.launch).await.expect("withdraw"), None);
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
            // The open credential attentions about the vessel: the convoy, for the inbox its
            // attentions go to, and the FleetDesignation naming the roles that send them. The
            // convoy's Project is read twice: for the orientation and for that inbox.
            ReadBudget::new()
                .objects("Convoy", Get, 2, 2)
                .calls("Demand", Query, 1)
                .objects("FleetDesignation", Get, 2, 1)
                .objects("Project", Get, 4, 2)
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
        // Withdrawal on the engine reads the queued message from the engine, and the fleet roles
        // its withdrawal is sent as.
        ("withdraw", ReadBudget::new().objects("Convoy", Get, 4, 3).objects("FleetDesignation", Get, 2, 1)),
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
    let (crew, backend, _probe, held) = fixture(CrewWorkPhase::Working).await;
    let launch = held.launch.as_str();
    let name = flotilla_resources::address::record_name(
        &flotilla_resources::address::artifact_address(launch, "coder", "decision-ledger", launch).expect("artifact address"),
    )
    .expect("record name");
    let local = backend
        .using::<flotilla_resources::Artifact>("flotilla")
        .create(
            &InputMeta::builder().name(name).build(),
            &flotilla_resources::ArtifactSpec::builder()
                .convoy(launch.into())
                .producer("coder".into())
                .kind("decision-ledger".into())
                .subject(launch.into())
                .digest(flotilla_resources::artifact::blob_digest_of(b"local"))
                .size(0)
                .media_type("text/markdown".into())
                .expires_at(chrono::Utc::now() + chrono::Duration::days(1))
                .build(),
        )
        .await
        .expect("local artifact");
    let mut replica = local;
    replica.spec.digest = flotilla_resources::artifact::blob_digest_of(b"replica");
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
        &command_context(&held),
        None,
        None,
        Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
    )
    .await
    .expect("complete");
    let convoy = held.convoy(&backend).await;
    assert!(convoy.status.expect("status").crew_work["work"]["coder"].decision_ledger_digest.is_none());
}

// Inspection boundary fake: explicit deletion obtains newly observed checkout facts, written to
// the engine as an inspection of the checkout's home writes them.
struct FreshDeleteInspection(std::sync::atomic::AtomicUsize, Arc<flotilla_provisioning::engine::checkouts::EngineCheckouts>);
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
            .inspected(&checkout.name, Inspection { clean: Some(true), pushed: Some(true), landed, landed_evidence, ..Default::default() })
            .map(|_| ())
    }
}

/// An Active convoy that adopted checkout `checkout`, held for its launch, realised and never
/// inspected; returns the checkout's launch.
async fn active_delete_fixture() -> (Arc<CrewService>, ResourceBackend, String, Held) {
    let spec = ConvoySpec::builder()
        .project_ref("flotilla".into())
        .workflow_ref("workflow".into())
        .role("crew".into())
        .adopted_checkout_refs(BTreeMap::from([(flotilla_protocol::RepositoryKey("repo".into()), "checkout".into())]))
        .build();
    let (crew, backend, _, held) = spec_fixture(CrewWorkPhase::Working, spec).await;
    assert_eq!(held.convoy(&backend).await.status.expect("status").phase, ConvoyPhase::Active);
    let launch = held.engine.engine.checkouts.put_ready(
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
        Some(&CheckoutHolder::Convoy(held.launch.clone())),
    );
    (crew, backend, launch, held)
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
        let (crew, _backend, launch, held) = active_delete_fixture().await;
        if configured {
            crew.set_checkout_inspector(Arc::new(FailedDeleteInspection));
        }
        let error = crew.teardown("flotilla", &held.launch, false).await.expect_err("inspection is required");
        assert!(
            error.contains(if configured {
                "fresh inspection failed for checkout checkout: VCS temporarily unavailable"
            } else {
                "inspector not configured"
            }),
            "{error}"
        );
        let checkout = held.engine.engine.checkouts.record(&launch);
        assert_eq!(checkout.inspection(), None, "no inspection is written for a failed one");
        assert!(checkout.is_ready(), "the checkout is preserved");
    }
}

// R4: landing evidence is judged by when its checkout was inspected, not when it is read. The gate
// refuses an inspection older than the evidence's lifetime; an operator's delete of an ended convoy
// asks for such a checkout afresh, once, and reads fresh evidence as it is.
#[tokio::test]
async fn landing_evidence_is_as_old_as_its_inspection() {
    let (crew, backend, launch, held) = active_delete_fixture().await;
    let inspector = Arc::new(FreshDeleteInspection(0.into(), Arc::clone(&held.engine.engine.checkouts.checkouts)));
    let inspections = || inspector.0.load(std::sync::atomic::Ordering::SeqCst);
    held.engine
        .engine
        .checkouts
        .checkouts
        .inspected(
            &launch,
            Inspection {
                landed: Some(true),
                landed_evidence: Some(Landed {
                    change_request: 42,
                    merged_at: Some(Utc::now()),
                    target_ref: None,
                    head_in_merged_head: true,
                }),
                ..Default::default()
            },
        )
        .expect("inspection");
    held.engine.end(&backend, &held.launch, ConvoyPhase::Failed).await;
    crew.set_checkout_inspector(inspector.clone());
    let convoy = held.convoy(&backend).await;
    let checkouts = crew.held_checkouts(&convoy).await.expect("held");

    crew.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkouts, false).await.expect("fresh evidence");
    crew.teardown("flotilla", &held.launch, false).await.expect("fresh evidence");
    assert_eq!(inspections(), 0, "fresh evidence is read as it is");

    held.clock.advance(31);
    let error = crew.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkouts, false).await.expect_err("stale evidence");
    assert!(error.contains("checkout: Landed evidence is missing or stale"), "{error}");
    assert_eq!(inspections(), 0, "the gate asks for no inspection");
    crew.teardown("flotilla", &held.launch, false).await.expect("inspected afresh");
    assert_eq!(inspections(), 1, "the delete asks for the stale checkout once");
}

// A convoy that expects no change request judges its checkouts without reading a forge, so a
// repository whose remote no Forge declares (the fleet canary's `file://` upstream) still has its
// teardown sanctioned once its checkout is released.
#[tokio::test]
async fn a_forge_free_repository_needs_no_forge_to_tear_down() {
    let spec = ConvoySpec::builder()
        .project_ref("flotilla".into())
        .workflow_ref("workflow".into())
        .role("crew".into())
        .repositories(vec![flotilla_resources::ConvoyRepositorySpec {
            url: "file:///tmp/canary/upstream.git".into(),
            repo_ref: flotilla_protocol::RepositoryKey("repo".into()),
            source_ref: "main".into(),
            target_ref: "main".into(),
            workspace_slug: "upstream".into(),
            subpaths: Vec::new(),
        }])
        .adopted_checkout_refs(BTreeMap::from([(flotilla_protocol::RepositoryKey("repo".into()), "checkout".into())]))
        .build();
    let (crew, backend, _, held) = spec_fixture(CrewWorkPhase::Done, spec).await;
    let launch = held.engine.engine.checkouts.put_ready(
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
        Some(&CheckoutHolder::Convoy(held.launch.clone())),
    );
    held.engine.end(&backend, &held.launch, ConvoyPhase::Landed).await;
    held.engine.engine.checkouts.finish(&launch, flotilla_provisioning_testkit::Ending::Released);
    let convoy = held.convoy(&backend).await;
    let checkouts = crew.held_checkouts(&convoy).await.expect("held");

    crew.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkouts, false).await.expect("no forge is needed");
}

// #3710: each manual Active-convoy delete forces one fresh checkout inspection,
// including a second request inside the normal evidence freshness horizon.
#[tokio::test]
async fn manual_active_convoy_delete_inspects_checkout_once_per_request() {
    let (crew, _backend, launch, held) = active_delete_fixture().await;
    let convoy = held.launch.as_str();
    assert!(crew.teardown("flotilla", convoy, false).await.expect_err("missing inspector").contains("inspector not configured"));
    let checkouts = &held.engine.engine.checkouts;
    let inspector = Arc::new(FreshDeleteInspection(0.into(), Arc::clone(&checkouts.checkouts)));
    crew.set_checkout_inspector(inspector.clone());
    crew.teardown("flotilla", convoy, false).await.expect("fresh delete evidence");
    crew.teardown("flotilla", convoy, false).await.expect("second explicit inspection");
    assert_eq!(inspector.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(checkouts.record(&launch).inspection().and_then(|inspection| inspection.landed), Some(true));
}
