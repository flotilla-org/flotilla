//! Teardown scenarios using the shared daemon harness.

use super::*;
use flotilla_resources::attentions::{Attention, Fact};

pub async fn refused_convoy_reclaim_leaves_runtime_children_untouched(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let repository = RepositoryKey("github.com-acme-standing".to_string());
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy_name = &launch_convoy(
        &daemon,
        "failed-before-checkout",
        ConvoySpec::builder()
            .project_ref("flotilla".into())
            .workflow_ref("quartermaster".to_string())
            .adopted_checkout_refs(BTreeMap::from([(repository, "checkout-never-provisioned".to_string())]))
            .build(),
        ConvoyPhase::Active,
    )
    .await;
    // Its work's vessel is requested while it runs, and holds it once it fails.
    let launch = engine(&daemon).vessels.request(convoy_name, "work", "local");
    fail_generation(&daemon, convoy_name, "failed").await;

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
    // On the engine an ended convoy is left as it is: the abandonment succeeds, and the convoy
    // stays the failure it ended as.
    let abandoned = convoys.get(convoy_name).await.expect("abandon retains the convoy record");
    assert_eq!(abandoned.status.expect("abandoned status").phase, ConvoyPhase::Failed);
}

pub async fn concurrent_convoy_phase_change_prevents_operator_abandonment(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let name = launch_convoy(
        &daemon,
        "abandon-race",
        ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("workflow".to_string()).build(),
        ConvoyPhase::Active,
    )
    .await;
    // A running convoy's work has its vessel, which holds it once it ends.
    engine(&daemon).vessels.request(&name, "work", "local");

    // On the engine the convoy's edges are serialized: a landing that wins the race ends it, and
    // the abandonment then finds nothing open to abandon.
    let racing = Arc::clone(&daemon);
    let racing_name = name.clone();
    daemon
        .abandon_convoy_internal_with_hook("flotilla", &name, "operator abandons", None, || async move {
            let current = racing.resource_backend().using::<ResourceConvoy>("flotilla").get(&racing_name).await.expect("convoy");
            let engine = convoy_engine(&racing);
            let scope = flotilla_resources::convoy::engine::VerdictScope { vessel: "work".to_string(), role: None };
            engine.keep_by_verdict(&current, &scope, "operator", "concurrent landing won").await.expect("keep the work");
            engine.deliver(&current, "claim").await.expect("concurrent landing");
        })
        .await
        .expect("an ended convoy is left as it is");

    let current = convoys.get(&name).await.expect("convoy after race");
    assert_eq!(current.status.expect("status").phase, ConvoyPhase::Landed);
}

pub async fn gone_worktree_satisfies_teardown_gate_without_integration_observation(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let name = launch_convoy(
        &daemon,
        "gone-worktree",
        ConvoySpec::builder()
            .project_ref("flotilla".into())
            .workflow_ref("quartermaster".to_string())
            .adopted_checkout_refs(BTreeMap::from([(RepositoryKey("repo".to_string()), "checkout-gone".to_string())]))
            .build(),
        ConvoyPhase::Failed,
    )
    .await;
    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&name).await.expect("failed convoy");
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
        holder: Some(flotilla_resources::checkout_records::CheckoutHolder::Convoy(name.clone())),
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
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let failed_ref = quartermaster(&daemon).await.convoy_ref.expect("live generation");
    let failed = convoys.get(&failed_ref).await.expect("generation");
    fail_generation(&daemon, &failed_ref, "clone failed before the work environment was provisioned").await;

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
    assert!(generations.iter().any(|convoy| convoy.metadata.name == failed_ref && convoy.spec.generation == failed.spec.generation));
    assert!(generations.iter().any(|convoy| convoy.spec.generation > failed.spec.generation));
    assert!(open_holds(&backend).await.is_empty());
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
    fail_ensured_generation(&daemon).await;
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
    let role = quartermaster(&daemon).await;
    assert_eq!(role.convoy_ref.as_deref(), Some(live[0].metadata.name.as_str()));
    assert_eq!(role.last_failure, None, "a stale periodic pass must not overwrite the forced recovery");
    assert_eq!(role.next_attempt, None);
    assert_eq!(role.hold, None);
}

pub async fn driver_reconcile_now_acknowledges_recordless_teardown_and_readmits_the_ensure(factory: &dyn EnsureScenarioController) {
    let (driver, backend, clock, _temp) = standing_ensure_fixture_for(factory, "udder", true).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&backend, &driver_id).await;
    // The public pass resolves the declared driver before exercising admission.
    backend.using::<ResourceHost>("flotilla").create(&test_meta(&driver_id), &HostSpec::default()).await.expect("driver host");

    driver.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &RecordlessBacking).await.expect("initial driver admission");
    fail_latest_ensured_generation(&driver, &clock).await;

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
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = quartermaster(&daemon).await.convoy_ref.expect("convoy ref");
    // Provisioning started: its work launched a vessel, which holds no environment yet.
    engine(&daemon).vessels.request(&convoy_ref, "work", "local");
    fail_generation(&daemon, &convoy_ref, "post-provisioning failure").await;
    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("standing convoy");

    let refusal =
        daemon.verify_backing_dead(&convoy).await.expect_err("missing backing evidence after provisioning must remain conservative");

    assert_eq!(refusal, "no backing environment evidence is available");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold without backing evidence"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );
    let held = quartermaster(&daemon).await;
    assert_eq!(held.hold, Some(StandingRoleHold::BackingUnverified));
    assert_eq!(held.convoy_ref.as_deref(), Some(convoy_ref.as_str()), "held on its ended generation");
    assert!(
        held.last_failure.as_deref().is_some_and(|failure| failure.contains("no backing environment evidence is available")),
        "{held:?}"
    );
}

