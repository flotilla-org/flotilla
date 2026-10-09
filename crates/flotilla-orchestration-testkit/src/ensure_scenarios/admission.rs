//! Admission scenarios using the shared daemon harness.

use super::*;

pub async fn two_origins_admit_identical_placements_without_authorship_collision(factory: &dyn EnsureScenarioController) {
    assert_independent_placement_snapshots(factory, false).await;
    assert_independent_placement_snapshots(factory, true).await;
}

pub async fn rebooted_standing_governor_admits_one_replacement_vessel_without_a_second_convoy(factory: &dyn EnsureScenarioController) {
    use flotilla_resources::controller::{Actuation, Reconciler};

    let (daemon, backend, clock, temp) = standing_ensure_fixture(factory).await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("admit standing governor");
    let convoys = backend.clone().using::<ResourceConvoy>("flotilla");
    let convoy_name = backend
        .definitions::<ConvoyEnsure>("flotilla")
        .get("quartermaster")
        .await
        .expect("ensure")
        .status
        .expect("ensure status")
        .convoy_ref
        .expect("governor convoy");
    let admitted = convoys.get(&convoy_name).await.expect("admitted convoy");
    let workflow = backend
        .using::<WorkflowTemplate>("flotilla")
        .get(&flotilla_core::ops_entry::materialized_workflow_name("standing-project", "quartermaster"))
        .await
        .expect("standing workflow");
    let mut status = admitted.status.clone().unwrap_or_default();
    status.phase = ConvoyPhase::Active;
    status.observed_workflow_ref = Some(admitted.spec.workflow_ref.clone());
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        cascade: None,
        exit: workflow.spec.exit,
        turn_delivery: workflow.spec.turn_delivery,
        stall_nudges: workflow.spec.stall_nudges,
        supervision: workflow.spec.supervision,
        vessels: workflow.spec.vessels,
    });
    status.work.insert("work".to_string(), flotilla_resources::WorkState::builder().phase(flotilla_resources::WorkPhase::Running).build());
    status.crew_work.insert(
        "work".to_string(),
        BTreeMap::from([("governor".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]),
    );
    convoys.update_status(&convoy_name, &admitted.metadata.resource_version, &status).await.expect("running governor");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    let vessel_name = format!("{convoy_name}-work");
    let vessel = vessels
        .create(
            &InputMeta::builder()
                .name(vessel_name.clone())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.clone())]))
                .build(),
            &VesselSpec {
                convoy_ref: convoy_name.clone(),
                vessel_name: "work".to_string(),
                placement_policy_ref: "docker".to_string(),
                adopted_checkout_refs: BTreeMap::new(),
            },
        )
        .await
        .expect("governor vessel");
    vessels
        .update_status(
            &vessel_name,
            &vessel.metadata.resource_version,
            &flotilla_resources::VesselStatus {
                phase: flotilla_resources::VesselPhase::Failed,
                message: Some("Docker container stopped after host reboot".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("lost container");
    assert!(backend.using::<ResourceCheckout>("flotilla").list().await.expect("ephemeral checkout observations").items.is_empty());
    drop(daemon);
    let restarted = InProcessDaemon::new_with_resource_backend_and_clock(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("local"),
        backend.clone(),
        clock.clone(),
    )
    .await;
    restarted.install_convoy_ensure_reconciler(factory.create(restarted.resource_backend(), restarted.clock_for_scenarios())).await;

    let reconciler =
        flotilla_resources::ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>("flotilla")).with_vessels(vessels.clone());
    let convoy = convoys.get(&convoy_name).await.expect("governor after reboot");
    let outcome = reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("reboot observations"), clock.now());
    assert!(matches!(outcome.patch, Some(flotilla_resources::ConvoyStatusPatch::WorkProvisioningRetry { .. })));
    // #2875: reboot recovery keeps the death cause and waits for a durable
    // deadline, rather than replacing the governor at every watch tick.
    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteVessel { .. })));
    flotilla_resources::apply_status_patch(&convoys, &convoy_name, &outcome.patch.expect("interrupt work")).await.expect("interrupt work");
    clock.advance(ChronoDuration::seconds(29));
    let convoy = convoys.get(&convoy_name).await.expect("backing off governor");
    assert_eq!(convoy.status.as_ref().expect("status").work["work"].message.as_deref(), Some("Docker container stopped after host reboot"));
    let waiting = reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("failed vessel still observed"), clock.now());
    assert!(!waiting
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteVessel { .. } | Actuation::CreateVessel { .. })));
    clock.advance(ChronoDuration::seconds(1));
    let retiring = reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("failed vessel"), clock.now());
    assert!(retiring.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == &vessel_name)));
    vessels.delete(&vessel_name).await.expect("retire lost vessel");

    let convoy = convoys.get(&convoy_name).await.expect("interrupted governor");
    let replacement = reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("lost vessel absent"), clock.now());
    assert_eq!(
        replacement.actuations.iter().filter(|actuation| matches!(actuation, Actuation::CreateVessel { .. })).count(),
        1,
        "convoy annotations: {:?}",
        convoy.metadata.annotations
    );
    flotilla_resources::apply_status_patch(&convoys, &convoy_name, &replacement.patch.expect("reserve next attempt"))
        .await
        .expect("retry reservation");
    let convoy = convoys.get(&convoy_name).await.expect("reserved retry");
    let stale_observation =
        reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("replacement not yet observed"), clock.now());
    assert!(!stale_observation.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateVessel { .. })));
    let (meta, spec) = replacement
        .actuations
        .into_iter()
        .find_map(|actuation| match actuation {
            Actuation::CreateVessel { meta, spec } => Some((meta, spec)),
            _ => None,
        })
        .expect("replacement vessel");
    vessels.create(&meta, &spec).await.expect("admit replacement");
    let convoy = convoys.get(&convoy_name).await.expect("standing convoy");
    let repeated = reconciler.reconcile(&convoy, &reconciler.prepare(&convoy).await.expect("replacement observed"), clock.now());
    assert!(!repeated.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreateVessel { .. })));
    restarted.reconcile_convoy_ensures_once("flotilla").await.expect("ensure remains steady after restart");
    assert_eq!(convoys.list().await.expect("convoys").items.len(), 1);
    assert_ne!(convoys.get(&convoy_name).await.expect("convoy").status.expect("status").phase, ConvoyPhase::Failed);
}

