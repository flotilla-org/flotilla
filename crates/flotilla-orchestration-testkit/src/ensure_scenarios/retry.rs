//! Retry scenarios using the shared daemon harness.

use super::*;

pub async fn standing_ensure_holds_after_three_failed_generations_and_resumes_when_attention_is_cleared(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");

    for delay in [30, 60] {
        fail_ensured_generation(&daemon).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        // The pass tells its caller when the backoff ends, as nothing else wakes it then.
        let retry_at = quartermaster(&daemon).await.next_attempt;
        assert!(retry_at.is_some(), "a backing-off role shows when it next admits");
        assert_eq!(daemon.convoy_ensure_next_due("flotilla").await.expect("next due"), retry_at);
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }

    fail_ensured_generation(&daemon).await;
    assert_eq!(
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    let held = quartermaster(&daemon).await;
    assert_eq!(held.strikes, 3);
    assert_eq!(held.hold, Some(StandingRoleHold::RestartLimit));
    assert_eq!(open_holds(&backend).await.len(), 1, "restart escalation");

    clock.advance(ChronoDuration::hours(6));
    assert!(daemon
        .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
        .await
        .expect("remain held")
        .is_empty());
    assert_eq!(daemon.convoy_ensure_next_due("flotilla").await.expect("next due"), None, "a hold waits for a person, not a time");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 3);

    assert_eq!(acknowledge_waiting(&daemon).await, 1, "operator acknowledges escalation");
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("clear hold");
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("schedule fresh episode");
    clock.advance(ChronoDuration::seconds(30));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("resume admission");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

pub async fn reconcile_now_resets_backoff_and_admits_the_next_ensure_generation_immediately(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    fail_ensured_generation(&daemon).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record backoff");
    let backed_off = quartermaster(&daemon).await;
    assert_eq!(backed_off.strikes, 1);
    assert!(backed_off.next_attempt.is_some());

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced admission");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let reconciled = quartermaster(&daemon).await;
    assert_eq!(reconciled.next_attempt, None);
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 2);
    // ConvoyEnsure B: reconcile-now skips the wait but does not erase the failed generation the
    // strike count is derived from: the new generation failing is the run's second strike.
    let live = reconciled.convoy_ref.expect("the admitted generation");
    fail_generation(&daemon, &live, "placement failed").await;
    assert_eq!(quartermaster(&daemon).await.strikes, 2);
}

pub async fn reconcile_now_clears_an_active_restart_limit_and_admits_in_one_pass(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&daemon).await;
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record failure");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("restart");
    }
    fail_ensured_generation(&daemon).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust restart budget");

    let outcome = daemon.reconcile_convoy_ensure_now("flotilla", "quartermaster", &VerifiedDeadBacking).await.expect("forced restart");

    assert_eq!(outcome, "started quartermaster@standing-project");
    let running = quartermaster(&daemon).await;
    assert_eq!((running.hold, running.next_attempt), (None, None));
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
    assert!(open_holds(&backend).await.is_empty());
    // ConvoyEnsure B: reconcile-now admits whatever the strikes say; only an acknowledgement ends
    // the run, so the three failed generations still count when the forced one fails too.
    fail_generation(&daemon, &running.convoy_ref.expect("the forced generation"), "placement failed").await;
    let held = quartermaster(&daemon).await;
    assert_eq!(held.strikes, 4);
    assert_eq!(held.hold, Some(StandingRoleHold::RestartLimit));
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
    assert_eq!(quartermaster(&daemon).await.convoy_ref, Some(live[0].metadata.name.clone()));
}

