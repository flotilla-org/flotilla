//! Addressing scenarios using the shared daemon harness.

use super::*;

pub async fn convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), "flotilla", flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route sole terminal generation").expect("routing target");
    assert_eq!(target.home, daemon.host_name().clone());

    seed_convoy_routing_row(&daemon, "convoy-two", Some("reviewer"), "flotilla", flotilla_protocol::ConvoyPhase::Failed).await;
    assert_eq!(
        daemon.resolve_existing_convoy_target(&action).await,
        Err("convoy `reviewer@flotilla` is held by several records; name one by its record: convoy-one, convoy-two".to_string())
    );
}

pub async fn convoy_routing_prefers_an_exact_terminal_pre_identity_record(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "pre-identity-record", None, "flotilla", flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "pre-identity-record".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route exact terminal record").expect("routing target");
    assert_eq!(target.home, daemon.host_name().clone());
}

pub async fn convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "reviewer", None, "flotilla", flotilla_protocol::ConvoyPhase::Landed).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), "flotilla", flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route explicit role identity").expect("routing target");
    assert_eq!(target.home, daemon.host_name().clone());
}

pub async fn convoy_explain_addresses_an_exact_terminal_pre_identity_record(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let record = launch_convoy(
        &daemon,
        "pre-identity-record",
        ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("review".to_string()).build(),
        ConvoyPhase::Failed,
    )
    .await;

    let explanation = daemon.explain_convoy_internal(None, &record).await.expect("explain terminal record");
    assert_eq!(explanation.convoy, record);
    assert_eq!(explanation.phase, "Failed");
}

// #2684: explain exposes the pending episode's durable age and live blocker,
// including when the terminal has disappeared. Glue: one projection call-through.
pub async fn convoy_explain_surfaces_queued_turn_age_and_blocker(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let queued_at = daemon.clock_for_scenarios().now() - chrono::Duration::seconds(601);
    // The queued turn is the home's note on the convoy, which its launch keeps.
    let status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        turn_deliveries: BTreeMap::from([(
            "review".into(),
            flotilla_resources::TurnDeliveryStatus {
                episodes: vec![flotilla_resources::TurnDeliveryEpisode {
                    subject_revision: "head".into(),
                    evidence_at: queued_at,
                    judged_claim_at: queued_at,
                    outcome: flotilla_resources::TurnDeliveryOutcome::Queued {
                        rung: flotilla_resources::TurnDeliveryRung::WarmSession,
                        queued_at,
                        vessel: "work".into(),
                        role: "coder".into(),
                        message_id: "turn".into(),
                        blocking_reason: "old readiness".into(),
                    },
                }],
                ..Default::default()
            },
        )]),
        ..flotilla_engine_convoys_testkit::single_agent("coder")
    };
    let queued = flotilla_engine_convoys_testkit::launch(
        &daemon.convoy_engine_path(),
        flotilla_engine_convoys_testkit::convoy(
            "flotilla",
            "queued",
            ConvoySpec::builder().project_ref("flotilla".into()).workflow_ref("review".into()).build(),
            status,
        ),
    )
    .await;
    let explanation = daemon.explain_convoy_internal(Some("flotilla"), &queued).await.unwrap();
    let turns = explanation.queued_turns;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].source, "review");
    assert_eq!(turns[0].subject_revision, "head");
    assert_eq!(serde_json::to_value(&turns[0]).unwrap()["rung"], "warm-session");
    assert_eq!(turns[0].queued_at, queued_at.to_rfc3339());
    assert_eq!(turns[0].age_seconds, 601);
    assert!(turns[0].overdue);
    assert_eq!(turns[0].blocking_reason, "terminal session unavailable");
}

pub async fn convoy_explain_refuses_multiple_terminal_generations(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let one = create_identity_convoy(&daemon, "convoy-one", "reviewer", "flotilla").await;
    let two = create_identity_convoy(&daemon, "convoy-two", "reviewer", "flotilla").await;
    // Each generation ends held, as an unread message to its seat holds it: neither retires.
    for name in [&one, &two] {
        hold_with_message(&daemon, name, "reviewer").await;
        land_convoy(&daemon, name).await;
    }

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer@flotilla").await,
        Err(format!("convoy `reviewer@flotilla` is held by several records; name one by its record: {one}, {two}"))
    );
}