pub async fn declaration_refusal_staleness_uses_the_daemon_clock_at_the_24_hour_boundary(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    let projects = backend.using::<Project>("flotilla");
    apply_resource_status_patch(
        &projects,
        "standing-project",
        &flotilla_resources::ProjectStatusPatch::DeclarationRefused {
            condition: Some(flotilla_resources::DeclarationRefusedCondition {
                entry_path: "broken.md".into(),
                message: "invalid declaration".into(),
                since: clock.now(),
                observed_at: clock.now(),
            }),
        },
    )
    .await
    .expect("record refusal");
    backend
        .using::<ResourceDemand>("flotilla")
        .create(
            &InputMeta::builder()
                .name("declaration-refused-standing-project".to_string())
                .annotations(BTreeMap::from([
                    (flotilla_core::ops_entry::DECLARATION_REFUSAL_REASON_ANNOTATION.into(), "invalid declaration".into()),
                    (flotilla_core::ops_entry::DECLARATION_REFUSED_SINCE_ANNOTATION.into(), clock.now().to_rfc3339()),
                ]))
                .build(),
            &DemandSpec::for_dispatching_principal(
                ResourceRef::new("flotilla.work/v1", "Project", "flotilla", "standing-project"),
                DemandKind::HumanGate,
                PrincipalRef::implicit_for_namespace("flotilla"),
            ),
        )
        .await
        .expect("refusal attention");
    let stale = || async {
        daemon
            .list_projects_internal()
            .await
            .expect("project list")
            .projects
            .into_iter()
            .find(|project| project.name == "standing-project")
            .expect("project")
            .declaration_stale
    };
    assert!(!stale().await);
    clock.advance(chrono::Duration::hours(24) - chrono::Duration::seconds(1));
    assert!(!stale().await);
    clock.advance(chrono::Duration::seconds(1));
    assert!(stale().await);
    assert!(daemon
        .fleet_list_internal()
        .await
        .expect("attention without a refresh")
        .declaration_attention
        .iter()
        .any(|row| row.message.ends_with("(stale)")));
}

pub async fn standing_ensure_records_admitted_config_and_surfaces_drift_without_replacing_work(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("admit initial configuration");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let first = ensures.get("quartermaster").await.expect("ensure");
    let first_status = first.status.clone().expect("admitted status");
    assert_eq!(first_status.admitted_config_hash, first_status.observed_config_hash);
    let mut changed = first.spec.clone();
    changed.presents_as = Some("project".into());
    ensures.apply(&InputMeta::from(&first.metadata), &changed).await.expect("change declaration");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("observe drift");
    let drifted = ensures.get("quartermaster").await.expect("drifted ensure").status.expect("status");
    assert_eq!(drifted.convoy_ref, first_status.convoy_ref, "running work must remain intact");
    assert_eq!(drifted.admitted_config_hash, first_status.admitted_config_hash);
    assert_ne!(drifted.admitted_config_hash, drifted.observed_config_hash);
    assert!(drifted.config_drift.expect("typed drift").changes.iter().any(|change| change.contains("presentation")));
    assert!(backend
        .using::<ResourceDemand>("flotilla")
        .list()
        .await
        .expect("attention")
        .items
        .iter()
        .any(|demand| demand.metadata.name == "ensure-config-drift-quartermaster"));
    let fleet = daemon.fleet_list_internal().await.expect("public fleet listing");
    assert!(fleet.declaration_attention.iter().any(|row| row.message.contains("presentation")));
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("drift attention persists across refreshes");
    assert_eq!(backend.using::<ResourceDemand>("flotilla").list().await.expect("attention").items.len(), 1);
}

pub async fn ensure_reconciliation_recovers_a_roll_interrupted_after_retirement(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let first = ensures.get("quartermaster").await.expect("ensure");
    let old_ref = first.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("active generation");
    let mut next = first.spec.clone();
    next.presents_as = Some("project".into());
    ensures.apply(&InputMeta::from(&first.metadata), &next).await.expect("change config");
    let desired = ensures.get("quartermaster").await.expect("desired ensure");
    daemon.prepare_ensured_convoy("flotilla", &desired).await.expect("prepare replacement");
    daemon.abandon_convoy_internal("flotilla", &old_ref, "simulate crash after retirement", None).await.expect("retire old work");
    // No successor was committed: the next ordinary passes must recover it.
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("schedule recovery");
    clock.advance(chrono::Duration::minutes(3));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("recover admission");
    let status = ensures.get("quartermaster").await.expect("ensure").status.expect("status");
    assert_ne!(status.convoy_ref.as_deref(), Some(old_ref.as_str()));
    assert!(status.convoy_ref.is_some());
    assert!(status.config_drift.is_none());
    assert_eq!(status.admitted_config_hash, status.observed_config_hash);
}

