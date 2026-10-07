//! Teardown scenarios using the shared daemon harness.

use super::*;

pub async fn refused_convoy_reclaim_leaves_runtime_children_untouched(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let repository = RepositoryKey("github.com-acme-standing".to_string());
    let convoy_name = "failed-before-checkout";
    let vessel_name = "failed-before-checkout-work";
    let terminal_name = "terminal-failed-before-checkout-work-coder";
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
    backend
        .clone()
        .using::<Vessel>("flotilla")
        .create(
            &InputMeta::builder()
                .name(vessel_name.to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
                .build(),
            &flotilla_resources::VesselSpec {
                convoy_ref: convoy_name.to_string(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "contained".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("vessel");
    backend
        .clone()
        .using::<ResourceTerminalSession>("flotilla")
        .create(
            &InputMeta::builder()
                .name(terminal_name.to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
                .build(),
            &ResourceTerminalSessionSpec {
                env_ref: "environment-still-live".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                env: Default::default(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal session");

    let refusal = daemon.reap_convoy_internal("flotilla", convoy_name, false).await.expect_err("unsafe reclaim must be refused");

    assert!(refusal.contains("missing checkout integration evidence"));
    assert!(convoys.get(convoy_name).await.is_ok(), "refusal must retain the convoy");
    assert!(backend.clone().using::<Vessel>("flotilla").get(vessel_name).await.is_ok(), "refusal must retain the vessel");
    assert!(
        backend.clone().using::<ResourceTerminalSession>("flotilla").get(terminal_name).await.is_ok(),
        "refusal must retain the terminal session"
    );

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
    let checkouts = backend.using::<ResourceCheckout>("flotilla");
    let created_checkout = checkouts
        .create(
            &test_meta("checkout-gone"),
            &ResourceCheckoutSpec::Worktree(flotilla_resources::CheckoutWorktreeSpec {
                repo_ref: RepositoryKey("repo".to_string()),
                env_ref: "host-direct-test".to_string(),
                r#ref: "feature/gone".to_string(),
                base_ref: Some("main".to_string()),
                target_path: "/missing/worktree".to_string(),
                clone_ref: "clone".to_string(),
            }),
        )
        .await
        .expect("checkout");
    let gone = checkouts
        .update_status(
            "checkout-gone",
            &created_checkout.metadata.resource_version,
            &ResourceCheckoutStatus {
                phase: flotilla_resources::CheckoutPhase::Gone,
                path: Some("/missing/worktree".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("gone checkout");

    daemon
        .verify_convoy_teardown_gate_for_checkouts(&convoy, &[gone], false)
        .await
        .expect("host-confirmed Gone checkout is safe to tear down");
    let mut unknown = checkouts.get("checkout-gone").await.expect("checkout");
    unknown.status = None;
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
    daemon.install_convoy_ensure_reconciler(factory.create(backend.clone(), clock.clone())).await;

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

    let refusal = daemon
        .verify_standing_convoy_resource_backing_dead(&convoy)
        .await
        .expect_err("missing backing evidence after provisioning must remain conservative");

    assert_eq!(refusal, "no backing environment evidence is available");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("hold without backing evidence"),
        vec!["ConvoyEnsure/quartermaster held for operator attention"]
    );
    let events = backend.using::<Event>("flotilla").list().await.expect("list object events").items;
    assert!(events.iter().any(|event| {
        event.spec.regarding.name == convoy_ref
            && event.spec.reason == "BackingEvidenceRefused"
            && event.spec.message.contains("no backing environment evidence is available")
    }));
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
                ..Default::default()
            },
        )
        .await
        .expect("fail convoy after resolution loss");
    let environments = backend.using::<ResourceEnvironment>("flotilla");
    let environment = environments
        .create(
            &InputMeta::builder()
                .name("quartermaster-work".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), first_ref.clone())]))
                .build(),
            &ResourceEnvironmentSpec {
                host_direct: None,
                docker: Some(flotilla_resources::DockerEnvironmentSpec {
                    image_composition: None,
                    image_build_ref: None,
                    memory_policy: Default::default(),
                    host_ref: "local".to_string(),
                    image: "standing:latest".to_string(),
                    declared_agent_adapters: BTreeSet::new(),
                    required_agent_adapters: BTreeSet::new(),
                    pull_policy: Default::default(),
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                }),
            },
        )
        .await
        .expect("backing environment");
    environments
        .update_status(
            &environment.metadata.name,
            &environment.metadata.resource_version,
            &ResourceEnvironmentStatus {
                phase: EnvironmentPhase::Ready,
                ready: true,
                docker_container_id: Some("live-container".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("mark backing live");

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

    let environment = environments.get("quartermaster-work").await.expect("backing environment");
    environments
        .update_status(
            &environment.metadata.name,
            &environment.metadata.resource_version,
            &ResourceEnvironmentStatus {
                phase: EnvironmentPhase::Failed,
                ready: false,
                message: Some("Docker container live-container is not running".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("verify backing dead");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("record crash backoff");
    clock.advance(ChronoDuration::seconds(30));
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

pub async fn convoy_teardown_removes_its_managed_presentations(factory: &dyn EnsureScenarioController) {
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
    let presentations = backend.using::<ResourcePresentation>("flotilla");
    presentations
        .create(
            &InputMeta::builder()
                .name("quartermaster-work".to_string())
                .labels(BTreeMap::from([
                    (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Managed.as_label_value().to_string()),
                    (CONVOY_LABEL.to_string(), convoy_ref.clone()),
                ]))
                .build(),
            &flotilla_resources::PresentationSpec {
                convoy_ref: convoy_ref.clone(),
                presentation_policy_ref: "default".to_string(),
                name: "quartermaster".to_string(),
                process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_ref.clone())]),
            },
        )
        .await
        .expect("presentation");

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("convoy teardown");

    assert!(matches!(presentations.get("quartermaster-work").await, Err(ResourceError::NotFound { .. })));
    assert!(matches!(backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await, Err(ResourceError::NotFound { .. })));
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
    backend
        .clone()
        .using::<ResourceCheckout>("flotilla")
        .create(
            &InputMeta::builder()
                .name("checkout-at-risk".to_string())
                .labels(BTreeMap::from([
                    (AUTHORITY_LABEL.to_string(), LifecycleAuthority::Managed.as_label_value().to_string()),
                    (CONVOY_LABEL.to_string(), convoy_ref.clone()),
                ]))
                .finalizers(vec!["flotilla.work/checkout-cleanup".to_string()])
                .build(),
            &ResourceCheckoutSpec::Observed(flotilla_resources::ObservedCheckoutSpec {
                r#ref: "feature/work".to_string(),
                path: "/tmp/checkout-at-risk".to_string(),
                repo_ref: RepositoryKey("repo-a".to_string()),
                host_ref: "host-test".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout");

    daemon.reap_convoy_internal("flotilla", &convoy_ref, true).await.expect("force delete");

    let convoy = backend.using::<ResourceConvoy>("flotilla").get(&convoy_ref).await.expect("convoy must await checkout");
    assert_eq!(convoy.metadata.annotations.get(flotilla_resources::FORCE_TEARDOWN_ANNOTATION).map(String::as_str), Some("true"));
    assert!(convoy.metadata.deletion_timestamp.is_some());
}
