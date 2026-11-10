//! Teardown scenarios using the shared daemon harness.

use super::*;
use flotilla_resources::attentions::{Attention, Fact};

pub async fn refused_convoy_reclaim_leaves_runtime_children_untouched(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let repository = RepositoryKey("github.com-acme-standing".to_string());
    let convoy_name = "failed-before-checkout";
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &test_meta(convoy_name),
            &ConvoySpec::builder()
                .workflow_ref("quartermaster".to_string())
                .adopted_checkout_refs(BTreeMap::from([(repository, "checkout-never-provisioned".to_string())]))
                .build(),
        )
        .await
        .expect("convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
        )
        .await
        .expect("failed convoy");
    let launch = engine(&daemon).vessels.request(convoy_name, "work", "local");

    let refusal = daemon.reap_convoy_internal("flotilla", convoy_name, false).await.expect_err("unsafe reclaim must be refused");

    assert!(refusal.contains("missing checkout integration evidence"));
    assert!(convoys.get(convoy_name).await.is_ok(), "refusal must retain the convoy");
    {
        use flotilla_resources::vessel_records::VesselRecords;
        let held = engine(&daemon).vessels.vessels.get(&launch, None).expect("read").expect("launch");
        assert!(!held.ended(), "refusal must retain the vessel");
    }

    let principal = PrincipalRef::implicit_for_namespace("flotilla");
    daemon
        .abandon_convoy_internal("flotilla", convoy_name, "operator accepts the unprovisioned checkout", Some(&principal))
        .await
        .expect("the refused shape must remain recoverable through convoy abandon");
    let abandoned = convoys.get(convoy_name).await.expect("abandon retains the convoy record");
    assert_eq!(abandoned.status.expect("abandoned status").phase, ConvoyPhase::Abandoned);
}

pub async fn concurrent_convoy_phase_change_prevents_operator_abandonment(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(&test_meta("abandon-race"), &ConvoySpec::builder().workflow_ref("workflow".to_string()).build())
        .await
        .expect("convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Active, ..Default::default() },
        )
        .await
        .expect("active convoy");

    let racing_convoys = convoys.clone();
    let error = daemon
        .abandon_convoy_internal_with_hook("flotilla", "abandon-race", "operator abandons", None, || async move {
            let current = racing_convoys.get("abandon-race").await.expect("convoy before concurrent completion");
            let mut status = current.status.expect("active status");
            status.phase = ConvoyPhase::Landed;
            status.message = Some("concurrent landing won".to_string());
            racing_convoys.update_status("abandon-race", &current.metadata.resource_version, &status).await.expect("concurrent completion");
        })
        .await
        .expect_err("phase change must reject abandonment");

    assert_eq!(error, "convoy phase changed while abandonment was being applied; retry the command");
    let current = convoys.get("abandon-race").await.expect("convoy after race");
    let status = current.status.expect("status");
    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert_eq!(status.message.as_deref(), Some("concurrent landing won"));
}

pub async fn gone_worktree_satisfies_teardown_gate_without_integration_observation(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &test_meta("gone-worktree"),
            &ConvoySpec::builder()
                .workflow_ref("quartermaster".to_string())
                .adopted_checkout_refs(BTreeMap::from([(RepositoryKey("repo".to_string()), "checkout-gone".to_string())]))
                .build(),
        )
        .await
        .expect("convoy");
    let convoy = convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
        )
        .await
        .expect("failed convoy");
    // The checkout's latest launch as the engine reads it: its worktree observed gone, with no
    // inspection written.
    let gone = flotilla_resources::checkout_records::CheckoutRecord {
        name: "checkout-gone#1".to_string(),
        home: "test".to_string(),
        spec: ResourceCheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
            repo_ref: RepositoryKey("repo".to_string()),
            env_ref: "host-direct-test".to_string(),
            r#ref: "feature/gone".to_string(),
            base_ref: Some("main".to_string()),
            target_path: "/missing/worktree".to_string(),
            clone_ref: "clone".to_string(),
        }),
        holder: Some(flotilla_resources::checkout_records::CheckoutHolder::Convoy("gone-worktree".to_string())),
        requested_at: chrono::Utc::now(),
        reading: flotilla_protocol::LifecycleReading::Lost { reason: None },
        degraded: None,
        integration: None,
    };

    daemon
        .verify_convoy_teardown_gate_for_checkouts(&convoy, std::slice::from_ref(&gone), false)
        .await
        .expect("host-confirmed Gone checkout is safe to tear down");
    // Still realised, with no inspection: its integration is unknown.
    let unknown = flotilla_resources::checkout_records::CheckoutRecord {
        reading: flotilla_protocol::LifecycleReading::Ready { identity: "/missing/worktree".to_string() },
        ..gone
    };
    assert!(daemon.verify_convoy_teardown_gate_for_checkouts(&convoy, &[unknown], false).await.is_err());
}