pub async fn explicit_ensure_roll_readmits_current_config_and_retains_the_previous_generation(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let old = ensures.get("quartermaster").await.expect("ensure");
    let old_ref = old.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("running convoy");
    let mut next = old.spec.clone();
    next.presents_as = Some("project".into());
    ensures.apply(&InputMeta::from(&old.metadata), &next).await.expect("desired configuration");
    daemon.roll_convoy_ensure("flotilla", "quartermaster").await.expect("operator rolls drifted ensure");
    let status = ensures.get("quartermaster").await.expect("ensure").status.expect("readmitted status");
    let next_ref = status.convoy_ref.expect("replacement convoy");
    assert_ne!(next_ref, old_ref);
    assert_eq!(status.admitted_config_hash, status.observed_config_hash);
    assert!(status.config_drift.is_none());
    let previous = backend.using::<ResourceConvoy>("flotilla").get(&old_ref).await.expect("retained history");
    assert_eq!(previous.status.expect("terminal history").phase, ConvoyPhase::Abandoned);
    let replacement = backend.using::<ResourceConvoy>("flotilla").get(&next_ref).await.expect("replacement");
    assert_eq!(replacement.spec.generation, previous.spec.generation + 1);
    assert_eq!(replacement.metadata.annotations.get(PRESENTS_AS_ANNOTATION).map(String::as_str), Some("project"));
    assert_eq!(
        replacement.status.as_ref().and_then(|status| status.ensure_admission.as_ref()).map(|config| config.presents_as.as_deref()),
        Some(Some("project"))
    );
    daemon.roll_convoy_ensure("flotilla", "quartermaster").await.expect("repeated roll is a no-op without drift");
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.expect("status").convoy_ref.as_deref(), Some(next_ref.as_str()));
    assert!(backend.using::<ResourceDemand>("flotilla").list().await.expect("attention cleared").items.is_empty());
}

pub async fn changed_ensure_driver_waits_for_operator_roll_of_the_running_remote_generation(factory: &dyn EnsureScenarioController) {
    let (old_driver, old_backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    old_driver.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let (new_driver, new_backend, _new_clock, _new_temp) = standing_ensure_fixture_for(factory, "new-driver", true).await;
    new_backend
        .replica_writer::<ResourceConvoy>(NodeId::new("old-driver"), "flotilla")
        .replace(&old_backend.using::<ResourceConvoy>("flotilla").list().await.expect("old generations"), Utc::now())
        .await
        .expect("replicate active generation");
    let ensures = new_backend.definitions::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let mut next = ensure.spec.clone();
    next.driver_ref = Some(new_driver.local_host_id().expect("new driver's host identity").to_string());
    ensures.apply(&InputMeta::from(&ensure.metadata), &next).await.expect("declare new driver");
    new_driver.reconcile_convoy_ensures_once("flotilla").await.expect("observe driver drift");
    new_driver
        .reconcile_convoy_ensure_now("flotilla", "quartermaster", &RecordlessBacking)
        .await
        .expect("explicit reconcile does not substitute for a roll");
    assert!(
        new_backend.using::<ResourceConvoy>("flotilla").list().await.expect("local generations").items.is_empty(),
        "driver drift must not start overlapping work"
    );
    assert!(ensures.get("quartermaster").await.expect("ensure").status.expect("drift status").config_drift.is_some());
}

pub async fn ensure_drift_names_config_changes_and_invalid_roll_keeps_the_current_generation(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let first = ensures.get("quartermaster").await.expect("ensure");
    let first_ref = first.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("convoy");
    let mut changed = first.spec.clone();
    changed.workflow_ref = "missing-workflow".into();
    changed.placement_policy = Some("missing-policy".into());
    changed.repositories.push(RepositoryKey("missing-repository".into()));
    changed.agent_overrides =
        vec![flotilla_protocol::AgentOverride { capability: "governor".into(), adapter: "codex".into(), model: Some("new-model".into()) }];
    ensures.apply(&InputMeta::from(&first.metadata), &changed).await.expect("change declaration");
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("report drift while keeping running work");
    let changes = ensures.get("quartermaster").await.expect("ensure").status.expect("status").config_drift.expect("drift").changes;
    for expected in ["repository added", "workflow", "placement", "agents"] {
        assert!(changes.iter().any(|change| change.contains(expected)), "missing {expected} in {changes:?}");
    }
    assert!(daemon.roll_convoy_ensure("flotilla", "quartermaster").await.is_err());
    let current = backend.using::<ResourceConvoy>("flotilla").get(&first_ref).await.expect("original generation remains");
    assert!(!current.status.expect("status").phase.is_terminal(), "invalid replacement must not end current work");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("generations").items.len(), 1);
}