pub async fn standing_ensure_holds_failed_convoy_while_backing_is_live_then_restarts_after_verified_death(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first_ref = quartermaster(&daemon).await.convoy_ref.expect("convoy ref");
    let first = convoys.get(&first_ref).await.expect("standing convoy");
    // Admission froze its workflow: its vessel, `work`, names the environment held for it.
    fail_generation(&daemon, &first_ref, "provider registry unavailable").await;
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
    let held = quartermaster(&daemon).await;
    // ConvoyEnsure B: the failed generation is its strike as soon as it ends.
    assert_eq!(held.strikes, 1);
    assert_eq!(held.hold, Some(StandingRoleHold::BackingUnverified));
    assert!(
        held.last_failure.as_deref().is_some_and(|failure| failure.contains("not verified dead")),
        "unexpected standing role: {held:?}"
    );
    assert_eq!(open_holds(&backend).await.len(), 1, "the live backing holds it for a person");

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
    assert!(generations.iter().any(|convoy| convoy.metadata.name == first_ref && convoy.spec.generation == first.spec.generation));
    let replacement = generations.iter().find(|convoy| convoy.spec.generation > first.spec.generation).expect("replacement");
    let running = quartermaster(&daemon).await;
    assert_eq!(running.convoy_ref.as_deref(), Some(replacement.metadata.name.as_str()));
    assert_eq!(running.hold, None);
}

pub async fn convoy_teardown_removes_its_managed_children(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = quartermaster(&daemon).await.convoy_ref.expect("convoy ref");
    // Convoy teardown ends the convoy and its managed children: its vessel's launch is released by
    // the vessel requester once the convoy has ended; its sessions are the engine's, released then too.
    let launch = engine(&daemon).vessels.request(&convoy_ref, "work", "local");
    // Attentions about the convoy, its vessel and its role end with them.
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
    assert_eq!(open_attentions(&backend).await.len(), 3, "one message per attention");

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("convoy teardown");

    assert!(open_attentions(&backend).await.is_empty(), "no attention is left about the reaped convoy");

    // The convoy ends and is never deleted (#3108).
    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("an ended convoy is kept");
    assert_eq!(convoy.status.expect("status").phase, ConvoyPhase::Abandoned);
    let gate = std::sync::Arc::new(Sanctioned::default());
    // The convoys as the daemon reads them, homed at its node, where its launches are held.
    let requests = flotilla_admission::reconcilers::vessel_requests::VesselRequests::new(
        std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::EngineConvoyWorks::new(daemon.lifecycle_reads(), "flotilla")),
        daemon.lifecycle_reads(),
        gate.clone(),
        flotilla_protocol::CanonicalHostId::resolved("local"),
        [],
    );
    let pass = requests.sync_once(chrono::Utc::now()).await.expect("vessel requests");
    assert_eq!(pass.released, vec![launch], "a reaped convoy's vessel is released");
    assert_eq!(*gate.asked.lock().expect("asked"), vec![convoy_ref.clone()], "an ended convoy's reclaim is sanctioned first");

    // Deleting it again has nothing left to end.
    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("a second teardown is a no-op");
    assert_eq!(
        backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("convoy").status.expect("status").phase,
        ConvoyPhase::Abandoned
    );
}

/// A gate that sanctions every reclaim, recording the convoys it was asked about.
#[derive(Default)]
struct Sanctioned {
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl flotilla_admission::reconcilers::vessel_requests::VesselReleaseGate for Sanctioned {
    async fn may_release(&self, convoy: &str, _convoy_home: &str) -> Result<(), String> {
        self.asked.lock().expect("asked").push(convoy.to_owned());
        Ok(())
    }
}

pub async fn forced_convoy_delete_ends_a_convoy_whose_checkout_is_at_risk(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = quartermaster(&daemon).await.convoy_ref.expect("convoy ref");
    // A checkout the convoy's launch holds on the engine, made for it and not yet removed.
    let holder = flotilla_resources::checkout_records::CheckoutHolder::Convoy(convoy_ref.clone());
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

    // Force skips the gate, which would first ask the checkout's home to inspect it afresh: the
    // convoy ends, and the checkout is released by its controller.
    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("force delete");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    assert_eq!(convoys.get(&convoy_ref).await.expect("an ended convoy is kept").status.expect("status").phase, ConvoyPhase::Abandoned);
}