pub async fn declared_driver_derives_bounded_backoff_from_its_homed_generations(factory: &dyn EnsureScenarioController) {
    let (authority, authority_backend, _authority_clock, _authority_temp) = standing_ensure_fixture_for(factory, "kiwi", true).await;
    let (driver, driver_backend, driver_clock, _driver_temp) = standing_ensure_fixture_for(factory, "udder", false).await;
    let (other, other_backend, _other_clock, _other_temp) = standing_ensure_fixture_for(factory, "feta", false).await;
    let driver_id = driver.local_host_id().expect("driver host identity").to_string();
    set_ensure_driver(&authority_backend, &driver_id).await;

    // The authority's Project reaches the others through its log; its ConvoyEnsure through the
    // old store's replication.
    let authority_root = authority.node_id().clone();
    let decided = |daemon: &InProcessDaemon| daemon.decided_store().expect("decided store");
    for (backend, daemon) in [(&driver_backend, &driver), (&other_backend, &other)] {
        flotilla_provisioning_testkit::replicate_home(&decided(&authority), authority_root.as_str(), &decided(daemon));
        backend
            .replica_writer::<ConvoyEnsure>(authority_root.clone(), "flotilla")
            .replace(&authority_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("authority ensures"), Utc::now())
            .await
            .expect("replicate ensures");
    }

    // The driver publishes its own Host, ready, and every root hears it.
    let hosts = driver_backend.using::<ResourceHost>("flotilla");
    let host = match hosts.get(&driver_id).await {
        Ok(host) => host,
        Err(_) => hosts
            .create(
                &test_meta(&driver_id),
                &HostSpec { display_name: "udder".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("driver host"),
    };
    hosts
        .update_status(&host.metadata.name, &host.metadata.resource_version, &HostStatus { ready: true, ..Default::default() })
        .await
        .expect("ready driver host");
    for daemon in [&authority, &other] {
        flotilla_provisioning_testkit::replicate_home(&decided(&driver), driver.node_id().as_str(), &decided(daemon));
    }

    assert!(authority.reconcile_convoy_ensures_once("flotilla").await.expect("authority skip").is_empty());
    assert_eq!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("driver admission").len(), 1);
    assert!(other.reconcile_convoy_ensures_once("flotilla").await.expect("non-driver skip").is_empty());
    assert!(driver.reconcile_convoy_ensures_once("flotilla").await.expect("steady-state driver pass").is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys").items.len(), 1);
    assert!(authority_backend.using::<ResourceConvoy>("flotilla").list().await.expect("authority convoys").items.is_empty());
    assert!(other_backend.using::<ResourceConvoy>("flotilla").list().await.expect("other convoys").items.is_empty());
    assert!(driver_backend.using::<ConvoyEnsure>("flotilla").list().await.expect("driver local ensures").items.is_empty());

    for delay in [30, 60] {
        fail_latest_ensured_generation(&driver, &driver_clock).await;
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

    fail_latest_ensured_generation(&driver, &driver_clock).await;
    assert_eq!(
        driver
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("escalate bounded failures"),
        vec!["ConvoyEnsure/quartermaster exhausted restart budget"]
    );
    assert_eq!(open_holds(&driver_backend).await.len(), 1, "driver-homed escalation");
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
    assert!(open_holds(&driver_backend).await.is_empty());
    assert_eq!(driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("resumed generations").items.len(), 4);
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
    let declared_version = ensures.get("quartermaster").await.expect("updated ensure").metadata.resource_version;

    let first = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("first admission refusal");
    assert!(first.contains("retry at"), "{first}");
    let refused = quartermaster(&daemon).await;
    assert_eq!(refused.convoy_ref, None);
    assert_eq!(refused.strikes, 0);
    assert!(refused.next_attempt.is_some());
    assert!(refused.last_failure.is_some(), "the driver shows why it was refused");

    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("backoff suppresses retry").is_empty());
    assert_eq!(
        ensures.get("quartermaster").await.expect("unchanged ensure").metadata.resource_version,
        declared_version,
        "a refusal writes nothing to its ensure"
    );
    clock.advance(ChronoDuration::seconds(30));
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("second admission refusal").contains("retry at"));
    // #2875: read-only refusals share the provisioning cadence and cap while
    // remaining independent of the runtime strike budget.
    for expected_delay in [60, 120, 240, 480, 900, 900] {
        clock.advance(ChronoDuration::seconds(expected_delay));
        assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("admission keeps retrying").contains("retry at"));
        let refused = quartermaster(&daemon).await;
        assert_eq!(refused.strikes, 0);
        assert_eq!(refused.next_attempt.expect("deadline") - clock.now(), ChronoDuration::seconds((expected_delay * 2).min(900)));
    }
    assert!(open_holds(&backend).await.is_empty());

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
    let refused = quartermaster(&daemon).await;
    assert_eq!(refused.strikes, 0);
    assert!(refused.next_attempt.is_some());

    let repository =
        backend.using::<Repository>("flotilla").list().await.expect("repositories").items.into_iter().next().expect("repository");
    // Another root resolves the default branch, and its record arrives through that root's log.
    let readiness_root = other_home(&daemon, "readiness-root");
    readiness_root.repository(&repository.metadata.name, &repository.spec).await;
    readiness_root
        .repository_status(
            &repository.metadata.name,
            &flotilla_resources::RepositoryStatus { default_branch: Some("main".to_string()), ..Default::default() },
        )
        .await;
    replicate_other_home(&daemon, "readiness-root");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("dependency change bypasses deadline"),
        vec!["started quartermaster@standing-project"]
    );
}