pub async fn standing_ensure_applies_agent_overrides_to_the_admitted_workflow_snapshot(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    configure_standing_ensure_agent(
        &backend,
        vec![flotilla_protocol::AgentOverride {
            capability: "governor".to_string(),
            adapter: "codex".to_string(),
            model: Some("fable".to_string()),
        }],
    )
    .await;

    let workflow = admitted_standing_workflow(&daemon, &backend).await;
    let CrewSource::Agent { selector, .. } = &workflow.vessels[0].crew[0].source else { panic!("governor must remain an agent") };
    assert_eq!(selector.adapter.as_deref(), Some("codex"));
    assert_eq!(selector.model.as_deref(), Some("fable"));
}

pub async fn standing_ensure_without_agent_overrides_preserves_the_workflow_selector(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;

    let workflow = admitted_standing_workflow(&daemon, &backend).await;
    let CrewSource::Agent { selector, .. } = &workflow.vessels[0].crew[0].source else { panic!("governor must remain an agent") };
    assert_eq!(selector.adapter.as_deref(), Some("codex"));
    assert_eq!(selector.model, None);
}

pub async fn standing_readmission_writes_a_new_brief_artifact(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let first = fail_ensured_generation(&backend, &clock).await;
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("record dead generation");
    clock.advance(ChronoDuration::seconds(30));
    daemon.reconcile_convoy_ensures_once_with_backing_inspector("flotilla", &VerifiedDeadBacking).await.expect("readmit governor");
    let writes = writer.writes.lock().await;
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].0, first);
    assert_ne!(writes[0].0, writes[1].0);
    for (convoy, role, body) in writes.iter() {
        assert_eq!(role, "governor");
        assert!(String::from_utf8_lossy(body).contains(convoy));
        let admitted = backend.using::<ResourceConvoy>("flotilla").get(convoy).await.expect("admitted convoy");
        assert!(admitted.metadata.annotations.contains_key(BRIEF_ARTIFACTS_ANNOTATION));
    }
}

pub async fn admission_rejects_agent_roles_reused_across_vessels_before_writing_briefs(factory: &dyn EnsureScenarioController) {
    let (daemon, _backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    let agent = || {
        CrewSpec::builder()
            .role("coder".to_string())
            .source(CrewSource::Agent { selector: Selector::for_capability("coding"), prompt: None, brief_template: None })
            .build()
    };
    let workflow = WorkflowTemplateSpec::builder()
        .vessels(vec![
            VesselRequirement::builder().name("implement".to_string()).crew(vec![agent()]).build(),
            VesselRequirement::builder().name("verify".to_string()).crew(vec![agent()]).build(),
        ])
        .build();
    let spec = ConvoySpec::builder().workflow_ref("workflow".to_string()).build();
    let error = daemon.write_admission_briefs("flotilla", "convoy-ambiguous", &spec, &workflow).await.expect_err("duplicate role");
    assert!(error.contains("agent role `coder` occurs in vessels `implement` and `verify`"), "{error}");
    assert!(writer.writes.lock().await.is_empty(), "admission must not publish a partial set of briefs");
}

pub async fn generation_allocation_sees_live_and_terminal_replicated_convoys(factory: &dyn EnsureScenarioController) {
    let (daemon, source, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let writer = backend.replica_writer::<ResourceConvoy>(NodeId::new("other-root"), "flotilla");
    writer.replace(&source.using::<ResourceConvoy>("flotilla").list().await.expect("source convoys"), Utc::now()).await.expect("replicate");
    let error = allocate_convoy_generation(&backend, "flotilla", Some("standing-project"), "quartermaster")
        .await
        .expect_err("remote live generation blocks admission");
    assert!(error.contains("generation 1 already exists"), "{error}");
    assert!(error.contains("as of root other-root, last synced"), "{error}");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local reconciliation view").items.is_empty());

    fail_ensured_generation(&source, &clock).await;
    writer
        .replace(&source.using::<ResourceConvoy>("flotilla").list().await.expect("source history"), Utc::now())
        .await
        .expect("replicate terminal history");
    assert_eq!(
        allocate_convoy_generation(&backend, "flotilla", Some("standing-project"), "quartermaster").await.expect("next generation"),
        2
    );
}

pub async fn ensure_dependency_fingerprint_tracks_replicated_placement_changes(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let mut ensure = backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("ensure");
    ensure.spec.placement_policy = Some("remote-policy".to_string());
    let absent = factory.dependency_hash(&daemon, "flotilla", &ensure).await.expect("absent fingerprint");
    let source = ResourceBackend::InMemory(InMemoryBackend::default());
    let mut policy = placement_policy(&source, "remote-policy", "other-host").await;
    let writer = backend.replica_writer::<PlacementPolicy>(NodeId::new("other-root"), "flotilla");
    writer
        .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("policies"), Utc::now())
        .await
        .expect("replicate policy");
    let present = factory.dependency_hash(&daemon, "flotilla", &ensure).await.expect("replica fingerprint");
    assert_ne!(absent, present, "arrival must invalidate an admission refusal");
    policy.spec.priority = 42;
    source
        .using::<PlacementPolicy>("flotilla")
        .update(&InputMeta::from(&policy.metadata), &policy.metadata.resource_version, &policy.spec)
        .await
        .expect("change policy");
    writer
        .replace(&source.using::<PlacementPolicy>("flotilla").list().await.expect("updated policies"), Utc::now())
        .await
        .expect("replicate change");
    assert_ne!(present, factory.dependency_hash(&daemon, "flotilla", &ensure).await.expect("changed fingerprint"));
}

pub async fn replicated_ensure_is_not_reconciled_away_from_its_project_home(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    let project = backend.using::<Project>("flotilla").get("standing-project").await.expect("local project");
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("local ensure");

    backend.using::<ConvoyEnsure>("flotilla").delete("quartermaster").await.expect("remove local ensure");
    backend.using::<Project>("flotilla").delete("standing-project").await.expect("remove local project");

    let remote_root = NodeId::new("remote-root");
    let origin = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(remote_root.clone());
    origin.using::<Project>("flotilla").create(&InputMeta::from(&project.metadata), &project.spec).await.expect("remote project");
    origin.using::<ConvoyEnsure>("flotilla").create(&InputMeta::from(&ensure.metadata), &ensure.spec).await.expect("remote ensure");
    backend
        .replica_writer::<Project>(remote_root.clone(), "flotilla")
        .replace(&origin.using::<Project>("flotilla").list().await.expect("remote projects"), Utc::now())
        .await
        .expect("replicate project");
    backend
        .replica_writer::<ConvoyEnsure>(remote_root, "flotilla")
        .replace(&origin.using::<ConvoyEnsure>("flotilla").list().await.expect("remote ensures"), Utc::now())
        .await
        .expect("replicate ensure");

    assert!(backend.definitions::<Project>("flotilla").get("standing-project").await.is_ok(), "replicated project should be visible");
    assert!(backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.is_ok(), "replicated ensure should be visible");
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("skip remote ensure").is_empty());
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
}