pub async fn reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let failed_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("live generation");
    let failed = convoys.get(&failed_ref).await.expect("generation");
    convoys
        .update_status(
            &failed_ref,
            &failed.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                provisioning: Some(ConvoyProvisioningState::Started { started_at: clock.now() }),
                message: Some("clone failed before the work environment was provisioned".to_string()),
                finished_at: Some(clock.now()),
                ..Default::default()
            },
        )
        .await
        .expect("record clone-failed generation");

    assert_eq!(
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &RecordlessBacking)
            .await
            .expect("automatic reconcile holds when bypass deletion removed the backing records"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );

    let outcome = daemon
        .reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
        .await
        .expect("reconcile-now acknowledges the record wipe");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    assert!(generations.iter().any(|convoy| convoy.metadata.name == failed_ref && convoy.spec.generation == 1));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));
}

pub async fn reconcile_now_waits_for_periodic_backing_inspection_and_keeps_one_generation(factory: &dyn EnsureScenarioController) {
    struct PausedBacking {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    #[async_trait]
    impl StandingConvoyBackingInspector for PausedBacking {
        async fn verify_backing_dead(&self, _convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
            self.entered.notify_one();
            self.release.notified().await;
            Err("backing is still live".to_string())
        }
    }

    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    fail_ensured_generation(&backend, &clock).await;
    let backing = Arc::new(PausedBacking { entered: Default::default(), release: Default::default() });
    let periodic = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        let backing = Arc::clone(&backing);
        async move { daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &*backing).await }
    });
    backing.entered.notified().await;
    // Rebuilding runtime around this daemon must retain the in-flight guard.
    daemon.install_convoy_ensure_reconciler(factory.create(daemon.convoy_teardown(), backend.clone(), clock.clone())).await;

    let mut forced = Box::pin(daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut forced).await.is_err(),
        "explicit reconciliation must wait for the periodic transaction"
    );
    backing.release.notify_one();
    periodic.await.expect("periodic task").expect("record backing hold");
    forced.await.expect("forced recovery");
    daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking).await.expect("idempotent forced pass");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("subsequent periodic pass");

    let convoys = backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations");
    assert_eq!(convoys.items.len(), 2, "one failed generation and one replacement");
    let live =
        convoys.items.iter().filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())).collect::<Vec<_>>();
    assert_eq!(live.len(), 1);
    let status = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.convoy_ref.as_deref(), Some(live[0].metadata.name.as_str()));
    assert_eq!(status.last_failure, None, "a stale periodic pass must not overwrite the forced recovery");
    assert_eq!(status.retry_at, None);
}

pub async fn driver_reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure(factory: &dyn EnsureScenarioController) {
    let (driver, backend, clock, _temp) = standing_ensure_fixture_for(factory, "udder", true).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&backend, &driver_id).await;
    // The public pass resolves the declared driver before exercising admission.
    backend.using::<ResourceHost>("flotilla").create(&test_meta(&driver_id), &HostSpec::default()).await.expect("driver host");

    driver.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &RecordlessBacking).await.expect("initial driver admission");
    fail_latest_ensured_generation(&backend, &clock).await;

    let refusal = driver
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &RecordlessBacking)
        .await
        .expect_err("automatic driver reconcile must hold on missing backing evidence");
    assert!(refusal.contains("no backing environment evidence is available"), "unexpected refusal: {refusal}");

    let outcome = driver
        .reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
        .await
        .expect("operator acknowledges the driver generation record wipe");

    assert_eq!(outcome, "started quartermaster@standing-project");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("standing generations").items.len(), 2);
}

