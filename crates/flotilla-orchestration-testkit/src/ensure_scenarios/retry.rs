//! Retry scenarios using the shared daemon harness.

use super::*;

pub async fn standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");

    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }

    fail_ensured_generation(&backend, &clock).await;
    assert_eq!(
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let held = ensures.get("quartermaster").await.expect("held ensure");
    assert_eq!(held.status.as_ref().expect("status").restart_count, 3);
    assert_eq!(held.status.as_ref().expect("status").hold_reason, Some(ConvoyEnsureHoldReason::RestartLimit));
    let demands = backend.using::<ResourceDemand>("flotilla");
    let demand = demands.get("ensure-attention-quartermaster").await.expect("restart escalation");
    assert!(demand.spec.expiry.is_some(), "restart exhaustion must carry an escalation deadline");

    clock.advance(ChronoDuration::hours(6));
    assert!(daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("remain held")
        .is_empty());
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 3);

    apply_resource_status_patch(
        &demands,
        "ensure-attention-quartermaster",
        &DemandStatusPatch::Acknowledge { as_of: clock.now(), authority: "operator".to_string() },
    )
    .await
    .expect("operator acknowledges escalation");
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("clear hold");
    assert!(
        matches!(demands.get("ensure-attention-quartermaster").await, Err(ResourceError::NotFound { .. })),
        "clearing a restart hold must retire its resolved demand before another hold can reuse the name"
    );
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("schedule fresh episode");
    clock.advance(ChronoDuration::seconds(30));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("resume admission");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

pub async fn reconcile_now_resets_backoff_and_admits_the_next_ensure_generation_immediately(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record backoff");
    let backed_off = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("backed-off ensure");
    assert_eq!(backed_off.status.as_ref().expect("status").restart_count, 1);
    assert!(backed_off.status.as_ref().expect("status").retry_at.is_some());

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced admission");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let reconciled = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("reconciled ensure");
    let status = reconciled.status.expect("status");
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.retry_at, None);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 2);
}

pub async fn reconcile_now_clears_an_active_restart_limit_and_admits_in_one_pass(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record failure");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("restart");
    }
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust restart budget");

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced restart");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let status = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.restart_count, 0);
    assert_eq!(status.retry_at, None);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));
}

pub async fn concurrent_periodic_and_explicit_ensure_admission_creates_only_one_live_generation(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let (left, right) = tokio::join!(
        daemon.reconcile_convoy_ensures_once("flotilla"),
        daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
    );
    left.expect("left reconcile");
    right.expect("right reconcile");

    let live = backend
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("convoys")
        .items
        .into_iter()
        .filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
        .collect::<Vec<_>>();
    assert_eq!(live.len(), 1);
    assert_eq!(
        backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure").status.unwrap().convoy_ref,
        Some(live[0].metadata.name.clone())
    );
}