pub async fn unavailable_declared_driver_surfaces_named_admission_conditions_without_fallback(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    set_ensure_driver(&backend, "missing-driver").await;
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("unknown driver skip").is_empty());
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("conditioned ensure");
    let condition = ensure
        .status
        .as_ref()
        .expect("ensure status")
        .conditions
        .iter()
        .find(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE)
        .expect("driver admission condition");
    assert_eq!(condition.reason, "UnknownDriver");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items.is_empty());

    let hosts = backend.using::<ResourceHost>("flotilla");
    hosts
        .create(
            &test_meta("missing-driver"),
            &HostSpec { display_name: "missing-driver".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("known but unreachable driver");
    assert!(daemon.reconcile_convoy_ensures_once("flotilla").await.expect("unreachable driver skip").is_empty());
    let ensure = backend.using::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("conditioned ensure");
    let condition = ensure
        .status
        .expect("ensure status")
        .conditions
        .into_iter()
        .find(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE)
        .expect("driver admission condition");
    assert_eq!(condition.reason, "DriverUnreachable");
}

pub async fn orphaned_ensure_reports_its_absent_parent_project(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    backend.definitions::<Project>("flotilla").delete("standing-project").await.expect("remove parent project");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("orphaned ensure must remain visible");
    assert_eq!(error, "ConvoyEnsure/quartermaster: parent Project/standing-project is absent");
    assert!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("local convoys").items.is_empty());
}

pub async fn statusless_ensured_generation_is_live_even_when_address_labels_are_missing(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial admission");

    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let ensure = ensures.get("quartermaster").await.expect("ensure");
    let convoy_ref = ensure.status.as_ref().and_then(|status| status.convoy_ref.clone()).expect("admitted generation");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let convoy = convoys.get(&convoy_ref).await.expect("statusless generation");
    convoys
        .update(
            &InputMeta::builder().name(convoy.metadata.name.clone()).annotations(convoy.metadata.annotations.clone()).build(),
            &convoy.metadata.resource_version,
            &convoy.spec,
        )
        .await
        .expect("simulate generation whose address labels have not materialized");
    ensures
        .update_status(
            &ensure.metadata.name,
            &ensure.metadata.resource_version,
            &ConvoyEnsureStatus {
                observed_config_hash: ensure.status.and_then(|status| status.observed_config_hash),
                ..Default::default()
            },
        )
        .await
        .expect("simulate lost ensure status update");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("rediscover admitted generation"),
        vec!["ConvoyEnsure/quartermaster observed running"]
    );
    assert_eq!(convoys.list().await.expect("convoys").items.len(), 1);
    assert_eq!(ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref), Some(convoy_ref));
}

pub async fn foreign_statusless_generation_at_ensure_address_blocks_admission_without_labels(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;
    backend
        .using::<ResourceConvoy>("flotilla")
        .create(
            &test_meta("foreign-generation"),
            &ConvoySpec::builder()
                .workflow_ref("quartermaster".to_string())
                .role("quartermaster".to_string())
                .generation(1)
                .project_ref("standing-project".to_string())
                .build(),
        )
        .await
        .expect("foreign statusless generation");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("foreign live address must block ensure admission");
    assert!(error.contains("live convoy quartermaster@standing-project already exists outside this ensure"), "{error}");
    assert_eq!(backend.using::<ResourceConvoy>("flotilla").list().await.expect("convoys").items.len(), 1);
}

