//! Addressing scenarios using the shared daemon harness.

use super::*;

pub async fn convoy_routing_falls_back_to_a_unique_terminal_generation_and_refuses_multiple(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route sole terminal generation").expect("routing target");
    assert_eq!(target.home, daemon.host_name);

    seed_convoy_routing_row(&daemon, "convoy-two", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Failed).await;
    assert_eq!(
        daemon.resolve_existing_convoy_target(&action).await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

pub async fn convoy_routing_prefers_an_exact_terminal_pre_identity_record(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "pre-identity-record", None, None, flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "pre-identity-record".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route exact terminal record").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

pub async fn convoy_routing_does_not_treat_a_legacy_display_name_as_role_identity(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    seed_convoy_routing_row(&daemon, "reviewer", None, Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    seed_convoy_routing_row(&daemon, "convoy-one", Some("reviewer"), Some("flotilla"), flotilla_protocol::ConvoyPhase::Landed).await;
    let action = flotilla_protocol::CommandAction::ConvoyDelete { namespace: None, name: "reviewer@flotilla".to_string(), force: false };

    let target = daemon.resolve_existing_convoy_target(&action).await.expect("route explicit role identity").expect("routing target");
    assert_eq!(target.home, daemon.host_name);
}

pub async fn convoy_explain_addresses_an_exact_terminal_pre_identity_record(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let created = convoys
        .create(
            &InputMeta::builder().name("pre-identity-record".to_string()).build(),
            &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
        )
        .await
        .expect("pre-identity convoy");
    convoys
        .update_status(
            &created.metadata.name,
            &created.metadata.resource_version,
            &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
        )
        .await
        .expect("mark convoy terminal");

    let explanation = daemon.explain_convoy_internal(None, "pre-identity-record").await.expect("explain terminal record");
    assert_eq!(explanation.convoy, "pre-identity-record");
    assert_eq!(explanation.phase, "Failed");
}

// #2684: explain exposes the pending episode's durable age and live blocker,
// including when the terminal has disappeared. Glue: one projection call-through.
pub async fn convoy_explain_surfaces_queued_turn_age_and_blocker(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let namespace = "queued-explain";
    let convoys = backend.using::<ResourceConvoy>(namespace);
    let convoy = convoys.create(&test_meta("queued"), &ConvoySpec::builder().workflow_ref("review".into()).build()).await.unwrap();
    let queued_at = daemon.clock.now() - chrono::Duration::seconds(601);
    convoys
        .update_status(
            "queued",
            &convoy.metadata.resource_version,
            &ConvoyStatus {
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
                            sender: Default::default(),
                        }],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let explanation = daemon.explain_convoy_internal(Some(namespace), "queued").await.unwrap();
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
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", Some("flotilla")).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("flotilla")).await;
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    for name in ["convoy-one", "convoy-two"] {
        let created = convoys.get(name).await.expect("terminal convoy");
        convoys
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
            )
            .await
            .expect("mark convoy terminal");
    }

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer@flotilla").await,
        Err("convoy address `reviewer@flotilla` matches multiple terminal records; use an exact record name: convoy-one, convoy-two"
            .to_string())
    );
}

pub async fn convoy_explain_rejects_projectless_and_project_bound_role_ambiguity(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    create_identity_convoy(&backend, "convoy-one", "reviewer", None).await;
    create_identity_convoy(&backend, "convoy-two", "reviewer", Some("beta")).await;

    assert_eq!(
        daemon.explain_convoy_internal(None, "reviewer").await.expect_err("bare role must be ambiguous"),
        "convoy role `reviewer` is ambiguous; use one of: reviewer@, reviewer@beta"
    );
}

pub async fn ensure_roll_targets_the_running_convoy_home_over_an_explicit_other_host(factory: &dyn EnsureScenarioController) {
    use crate::command_target::{RemoteDelivery, TargetHost, TargetReason};
    let (driver, driver_backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    driver.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let (observer, observer_backend, _observer_clock, _observer_temp) = standing_ensure_fixture_for(factory, "observer", false).await;
    let driver_root = NodeId::new("driver-root");
    observer_backend
        .replica_writer::<ResourceConvoy>(driver_root.clone(), "flotilla")
        .replace(&driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys"), Utc::now())
        .await
        .expect("replicate driver history");
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