pub async fn declared_driver_derives_bounded_backoff_from_its_homed_generations(factory: &dyn EnsureScenarioController) {
    let (authority, authority_backend, _authority_clock, _authority_temp) = standing_ensure_fixture_for(factory, "kiwi", true).await;
    let (driver, driver_backend, driver_clock, _driver_temp) = standing_ensure_fixture_for(factory, "udder", false).await;
    let (other, other_backend, _other_clock, _other_temp) = standing_ensure_fixture_for(factory, "feta", false).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&authority_backend, &driver_id).await;

    let authority_root = authority.node_id().clone();
    for backend in [&driver_backend, &other_backend] {
        backend
            .replica_writer::<Project>(authority_root.clone(), "flotilla")
            .replace(&authority_backend.using::<Project>("flotilla").list().await.expect("authority projects"), Utc::now())
            .await
            .expect("replicate projects");
        backend
            .replica_writer::<ConvoyEnsure>(authority_root.clone(), "flotilla")
            .replace(&authority_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("authority ensures"), Utc::now())
            .await
            .expect("replicate ensures");
    }

    let host_origin = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(driver.node_id().clone());
    let hosts = host_origin.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(&driver_id),
            &HostSpec { display_name: "udder".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("driver host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus { ready: true, ..Default::default() })
        .await
        .expect("ready driver host");
    let host_snapshot = hosts.list().await.expect("driver host snapshot");
    for backend in [&authority_backend, &driver_backend, &other_backend] {
        backend
            .replica_writer::<ResourceHost>(driver.node_id().clone(), "flotilla")
            .replace(&host_snapshot, Utc::now())
            .await
            .expect("replicate driver host");
    }

    assert!(authority.reconcile_convoy_ensures_once("flotilla").await.expect("authority skip").is_empty());
    assert_eq!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("driver admission").len(), 1);
    assert!(other.reconcile_convoy_ensures_once("flotilla").await.expect("non-driver skip").is_empty());
    assert!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("steady-state driver pass").is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys").items.len(), 1);
    assert!(authority_backend.using::<ResourceConvoy>("flotilla").list().await.expect("authority convoys").items.is_empty());
    assert!(other_backend.using::<ResourceConvoy>("flotilla").list().await.expect("other convoys").items.is_empty());
    assert!(driver_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("driver local ensures").items.is_empty());

    let reaped = fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
    assert!(driver
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("failed generation starts backoff")
        .is_empty());
    driver_backend.using::<ResourceConvoy>("flotilla").delete(&reaped).await.expect("operator reaps failed husk");
    assert_eq!(
        driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("reaping resets derived failure count")
            .len(),
        1
    );

    for delay in [30, 60] {
        fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
        assert!(driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("backoff pass")
            .is_empty());
        driver_clock.advance(ChronoDuration::seconds(delay - 1));
        assert!(driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("retry not yet due")
            .is_empty());
        driver_clock.advance(ChronoDuration::seconds(1));
        assert_eq!(
            driver
                .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
                .await
                .expect("admit replacement")
                .len(),
            1
        );
    }

    fail_latest_ensured_generation(&driver_backend, &driver_clock).await;
    assert_eq!(
        driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("escalate bounded failures"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    let demands = driver_backend.using::<ResourceDemand>("flotilla");
    let demand = demands.get("ensure-attention-quartermaster").await.expect("driver-homed escalation");
    assert!(demand.spec.expiry.is_some());
    driver_clock.advance(ChronoDuration::hours(1));
    assert!(driver
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("unresolved escalation blocks admission")
        .is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("bounded generations").items.len(), 3);

    assert_eq!(
        driver
            .reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking)
            .await
            .expect("forced reconcile bypasses active driver escalation"),
        "started quartermaster@standing-project".to_string()
    );
    assert!(matches!(demands.get("ensure-attention-quartermaster").await, Err(ResourceError::NotFound { .. })));
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("resumed generations").items.len(), 4);
    assert!(
        authority_backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("authority ensure").status.is_none(),
        "driver admission must not persist control state on its ensure definition"
    );
}