pub async fn standing_ensure_admission_uses_default_branch_observed_only_on_non_driver_root(factory: &dyn EnsureScenarioController) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"root-b\"\n").expect("daemon config");
    let target = ResourceBackend::InMemory(InMemoryBackend::default());
    let daemon = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(temp.path())),
        fake_discovery(false),
        HostName::new("root-b"),
        target.clone(),
    )
    .await;
    daemon.install_convoy_ensure_reconciler(factory.create(daemon.resource_backend(), daemon.clock_for_scenarios())).await;
    let driver_ref = daemon.canonical_local_host_id().expect("root B host identity").to_string();
    target
        .using::<ResourceHost>("flotilla")
        .create(
            &test_meta(&driver_ref),
            &HostSpec { display_name: "root-b".to_string(), connection: Default::default(), ..HostSpec::default() },
        )
        .await
        .expect("root B host resource");
    let source = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let repository_spec = RepositorySpec::remote("https://github.com/acme/cross-root").expect("repository spec");
    let repository_key = repository_spec.key();
    let source_repository = source
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("source repository");
    source
        .using::<Repository>("flotilla")
        .update_status(
            &source_repository.metadata.name,
            &source_repository.metadata.resource_version,
            &RepositoryStatus { default_branch: Some("main".to_string()), ..Default::default() },
        )
        .await
        .expect("source default branch observation");
    target
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("driver repository without a local status observation");
    source
        .definitions::<Project>("flotilla")
        .create(
            &test_meta("cross-root-project"),
            &ProjectSpec::builder()
                .display_name("Cross Root Project".to_string())
                .default_workflow_ref("cross-root-workflow".to_string())
                .repositories(vec![ProjectRepositorySpec {
                    charter_store: None,
                    repo: repository_key.clone(),
                    alias: None,
                    roles: BTreeSet::from([ProjectRepositoryRole::Code]),
                    subpath: None,
                    default_branch: None,
                }])
                .build(),
        )
        .await
        .expect("source project");
    target
        .using::<WorkflowTemplate>("flotilla")
        .create(
            &test_meta("cross-root-workflow"),
            &WorkflowTemplateSpec::builder()
                .vessels(vec![VesselRequirement::builder()
                    .name("work".to_string())
                    .repository_refs(vec![repository_key.clone()])
                    .crew(Vec::new())
                    .build()])
                .build(),
        )
        .await
        .expect("driver-local workflow");
    source
        .definitions::<ConvoyEnsure>("flotilla")
        .create(
            &InputMeta::builder()
                .name("cross-root".to_string())
                .annotations(BTreeMap::from([
                    (MATERIALIZED_PROJECT_ANNOTATION.to_string(), "cross-root-project".to_string()),
                    (SOURCE_REPOSITORY_ANNOTATION.to_string(), repository_key.to_string()),
                    (SOURCE_COMMIT_ANNOTATION.to_string(), "abc123".to_string()),
                    (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), "ops/cross-root.md".to_string()),
                ]))
                .build(),
            &ConvoyEnsureSpec {
                project_ref: "cross-root-project".to_string(),
                role: "quartermaster".to_string(),
                driver_ref: Some(driver_ref),
                workflow_ref: "cross-root-workflow".to_string(),
                placement_policy: None,
                escalation_reason: None,
                repositories: vec![repository_key.clone()],
                presents_as: None,
                agent_overrides: Vec::new(),
            },
        )
        .await
        .expect("source ensure");

    let origin = NodeId::new("root-a");
    target
        .replica_writer::<Repository>(origin.clone(), "flotilla")
        .replace(&source.using::<Repository>("flotilla").list().await.expect("source repositories"), Utc::now())
        .await
        .expect("replicate repositories");
    target
        .replica_writer::<Project>(origin.clone(), "flotilla")
        .replace(&source.using::<Project>("flotilla").list().await.expect("source projects"), Utc::now())
        .await
        .expect("replicate projects");
    target
        .replica_writer::<ConvoyEnsure>(origin, "flotilla")
        .replace(&source.using::<ConvoyEnsure>("flotilla").list().await.expect("source ensures"), Utc::now())
        .await
        .expect("replicate ensures");

    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("cross-root ensure admission"),
        vec!["started quartermaster@cross-root-project"]
    );
    let convoy = target
        .using::<ResourceConvoy>("flotilla")
        .list()
        .await
        .expect("list admitted convoys")
        .items
        .into_iter()
        .next()
        .expect("admitted convoy on root B");
    assert_eq!(convoy.spec.project_ref.as_deref(), Some("cross-root-project"));

    let conflicting_source = ResourceBackend::InMemory(InMemoryBackend::default());
    let conflicting_repository = conflicting_source
        .using::<Repository>("flotilla")
        .create(&test_meta(&repository_key.to_string()), &repository_spec)
        .await
        .expect("conflicting source repository");
    conflicting_source
        .using::<Repository>("flotilla")
        .update_status(
            &conflicting_repository.metadata.name,
            &conflicting_repository.metadata.resource_version,
            &RepositoryStatus { default_branch: Some("trunk".to_string()), ..Default::default() },
        )
        .await
        .expect("conflicting default branch observation");
    target
        .replica_writer::<Repository>(NodeId::new("root-c"), "flotilla")
        .replace(&conflicting_source.using::<Repository>("flotilla").list().await.expect("conflicting repositories"), Utc::now())
        .await
        .expect("replicate conflicting repository status");

    let error = daemon
        .snapshot_project_repositories("flotilla", "cross-root-project", None)
        .await
        .expect_err("different non-driver observations must fail admission closed");
    assert!(error.contains("conflicting observed default branches"), "unexpected readiness error: {error}");
}