pub async fn standing_backing_inspection_holds_empty_evidence_after_provisioning_started(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("standing convoy");
    let convoy = convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                provisioning: Some(ConvoyProvisioningState::Started { started_at: clock.now() }),
                finished_at: Some(clock.now()),
                ..Default::default()
            },
        )
        .await
        .expect("record post-provisioning failure");

    let refusal =
        daemon.verify_backing_dead(&convoy).await.expect_err("missing backing evidence after provisioning must remain conservative");

    assert_eq!(refusal, "no backing environment evidence is available");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold without backing evidence"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );
    let status = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.expect("ensure status");
    assert!(
        status.last_failure.as_deref().is_some_and(|failure| failure.contains("no backing environment evidence is available")),
        "{status:?}"
    );
}

pub async fn standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let first = convoys.get(&first_ref).await.expect("standing convoy");
    let now = clock.now();
    convoys
        .update_status(
            &first.metadata.name,
            &first.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                message: Some("provider registry unavailable".to_string()),
                started_at: Some(now),
                finished_at: Some(now),
                observed_workflow_ref: Some("quartermaster".to_string()),
                // The workflow its convoy controller froze when it started: its vessel names the
                // environment held for it.
                workflow_snapshot: Some(flotilla_resources::WorkflowSnapshot {
                    cascade: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    stall_nudges: Default::default(),
                    supervision: None,
                    vessels: vec![VesselRequirement::builder().name("work".to_string()).crew(Vec::new()).build()],
                }),
                ..first.status.clone().unwrap_or_default()
            },
        )
        .await
        .expect("fail convoy after resolution loss");
    // Its live backing environment, held on the engine for the work's launch.
    let environments = engine(&daemon);
    let vessel = environments.vessels.request(&first_ref, "work", "local");
    environments.environments.put_ready(
        "quartermaster-work",
        &ResourceEnvironmentSpec {
            cloud_sandbox: None,
            provider_address: None,
            host_direct: None,
            docker: Some(flotilla_resources::DockerEnvironmentSpec {
                image_build_ref: None,
                host_ref: "local".to_string(),
                image: "standing:latest".to_string(),
                declared_agent_adapters: BTreeSet::new(),
                required_agent_adapters: BTreeSet::new(),
                pull_policy: Default::default(),
                mounts: Vec::new(),
                env: BTreeMap::new(),
            }),
        },
        Some(&vessel),
        "live-container",
        Default::default(),
    );

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold live backing"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );
    clock.advance(ChronoDuration::hours(1));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("continue holding").is_empty());
    assert!(convoys.get(&first_ref).await.is_ok(), "failed convoy and its live container must survive");
    let held = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("held ensure");
    assert_eq!(held.status.as_ref().expect("status").restart_count, 0);
    assert!(
        held.status.as_ref().expect("status").last_failure.as_deref().is_some_and(|failure| failure.contains("not verified dead")),
        "unexpected ensure status: {:?}",
        held.status
    );
    assert_eq!(backend.using::<ResourceDemand>("flotilla").list().await.expect("attention demands").items.len(), 1);

    // Its container stopped: on the engine the environment vanished, failed.
    environments
        .environments
        .finish("quartermaster-work", flotilla_provisioning_testkit::Ending::Failed("Docker container live-container is not running"));
    // ConvoyEnsure B: the crash backoff runs from the generation's end an hour ago, so the
    // replacement starts on the pass that sees the backing dead.
    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("restart dead backing"),
        vec!["started quartermaster@standing-project"]
    );
    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    assert!(generations.iter().any(|convoy| convoy.metadata.name == first_ref && convoy.spec.generation == 1));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2));
    assert_eq!(backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 1);
}