pub async fn changing_ensure_config_starts_a_fresh_retry_episode(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    for delay in [30, 60] {
        fail_ensured_generation(&daemon).await;
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("record failed generation");
        clock.advance(ChronoDuration::seconds(delay));
        daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("admit replacement");
    }
    fail_ensured_generation(&daemon).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("exhaust retry budget");

    let definitions = backend.definitions::<ConvoyEnsure>("flotilla");
    let ensure = definitions.get("quartermaster").await.expect("ensure definition");
    let mut changed_spec = ensure.spec.clone();
    changed_spec.presents_as = Some("updated-fleet".to_string());
    definitions.apply(&InputMeta::from(&ensure.metadata), &changed_spec).await.expect("change ensure config");

    // ConvoyEnsure B: the held run failed under the old configuration, so under the new one no
    // generation has struck yet, and the next is admitted at once.
    assert_eq!(
        daemon
            .reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking)
            .await
            .expect("config change opens a fresh episode"),
        vec!["started quartermaster@standing-project"]
    );
    let fresh = quartermaster(&daemon).await;
    assert_eq!(fresh.strikes, 0);
    assert_eq!(fresh.hold, None);
    assert!(open_attentions(&backend).await.is_empty());
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 4);
}

pub async fn standing_ensure_retries_convoy_that_failed_before_provisioning(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let convoy_ref = quartermaster(&daemon).await.convoy_ref.expect("convoy ref");
    // It failed before any of its works launched a vessel.
    fail_generation(&daemon, &convoy_ref, "workflow validation failed").await;

    let events = daemon.reconcile_convoy_ensures_once("flotilla").await.expect("reconcile terminal convoy");

    assert!(events.is_empty(), "a backoff writes nothing and admits nothing: {events:?}");
    let backing_off = quartermaster(&daemon).await;
    assert_eq!(backing_off.strikes, 1);
    assert!(backing_off.next_attempt.is_some());
    assert!(open_attentions(&backend).await.is_empty());
}

pub async fn operator_reap_restarts_immediately_without_burning_budget_and_past_due_retry_survives_restart(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let first_ref = quartermaster(&daemon).await.convoy_ref.expect("first convoy ref");
    // ConvoyEnsure B: the strike count is derived from the role's generations, so there is no
    // stored budget to seed; a reap leaves no failed generation behind, and the count stays 0.
    abandon_generation(&daemon, &first_ref).await;

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("prompt resurrection"),
        vec!["started quartermaster@standing-project"]
    );
    let resurrected = quartermaster(&daemon).await;
    assert_eq!(resurrected.strikes, 0);

    let materialized_name = flotilla_charter::ops_entry::materialized_workflow_name("standing-project", "quartermaster");
    backend.definitions::<WorkflowTemplate>("flotilla").delete(&materialized_name).await.expect("temporary resolution loss");
    let second_ref = resurrected.convoy_ref.expect("second convoy ref");
    abandon_generation(&daemon, &second_ref).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("unresolved workflow schedules retry");
    let retrying = quartermaster(&daemon).await;
    assert_eq!(retrying.strikes, 0, "a reap is no strike");
    assert!(retrying.next_attempt.is_some_and(|retry_at| retry_at > clock.now()), "the refusal backs off");
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("retry not due").is_empty());

    let repository_key = backend.using::<Repository>("flotilla").list().await.expect("repositories").items[0].spec.key();
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder().name(materialized_name).annotations(project_ops_stamps("standing-project", &repository_key)).build(),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key])
                    .crew(vec![governor()])
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
        std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
    )
    .await;
    install_engine(&restarted_daemon);
    restarted_daemon
        .install_convoy_ensure_reconciler(factory.create(
            restarted_daemon.convoy_teardown(),
            restarted_daemon.resource_backend(),
            restarted_daemon.clock_for_scenarios(),
        ))
        .await;
    // The refusal note is the admitting controller's, in memory (#3108): the restarted one holds
    // none, and retries at once.
    assert_eq!(
        restarted_daemon.reconcile_convoy_ensures_once("flotilla").await.expect("retry after restart"),
        vec!["started quartermaster@standing-project"]
    );
    assert_eq!(quartermaster(&restarted_daemon).await.strikes, 0);
}