pub async fn duplicate_operational_entry_refusal_records_a_project_event(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, _clock, _temp) = standing_ensure_fixture(factory).await;

    daemon
        .record_project_operational_refusal("flotilla", "standing-project", "duplicate materialized WorkflowTemplate `quartermaster`")
        .await;

    let events = backend.using::<Event>("flotilla").list().await.expect("events").items;
    assert!(events.iter().any(|event| {
        event.spec.regarding.name == "standing-project"
            && event.spec.reason == "DuplicateOperationalEntryRefused"
            && event.spec.message == "duplicate materialized WorkflowTemplate `quartermaster`"
    }));
}

pub async fn abandoned_ensure_generation_survives_a_stale_reconcile_write_and_is_superseded(factory: &dyn EnsureScenarioController) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("initial ensure");
    let ensures = backend.using::<ConvoyEnsure>("flotilla");
    let first_ref =
        ensures.get("quartermaster").await.expect("ensure").status.and_then(|status| status.convoy_ref).expect("first convoy ref");
    let convoys = backend.using::<ResourceConvoy>("flotilla");
    let first = convoys.get(&first_ref).await.expect("first generation");
    let workflow_snapshot_ref =
        first.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION).cloned().expect("workflow archive pointer");

    let principal = PrincipalRef::implicit_for_namespace("flotilla");
    daemon
        .abandon_convoy_internal("flotilla", &first_ref, "operator requested replacement", Some(&principal))
        .await
        .expect("abandon generation");

    // This patch represents a reconcile that read the generation while it was
    // still active and lost the optimistic write race to the abandon command.
    // Retrying it against the newer status must not resurrect the generation.
    apply_resource_status_patch(&convoys, &first_ref, &controller_patches::roll_up_phase(ConvoyPhase::Active, Some(clock.now()), None))
        .await
        .expect("stale reconcile write");

    daemon.reconcile_convoy_ensures_once("flotilla").await.expect("observe abandoned generation");
    clock.advance(ChronoDuration::seconds(30));
    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("supersede abandoned generation"),
        vec!["started quartermaster@standing-project"]
    );

    let generations = convoys.list().await.expect("standing generations").items;
    assert_eq!(generations.len(), 2);
    let abandoned = generations.iter().find(|convoy| convoy.metadata.name == first_ref).expect("abandoned history");
    let abandoned_status = abandoned.status.as_ref().expect("abandoned status");
    assert_eq!(abandoned.spec.generation, 1);
    assert_eq!(abandoned_status.phase, ConvoyPhase::Abandoned);
    assert_eq!(abandoned_status.message.as_deref(), Some("abandoned by human override: operator requested replacement"));
    assert_eq!(abandoned.metadata.annotations.get(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION), Some(&workflow_snapshot_ref));
    assert!(generations.iter().any(|convoy| convoy.spec.generation == 2 && convoy.metadata.name != first_ref));
}

pub async fn standing_ensure_does_not_capture_another_projects_bare_workflow_but_accepts_a_global_builtin(
    factory: &dyn EnsureScenarioController,
) {
    let (daemon, backend, clock, _temp) = standing_ensure_fixture(factory).await;
    let own_name = flotilla_core::ops_entry::materialized_workflow_name("standing-project", "quartermaster");
    let own = backend.definitions::<WorkflowTemplate>("flotilla").get(&own_name).await.expect("own workflow");
    backend.definitions::<WorkflowTemplate>("flotilla").delete(&own_name).await.expect("remove own workflow");
    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(
            &InputMeta::builder()
                .name("quartermaster".to_string())
                .annotations(BTreeMap::from([(MATERIALIZED_PROJECT_ANNOTATION.to_string(), "other-project".to_string())]))
                .build(),
            &own.spec,
        )
        .await
        .expect("other project's workflow");

    let error = daemon.reconcile_convoy_ensures_once("flotilla").await.expect_err("cross-project bare name must not resolve");
    assert!(error.contains("workflow template quartermaster is materialized by another project"), "unexpected error: {error}");

    backend
        .definitions::<WorkflowTemplate>("flotilla")
        .apply(&test_meta("quartermaster"), &own.spec)
        .await
        .expect("global builtin workflow");
    clock.advance(ChronoDuration::minutes(1));
    assert_eq!(
        daemon.reconcile_convoy_ensures_once("flotilla").await.expect("global workflow admits"),
        vec!["started quartermaster@standing-project"]
    );
}

pub async fn off_home_driver_admits_an_ensure_from_replicated_project_definitions(factory: &dyn EnsureScenarioController) {
    let (_home, home_backend, _clock, _home_temp) = standing_ensure_fixture(factory).await;
    let driver_temp = tempfile::tempdir().expect("driver tempdir");
    std::fs::write(driver_temp.path().join("daemon.toml"), "machine_id = \"driver-test\"\n").expect("driver config");
    let driver_backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("driver-root"));
    let home_root = NodeId::new("home-root");
    driver_backend
        .replica_writer::<Project>(home_root.clone(), "flotilla")
        .replace(&home_backend.using::<Project>("flotilla").list().await.expect("home projects"), Utc::now())
        .await
        .expect("replicate projects");
    driver_backend
        .replica_writer::<WorkflowTemplate>(home_root.clone(), "flotilla")
        .replace(&home_backend.using::<WorkflowTemplate>("flotilla").list().await.expect("home workflows"), Utc::now())
        .await
        .expect("replicate workflows");
    for repository in home_backend.using::<Repository>("flotilla").list().await.expect("home repositories").items {
        driver_backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::from(&repository.metadata), &repository.spec)
            .await
            .expect("driver repository observation");
    }
    let driver = InProcessDaemon::new_with_resource_backend(
        Vec::new(),
        Arc::new(ConfigStore::with_base(driver_temp.path())),
        fake_discovery(false),
        HostName::new("driver"),
        driver_backend.clone(),
    )
    .await;
    driver.install_convoy_ensure_reconciler(factory.create(driver.resource_backend(), driver.clock_for_scenarios())).await;
    let ensure = home_backend.definitions::<ConvoyEnsure>("flotilla").get("quartermaster").await.expect("home ensure");

    factory.start(&driver, "flotilla", &ensure).await.expect("driver admits replicated template");

    let admitted = driver_backend.using::<ResourceConvoy>("flotilla").list().await.expect("driver convoys").items;
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].metadata.labels.get(PROJECT_LABEL).map(String::as_str), Some("standing-project"));
}