pub async fn convoy_teardown_removes_its_managed_children(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("convoy ref");
    // Convoy teardown ends its managed children: its vessel's launch is released by the vessel
    // requester once the convoy is gone; its sessions are the engine's, released once it ends.
    let launch = engine(&daemon).vessels.request(&convoy_ref, "work", "local");
    // Attentions about the convoy, its vessel and its role, every one acknowledged, end with them.
    let attentions = backend.attentions();
    let now = chrono::Utc::now();
    for attention in [
        Attention::new(Fact::Hold { ensure: "quartermaster".into(), convoy: convoy_ref.clone() }, "held", now),
        Attention::credential_refresh(&convoy_ref, "work", BTreeMap::from([("github".into(), "expired".into())]), now),
        Attention::new(
            Fact::DeliveryHeld { session: "quartermaster-work".into(), convoy: convoy_ref.clone(), role: "tool".into() },
            "held",
            now,
        ),
    ] {
        attentions.raise("flotilla", &attention).await.expect("raise");
    }
    let demands = backend.using::<ResourceDemand>("flotilla");
    let raised = demands.list().await.expect("demands").items;
    assert_eq!(raised.len(), 3, "one demand per attention");
    for demand in raised {
        let patch = flotilla_resources::DemandStatusPatch::Acknowledge { as_of: chrono::Utc::now(), authority: "person".into() };
        flotilla_store::apply_status_patch(&demands, &demand.metadata.name, &patch).await.expect("acknowledge");
    }

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("convoy teardown");

    assert!(demands.list().await.expect("demands").items.is_empty(), "no attention is left about the reaped convoy");

    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await, Err(ResourceError::NotFound { .. })));
    let requests = flotilla_admission::reconcilers::vessel_requests::VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(backend.clone(), "flotilla")),
        daemon.lifecycle_reads(),
        std::sync::Arc::new(NoGate),
        flotilla_protocol::CanonicalHostId::resolved("local"),
        [],
    );
    let pass = requests.sync_once(chrono::Utc::now()).await.expect("vessel requests");
    assert_eq!(pass.released, vec![launch], "a reaped convoy's vessel is released");
}

/// A gate never asked: a convoy that is gone sanctions its own reclaim.
struct NoGate;

#[async_trait::async_trait]
impl flotilla_admission::reconcilers::vessel_requests::VesselReleaseGate for NoGate {
    async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
        panic!("convoy {} is gone; its reclaim needs no gate", convoy)
    }
}

pub async fn forced_convoy_delete_retains_force_intent_until_checkout_finalizes(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .and_then(|status| status.convoy_ref)
        .expect("convoy ref");
    // A checkout the convoy holds on the engine, made for it and not yet removed.
    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("standing convoy");
    let engine_convoy = flotilla_resources::convoy::engine::engine_name(&convoy).expect("engine convoy name");
    // Named on the convoy's status as admission names it: a checkout is held for that launch.
    engine(&daemon).checkouts.admit_for(&backend, "flotilla", &engine_convoy, &convoy_ref).await;
    let holder = engine(&daemon).checkouts.holder(&engine_convoy);
    engine(&daemon).checkouts.put_ready(
        "checkout-at-risk",
        &ResourceCheckoutSpec::Worktree(
            flotilla_resources::CheckoutWorktreeSpec::builder()
                .repo_ref(RepositoryKey("repo-a".into()))
                .env_ref("host-direct-host-test".into())
                .r#ref("feature/work".into())
                .target_path("/tmp/checkout-at-risk".into())
                .clone_ref("clone".into())
                .build(),
        ),
        Some(&holder),
    );

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("force delete");

    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("convoy must await checkout");
    assert_eq!(convoy.metadata.annotations.get(flotilla_resources::FORCE_TEARDOWN_ANNOTATION).map(String::as_str), Some("true"));
    assert!(convoy.metadata.deletion_timestamp.is_some());
}