// #3108 retirement: a role address selects among live and ended-but-unretired holders, as a
// listing does. Once every generation has retired, the plain name names the newest, and each is
// reached by its record name, which reports how it ended.
pub async fn convoy_explain_names_retired_generations_by_record_only(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let mut watch = backend.including_replicas::<ResourceConvoy>("flotilla").watch().await.expect("watch");
    let one = create_identity_convoy(&daemon, "convoy-one", "reviewer", "flotilla").await;
    let two = create_identity_convoy(&daemon, "convoy-two", "reviewer", "flotilla").await;
    for name in [&one, &two] {
        land_convoy(&daemon, name).await;
    }
    // Each leaves the listing as it retires.
    let mut retired = BTreeSet::new();
    while retired.len() < 2 {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(10), futures::StreamExt::next(&mut watch)).await.expect("retired in time");
        match event.expect("watching").expect("event") {
            flotilla_resources::ReadWatchEvent::Deleted(read) => retired.insert(read.object.metadata.name),
            flotilla_resources::ReadWatchEvent::DeletedByName { tombstone, .. } => retired.insert(tombstone.name),
            _ => false,
        };
    }
    assert_eq!(retired, BTreeSet::from([one.clone(), two.clone()]));

    // `name@project` is a plain name: once every launch has retired it names the newest (#3108).
    assert_eq!(flotilla_store::resolve_convoy_record(&backend, "flotilla", "reviewer@flotilla").await, Ok(two.clone()));
    let newest = daemon.explain_convoy_internal(None, "reviewer@flotilla").await.expect("the newest explains by the plain name");
    assert_eq!(newest.phase, "Landed");
    for record in [&one, &two] {
        let explanation = daemon.explain_convoy_internal(None, record).await.expect("a retired generation explains by its record");
        assert_eq!(explanation.phase, "Landed", "{record}");
    }
}

// #4272: the lookup is exact. Every convoy names its project, so a bare role names none; the
// client completes a role from its context before sending it.
pub async fn convoy_explain_names_no_convoy_by_a_bare_role(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let one = create_identity_convoy(&daemon, "convoy-one", "reviewer", "flotilla").await;
    let two = create_identity_convoy(&daemon, "convoy-two", "reviewer", "beta").await;

    for (address, record) in [("reviewer@flotilla", &one), ("reviewer@beta", &two)] {
        daemon.explain_convoy_internal(None, address).await.expect("explained");
        let found = flotilla_store::resolve_convoy_record(&daemon.resource_backend(), "flotilla", address).await;
        assert_eq!(found.as_ref(), Ok(record), "{address}");
    }
    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer").await.map(|explanation| explanation.convoy),
        Err("no convoy matches `reviewer`".to_string())
    );
}

pub async fn ensure_roll_targets_the_running_convoy_home_over_an_explicit_other_host(factory: &dyn EnsureScenarioController) {
    use flotilla_core::command_target::{RemoteDelivery, TargetHost, TargetReason};
    let (driver, _driver_backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    driver.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let (observer, _observer_backend, _observer_clock, _observer_temp) = standing_ensure_fixture_for(factory, "observer", false).await;
    // The driver's history reaches the observer through the driver's log.
    let driver_root = driver.node_id().clone();
    flotilla_provisioning_testkit::replicate_home(
        &driver.decided_store().expect("driver's decided store"),
        driver_root.as_str(),
        &observer.decided_store().expect("observer's decided store"),
    );
    let target = observer
        .resolve_command_target(
            &CommandAction::ConvoyEnsureRoll { namespace: "flotilla".into(), name: "quartermaster".into() },
            Some(&NodeId::new("unrelated")),
        )
        .await
        .expect("resolve roll target");
    assert_eq!(target.host, TargetHost::Node(driver_root));
    assert_eq!(target.reason, TargetReason::RecordHome);
    assert_eq!(target.delivery, RemoteDelivery::Command);
}