// #2719: local standing presence inherits fleet shape, freezes it for explain,
// and delivers local charter prose and its source commit in the brief artifact.
pub async fn standing_presence_inherits_role_shape_and_delivers_charter_artifact(factory: &dyn EnsureScenarioController) {
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, RoleDefinition};

    let (daemon, backend, _, _temp) = standing_ensure_fixture(factory).await;
    configure_standing_ensure_agent(&backend, Vec::new()).await;
    let projects = backend.definitions::<Project>("flotilla");
    let fleet = ProjectSpec::builder()
        .display_name("Fleet".into())
        .role_definitions(BTreeMap::from([
            ("quartermaster".into(), RoleDefinition { workflow: Some("quartermaster".into()), ..Default::default() }),
            (
                "governor".into(),
                RoleDefinition {
                    agent: Some("codex".into()),
                    brief_template: Some("{% block operating_instructions %}Fleet governor template{% endblock %}".into()),
                    ..Default::default()
                },
            ),
        ]))
        .build();
    projects.apply(&test_meta("fleet"), &fleet).await.expect("fleet");
    backend
        .definitions::<FleetDesignation>("flotilla")
        .apply(&test_meta("fleet"), &FleetDesignationSpec { project: "fleet".into(), image_cache: None })
        .await
        .expect("fleet designation");
    let mut project = projects.get("standing-project").await.expect("project");
    project.spec.default_workflow_ref.clear();
    project.spec.charter_prose.insert("governor".into(), "Govern the child project from this delivered charter.".into());
    let mut meta = InputMeta::from(&project.metadata);
    meta.annotations.insert(flotilla_core::project_declaration::BOOTSTRAP_COMMIT_ANNOTATION.into(), "charter-2719".into());
    projects.apply(&meta, &project.spec).await.expect("local charter");
    let ensures = backend.definitions::<ConvoyEnsure>("flotilla");
    let mut ensure = ensures.get("quartermaster").await.expect("presence");
    ensure.spec.workflow_ref.clear();
    ensures.apply(&InputMeta::from(&ensure.metadata), &ensure.spec).await.expect("presence only workflow");
    let live = daemon.explain_project_internal("standing-project").await.expect("project explain");
    assert_eq!(live["cascade"]["settings"]["roles.governor.brief_template"]["layer"], "project:fleet");
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    let frozen = admitted_standing_workflow(&daemon, &backend).await;
    let cascade = frozen.cascade.as_ref().expect("frozen defaults");
    assert_eq!(cascade.settings["roles.quartermaster.workflow"].layer, "project:fleet");
    let writes = writer.writes.lock().await;
    let (_, role, body) = &writes[0];
    assert_eq!(role, "governor");
    let rendered = String::from_utf8_lossy(body);
    assert!(rendered.contains("Fleet governor template"));
    assert!(rendered.contains("Govern the child project from this delivered charter."));
    assert!(rendered.contains("Charter revision: `charter-2719`"));
    let explanation = daemon.explain_convoy_internal(Some("flotilla"), &writes[0].0).await.expect("explain");
    assert_eq!(explanation.cascade.as_ref().expect("cascade")["charter_commit"], "charter-2719");
    project.spec.charter_prose.insert("governor".into(), "Later prose".into());
    projects.apply(&meta, &project.spec).await.expect("charter change");
    let explanation = daemon.explain_convoy_internal(Some("flotilla"), &writes[0].0).await.expect("frozen explain");
    assert_eq!(
        explanation.cascade.expect("frozen cascade")["charter"]["governor"],
        "Govern the child project from this delivered charter."
    );
}

// Admission's artifact address is convoy/role/subject, so duplicate agent roles
// across vessels must refuse before any write can replace another vessel's brief.
pub async fn admission_refuses_same_role_briefs_before_writing_any_artifact(factory: &dyn EnsureScenarioController) {
    let (daemon, _, _, _temp) = standing_ensure_fixture(factory).await;
    let writer = Arc::new(RecordingBriefArtifacts::default());
    daemon.set_brief_artifact_writer(writer.clone()).await;
    let mut workflow = flotilla_resources::single_agent_workflow_spec();
    let mut second = workflow.vessels[0].clone();
    second.name = "second".into();
    workflow.vessels.push(second);
    let spec = ConvoySpec::builder().workflow_ref("same-role".into()).build();
    let error = daemon.write_admission_briefs("flotilla", "same-role", &spec, &workflow).await.expect_err("collision refused");
    assert!(error.contains("convoy-wide unique roles"));
    assert!(writer.writes.lock().await.is_empty(), "refusal is before all artifact writes");
}