pub async fn declared_driver_admission_refusals_retry_indefinitely_without_strikes_or_human_gate(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    let driver_id = daemon.local_host_id().expect("driver host identity").to_string();
    let hosts = backend.using::<ResourceHost>("flotilla");
    let host = hosts
        .create(
            &test_meta(&driver_id),
            &HostSpec { display_name: "local".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("driver host");
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus { ready: true, ..Default::default() })
        .await
        .expect("ready driver host");
    set_ensure_driver(&backend, &driver_id).await;
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let mut spec = ensure.spec.clone();
    spec.workflow_ref = "missing-workflow".to_string();
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &spec).await.expect("make admission fail");
    let ensure = ensures.get("quartermaster").await.expect("updated ensure");
    ensures
        .update_status(
            &ensure.metadata.name,
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus {
                convoy_ref: Some("stale-convoy".to_string()),
                running_since: Some(clock.now() - ChronoDuration::days(1)),
                ..Default::default()
            },
        )
        .await
        .expect("seed stale pre-driver status");

    let first = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("first admission refusal");
    assert!(first.contains("retry at"), "{first}");
    let retrying_ensure = ensures.get("quartermaster").await.expect("ensure");
    let retry_resource_version = retrying_ensure.metadata.resource_version.clone();
    let status = retrying_ensure.status.expect("driver-managed status");
    assert_eq!(status.convoy_ref, None);
    assert_eq!(status.running_since, None);
    assert_eq!(status.restart_count, 0);
    assert!(status.retry_at.is_some());

    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("backoff suppresses retry").is_empty());
    assert_eq!(
        ensures.get("quartermaster").await.expect("unchanged ensure").metadata.resource_version,
        retry_resource_version,
        "a driver retry deadline must not cause a no-op legacy-status write"
    );
    clock.advance(ChronoDuration::seconds(30));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("second admission refusal").contains("retry at"));
    // #2875: read-only refusals share the provisioning cadence and cap while
    // remaining independent of the runtime strike budget.
    for expected_delay in [60, 120, 240, 480, 900, 900] {
        clock.advance(ChronoDuration::seconds(expected_delay));
        assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("admission keeps retrying").contains("retry at"));
        let status = ensures.get("quartermaster").await.expect("ensure").status.expect("retry status");
        assert_eq!(status.restart_count, 0);
        assert_eq!(status.retry_at.expect("deadline") - clock.now(), ChronoDuration::seconds((expected_delay * 2).min(900)));
    }
    assert!(matches!(
        backend.using::<ResourceDemand>("flotilla").get("ensure-attention-quartermaster").await,
        Err(ResourceError::NotFound { .. })
    ));

    let ensure = ensures.get("quartermaster").await.expect("failed ensure");
    let mut recovered_spec = ensure.spec.clone();
    recovered_spec.workflow_ref = "quartermaster".to_string();
    ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &recovered_spec).await.expect("repair admission");
    assert_eq!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("workflow dependency change resumes admission").len(), 1);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("recovered convoy").items.len(), 1);
}

pub async fn resolved_default_branch_dependency_change_retries_admission_before_deadline(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let projects = backend.definitions::<Project>("flotilla");
    let project = projects.get("standing-project").await.expect("project");
    let mut project_spec = project.spec.clone();
    project_spec.repositories[0].default_branch = None;
    projects.apply(&InputMeta::from(&project.metadata), &project_spec).await.expect("require discovered default branch");

    let refusal = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved default branch refuses admission");
    assert!(refusal.contains("no resolved default branch"), "{refusal}");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let status = ensures.get("quartermaster").await.expect("ensure").status.expect("retry status");
    assert_eq!(status.restart_count, 0);
    assert!(status.retry_at.is_some());

    let repository =
        backend.using::<Repository>("flotilla").list().await.expect("repositories").items.into_iter().next().expect("repository");
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let source_repositories = source.using::<Repository>("flotilla");
    let source_repository =
        source_repositories.create(&test_meta(&repository.metadata.name), &repository.spec).await.expect("replica repository source");
    source_repositories
        .update_status(
            &source_repository.metadata.name,
            &source_repository.metadata.resource_version,
            &flotilla_resources::RepositoryStatus { default_branch: Some("main".to_string()), ..Default::default() },
        )
        .await
        .expect("resolve default branch on another root");
    backend
        .replica_writer::<Repository>(NodeId::new("readiness-root"), "flotilla")
        .replace(&source_repositories.list().await.expect("repository source snapshot"), Utc::now())
        .await
        .expect("replicate resolved default branch");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("dependency change bypasses deadline"),
        vec!["started quartermaster@standing-project"]
    );
}

pub async fn changing_ensure_config_starts_a_fresh_retry_episode(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&backend, &clock).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }
    fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget");

    let definitions = backend.definitions::<ConvoyEnsure>("flotilla");
    let ensure = definitions.get("quartermaster").await.expect("ensure definition");
    let mut changed_spec = ensure.spec.clone();
    changed_spec.presents_as = Some("updated-fleet".to_string());
    definitions.apply(&InputMeta::from(&ensure.metadata), &changed_spec).await.expect("change ensure config");

    daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("config change opens a fresh episode");
    let status = definitions.get("quartermaster").await.expect("ensure").status.expect("status");
    assert_eq!(status.restart_count, 1);
    assert_eq!(status.hold_reason, None);
    assert!(backend.using::<ResourceDemand>("flotilla").list().await.expect("demands").items.is_empty());
    clock.advance(ChronoDuration::seconds(30));
    daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("admit after config change backoff");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

pub async fn standing_ensure_retries_convoy_that_failed_before_provisioning(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy_ref = backend
        .using::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("convoy ref");
    let convoy = convoys.get(&convoy_ref).await.expect("standing convoy");
    convoys
        .update_status(
            &convoy.metadata.name,
            &convoy.metadata.resource_version,
            &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                provisioning: Some(ConvoyProvisioningState::NotStarted),
                message: Some("workflow validation failed".to_string()),
                finished_at: Some(clock.now()),
                ..Default::default()
            },
        )
        .await
        .expect("record pre-provisioning failure");

    let events = daemon.reconcile_convoy_ensures_once("flotilla").await.expect("reconcile terminal convoy");

    assert!(events.iter().any(|event| event.contains("backing off")), "unexpected events: {events:?}");
    let ensure = backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure");
    assert!(ensure.status.expect("ensure status").retry_at.is_some());
    assert!(backend.using::<ResourceDemand>("flotilla").list().await.expect("demands").items.is_empty());
}

pub async fn operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let first_ref = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("first convoy ref");
    ensures
        .update_status(
            &ensure.metadata.name,
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus {
                convoy_ref: Some(first_ref.clone()),
                restart_count: 7,
                running_since: Some(clock.now()),
                retry_at: None,
                last_failure: None,
                hold_reason: None,
                observed_config_hash: None,
                declaration_refused: None,
                admitted_config_hash: None,
                config_drift: None,
                conditions: Vec::new(),
                retry: None,
                stalled: None,
            },
        )
        .await
        .expect("seed crash budget");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    convoys.delete(&first_ref).await.expect("operator reap");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("prompt resurrection"),
        vec!["started quartermaster@standing-project"]
    );
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);

    let materialized_name = flotilla_core::ops_entry::materialized_workflow_name("standing-project", "quartermaster");
    backend.definitions::<WorkflowTemplate>("flotilla").delete(&materialized_name).await.expect("temporary resolution loss");
    let second_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("second convoy ref");
    convoys.delete(&second_ref).await.expect("second operator reap");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved workflow schedules retry");
    let retrying = ensures.get("quartermaster").await.expect("retrying ensure");
    assert_eq!(retrying.status.as_ref().expect("status").restart_count, 7);
    let retry_at = retrying.status.as_ref().expect("status").retry_at.expect("durable retry time");

    let repository_key = backend.using::<Repository>("flotilla").list().await.expect("repositories").items[0].spec.key();
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder()
                .name(materialized_name)
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "standing-project".to_string())]))
                .build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("restore workflow resolution");
    let restarted_daemon = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::local(),
        backend.clone(),
        clock.clone(),
    )
    .await;
    restarted_daemon
        .install_convoy_ensure_reconciler(factory.create(restarted_daemon.resource_backend(), restarted_daemon.clock_for_scenarios()))
        .await;
    assert!(restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("retry not due").is_empty());
    clock.set(retry_at);
    assert_eq!(
        restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("past-due retry"),
        vec!["started quartermaster@standing-project"]
    );
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.unwrap().restart_count, 7);
}
