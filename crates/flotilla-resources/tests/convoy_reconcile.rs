mod common;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use common::{
    bootstrapped_convoy_status, bootstrapped_tool_only_convoy_status, convoy_meta, convoy_object, pending_task_state,
    task_provisioning_convoy_spec, timestamp, tool_only_workflow_template_object, valid_convoy_spec, valid_workflow_template_object,
    workflow_template_meta,
};
use flotilla_resources::{
    change_request_record_name,
    controller::{Actuation, Reconciler},
    controller_patches, interactive_single_workflow_spec, reconcile, BoundChangeRequest, ChangeRequest, ChangeRequestReviewObservation,
    ChangeRequestSpec, ChangeRequestStatus, Checkout, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus,
    CheckoutWorktreeSpec, Clock, ConditionValue, Convoy, ConvoyEvent, ConvoyPhase, ConvoyReconciler, ConvoyStatus, ConvoyStatusPatch,
    ConvoyTeardownRuntime, CrewSource, CrewWorkPhase, InMemoryBackend, InputMeta, InputValue, IntegrationCondition, LandedEvidence,
    LifecycleAuthority, Observation, ObservedChangeRequestState, ObservedCheckoutSpec, ObservedChecks, ObservedMergeability,
    OwnerReference, Presentation, PresentationSpec, RepositoryKey, ResourceBackend, StatusPatch, TargetMismatch, TerminalSession,
    TerminalSessionSource, TerminalSessionSpec, ValidationError, Vessel, VesselPhase, VesselSpec, VesselStatus, WorkCompletionAuthority,
    WorkPhase, WorkflowSnapshot, WorkflowTemplate, CONVOY_LABEL, VESSEL_LABEL,
};

struct AlwaysEligible;

#[async_trait]
impl ConvoyTeardownRuntime for AlwaysEligible {
    async fn verify_reclaim(
        &self,
        _convoy: &flotilla_resources::ResourceObject<Convoy>,
        _checkouts: &[flotilla_resources::ResourceObject<Checkout>],
    ) -> Result<(), String> {
        Ok(())
    }
}

async fn reconcile_once_with_resources(
    convoy: &flotilla_resources::ResourceObject<Convoy>,
    template: Option<&flotilla_resources::ResourceObject<WorkflowTemplate>>,
    workspaces: Vec<flotilla_resources::ResourceObject<Vessel>>,
    presentations: Vec<flotilla_resources::ResourceObject<Presentation>>,
    now: chrono::DateTime<chrono::Utc>,
) -> flotilla_resources::controller::ReconcileOutcome<Convoy> {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let vessels = backend.clone().using::<Vessel>("flotilla");
    let presentations_resolver = backend.clone().using::<Presentation>("flotilla");

    if let Some(template) = template {
        templates.create(&workflow_template_meta(&template.metadata.name), &template.spec).await.expect("template create should succeed");
    }

    let created = convoys.create(&convoy_meta(&convoy.metadata.name), &convoy.spec).await.expect("convoy create should succeed");
    if let Some(status) = convoy.status.as_ref() {
        convoys
            .update_status(&convoy.metadata.name, &created.metadata.resource_version, status)
            .await
            .expect("convoy status update should succeed");
    }

    for workspace in workspaces {
        let created = vessels
            .create(&vessel_meta(&workspace.metadata.name, &workspace.spec.convoy_ref, &workspace.spec.vessel_name), &workspace.spec)
            .await
            .expect("workspace create should succeed");
        if let Some(status) = workspace.status.as_ref() {
            vessels
                .update_status(&workspace.metadata.name, &created.metadata.resource_version, status)
                .await
                .expect("workspace status update should succeed");
        }
    }

    for presentation in presentations {
        let created = presentations_resolver
            .create(
                &presentation_meta(&presentation.metadata.name, &presentation.spec.convoy_ref, &presentation.spec.name),
                &presentation.spec,
            )
            .await
            .expect("presentation create should succeed");
        if let Some(status) = presentation.status.as_ref() {
            presentations_resolver
                .update_status(&presentation.metadata.name, &created.metadata.resource_version, status)
                .await
                .expect("presentation status update should succeed");
        }
    }

    let current = convoys.get(&convoy.metadata.name).await.expect("convoy get should succeed");
    let reconciler = ConvoyReconciler::new(templates.clone())
        .with_vessels(vessels.clone())
        .with_presentations(presentations_resolver.clone())
        .with_teardown_runtime(Arc::new(AlwaysEligible));
    let deps = reconciler.fetch_dependencies(&current).await.expect("dependency fetch should succeed");
    reconciler.reconcile(&current, &deps, now)
}

struct FixedClock(chrono::DateTime<chrono::Utc>);

impl Clock for FixedClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.0
    }
}

fn merged_change_request_status(observed_at: chrono::DateTime<chrono::Utc>) -> ChangeRequestStatus {
    ChangeRequestStatus {
        state: Observation::known(ObservedChangeRequestState::Merged, observed_at),
        head_sha: Observation::known("abc123".to_string(), observed_at),
        checks: Observation::known(ObservedChecks::Pass, observed_at),
        review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, observed_at) },
        mergeable: Observation::known(ObservedMergeability::Mergeable, observed_at),
    }
}

async fn reconcile_with_observed_change_request(
    phase: ConvoyPhase,
    condition: Option<ConditionValue>,
    observed_target_ref: Option<&str>,
    observed_at: chrono::DateTime<chrono::Utc>,
) -> flotilla_resources::controller::ReconcileOutcome<Convoy> {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let checkouts = backend.clone().using::<Checkout>("flotilla");
    let mut status = bootstrapped_convoy_status();
    status.phase = phase;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(RepositoryKey("repo-a".to_string()), "checkout-a".to_string())])),
        )]),
    });
    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://example.com/repo-a".to_string())
        .repo_ref(RepositoryKey("repo-a".to_string()))
        .source_ref("main".to_string())
        .target_ref("main".to_string())
        .workspace_slug("repo-a".to_string())
        .subpaths(Vec::new())
        .build()];
    let source = convoy_object("convoy-a", spec, Some(status));
    let created = convoys.create(&convoy_meta("convoy-a"), &source.spec).await.expect("convoy create");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, source.status.as_ref().expect("status"))
        .await
        .expect("convoy status");

    if let Some(value) = condition {
        let meta = InputMeta {
            name: "checkout-a".to_string(),
            labels: BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]),
            ..Default::default()
        };
        let checkout = checkouts
            .create(
                &meta,
                &CheckoutSpec::Observed(ObservedCheckoutSpec {
                    r#ref: "feature/lifecycle".to_string(),
                    path: "/tmp/checkout-a".to_string(),
                    repo_ref: RepositoryKey("repo-a".to_string()),
                    host_ref: "host-a".to_string(),
                    is_main: false,
                }),
            )
            .await
            .expect("checkout create");
        checkouts
            .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
                phase: CheckoutPhase::Ready,
                path: Some("/tmp/checkout-a".to_string()),
                commit: None,
                branch_provenance: Default::default(),
                integration: CheckoutIntegrationStatus {
                    clean: Default::default(),
                    pushed: Default::default(),
                    landed: IntegrationCondition::builder().value(value).observed_at(observed_at.to_rfc3339()).build(),
                    landed_evidence: observed_target_ref.map(|target_ref| {
                        LandedEvidence::builder().change_request_id("42".to_string()).target_ref(target_ref.to_string()).build()
                    }),
                    change_request: None,
                },
                message: None,
            })
            .await
            .expect("checkout status");
    }

    let current = convoys.get("convoy-a").await.expect("convoy");
    let reconciler = ConvoyReconciler::new(templates).with_checkouts(checkouts).with_clock(Arc::new(FixedClock(timestamp(40))));
    let deps = reconciler.fetch_dependencies(&current).await.expect("dependencies");
    reconciler.reconcile(&current, &deps, timestamp(40))
}

#[tokio::test]
async fn convoy_finalizer_deletes_orphaned_terminal_sessions() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let sessions = backend.clone().using::<TerminalSession>("flotilla");
    let convoy = convoys.create(&convoy_meta("convoy-a"), &valid_convoy_spec()).await.expect("convoy create should succeed");
    sessions
        .create(
            &InputMeta::builder()
                .name("terminal-convoy-a-work-coder".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.metadata.name.clone())]))
                .build(),
            &TerminalSessionSpec {
                env_ref: "host-direct-feta".to_string(),
                role: "coder".to_string(),
                source: TerminalSessionSource::Tool { command: "cargo test".to_string() },
                cwd: "/workspace".to_string(),
                pool: "cleat".to_string(),
            },
        )
        .await
        .expect("terminal create should succeed");

    ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla"))
        .with_terminal_sessions(sessions.clone())
        .run_finalizer(&convoy)
        .await
        .expect("convoy finalizer should succeed");

    assert!(matches!(sessions.get("terminal-convoy-a-work-coder").await, Err(flotilla_resources::ResourceError::NotFound { .. })));
}

fn vessel_meta(name: &str, convoy_name: &str, task: &str) -> InputMeta {
    let repository_key =
        flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository identity").key();
    InputMeta {
        name: name.to_string(),
        labels: [
            ("flotilla.work/convoy".to_string(), convoy_name.to_string()),
            ("flotilla.work/vessel".to_string(), task.to_string()),
            ("flotilla.work/repo-key".to_string(), repository_key.to_string()),
        ]
        .into_iter()
        .collect(),
        annotations: BTreeMap::new(),
        owner_references: vec![OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Convoy".to_string(),
            name: convoy_name.to_string(),
            controller: true,
        }],
        finalizers: Vec::new(),
        deletion_timestamp: None,
    }
}

fn vessel_object(convoy_name: &str, task: &str, phase: VesselPhase, message: Option<&str>) -> flotilla_resources::ResourceObject<Vessel> {
    vessel_object_with_image_digest(convoy_name, task, phase, message, "sha256:first")
}

fn vessel_object_with_image_digest(
    convoy_name: &str,
    task: &str,
    phase: VesselPhase,
    message: Option<&str>,
    image_digest: &str,
) -> flotilla_resources::ResourceObject<Vessel> {
    let repository_key =
        flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository identity").key();
    flotilla_resources::ResourceObject {
        metadata: common::object_meta(&format!("{convoy_name}-{task}"), "flotilla", "17"),
        spec: VesselSpec {
            convoy_ref: convoy_name.to_string(),
            vessel_name: task.to_string(),
            placement_policy_ref: "laptop-docker".to_string(),
            adopted_checkout_refs: Default::default(),
        },
        status: Some(VesselStatus {
            placement_decision: None,
            phase,
            message: message.map(str::to_string),
            observed_policy_ref: Some("laptop-docker".to_string()),
            observed_policy_version: Some("19".to_string()),
            environment_ref: Some(format!("env-{task}")),
            image_ref: Some("registry.example/crew:latest".to_string()),
            image_digest: Some(image_digest.to_string()),
            checkout_refs: BTreeMap::from([(repository_key, format!("checkout-{task}"))]),
            terminal_session_refs: vec![format!("terminal-{task}-coder")],
            interrupted_roles: (phase == VesselPhase::Interrupted).then(|| "coder".to_string()).into_iter().collect(),
            started_at: Some(timestamp(16)),
            ready_at: (phase == VesselPhase::Ready).then(|| timestamp(18)),
            requested_stance: None,
            effective_stance: None,
        }),
    }
}

fn presentation_meta(name: &str, convoy_name: &str, task: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string()), (VESSEL_LABEL.to_string(), task.to_string())]))
        .owner_references(vec![OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Convoy".to_string(),
            name: convoy_name.to_string(),
            controller: true,
        }])
        .build()
}

fn presentation_object(convoy_name: &str, task: &str) -> flotilla_resources::ResourceObject<Presentation> {
    flotilla_resources::ResourceObject {
        metadata: common::object_meta(&format!("{convoy_name}-{task}"), "flotilla", "23"),
        spec: PresentationSpec {
            convoy_ref: convoy_name.to_string(),
            presentation_policy_ref: "default".to_string(),
            name: task.to_string(),
            process_selector: BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy_name.to_string()),
                (VESSEL_LABEL.to_string(), task.to_string()),
            ]),
        },
        status: None,
    }
}

fn mark_crew_done(status: &mut flotilla_resources::ConvoyStatus, vessel: &str, role: &str) {
    let crew = status.crew_work.get_mut(vessel).expect("vessel crew work");
    let member = crew.get_mut(role).expect("crew member work");
    member.phase = CrewWorkPhase::Done;
    member.started_at = Some(timestamp(7));
    member.finished_at = Some(timestamp(8));
}

#[test]
fn bootstrap_from_valid_template_returns_bootstrap_patch() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let template = tool_only_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let expected_snapshot = flotilla_resources::WorkflowSnapshot {
        exit: template.spec.exit.clone(),
        vessels: template
            .spec
            .vessels
            .iter()
            .map(|task| flotilla_resources::VesselRequirement {
                name: task.name.clone(),
                stance: task.stance,
                repository_refs: task.repository_refs.clone(),
                credential_refs: task.credential_refs.clone(),
                credential_scopes: task.credential_scopes.clone(),
                depends_on: task.depends_on.clone(),
                crew: task.crew.clone(),
            })
            .collect(),
    };
    let expected_tasks =
        [("implement".to_string(), pending_task_state()), ("review".to_string(), pending_task_state())].into_iter().collect();
    let expected_crew_work = BTreeMap::from([("implement".to_string(), BTreeMap::new()), ("review".to_string(), BTreeMap::new())]);
    let expected_patch = controller_patches::bootstrap(
        expected_snapshot,
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        expected_tasks,
        expected_crew_work,
        ConvoyPhase::Pending,
        None,
    );

    assert_eq!(outcome.patch, Some(expected_patch));
    assert!(outcome.events.is_empty());
}

#[test]
fn existing_convoy_backfills_agent_work_without_inventing_tool_state() {
    let mut status = bootstrapped_convoy_status();
    status.crew_work.clear();
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement work").started_at = Some(timestamp(9));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(10));
    let Some(ConvoyStatusPatch::BackfillCrewWork { crew_work, completion_overrides }) = outcome.patch else {
        panic!("expected crew work backfill");
    };

    assert_eq!(crew_work["implement"].keys().map(String::as_str).collect::<Vec<_>>(), vec!["coder"]);
    assert_eq!(crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(crew_work["implement"]["coder"].started_at, Some(timestamp(9)));
    assert_eq!(crew_work["review"].keys().map(String::as_str).collect::<Vec<_>>(), vec!["reviewer"]);
    assert!(completion_overrides.is_empty());
}

#[test]
fn existing_completed_work_backfills_as_a_rollup_override() {
    let mut status = bootstrapped_convoy_status();
    status.crew_work.clear();
    let implement = status.work.get_mut("implement").expect("implement work");
    implement.phase = WorkPhase::Complete;
    implement.finished_at = Some(timestamp(9));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(10));
    let Some(ConvoyStatusPatch::BackfillCrewWork { crew_work, completion_overrides }) = outcome.patch else {
        panic!("expected crew work backfill");
    };

    assert_eq!(crew_work["implement"]["coder"].phase, CrewWorkPhase::Pending);
    assert!(completion_overrides.contains("implement"));
}

#[test]
fn bootstrap_interpolates_tool_process_commands() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let mut template = tool_only_workflow_template_object("review-and-fix");
    if let CrewSource::Tool { command } = &mut template.spec.vessels[0].crew[0].source {
        *command = "printf '{{workflow.namespace}}/{{workflow.name}}/{{inputs.feature}}/{{inputs.branch}}/{{.metadata.name}}'".to_string();
    }

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let Some(ConvoyStatusPatch::Bootstrap { workflow_snapshot, .. }) = outcome.patch else {
        panic!("expected bootstrap patch");
    };
    let CrewSource::Tool { command } = &workflow_snapshot.vessels[0].crew[0].source else {
        panic!("expected tool process");
    };
    assert_eq!(command, "printf 'flotilla/convoy-a/Retry logic/fix-retry-logic/{{.metadata.name}}'");
}

#[test]
fn missing_template_fails_init() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);

    let outcome = reconcile(&convoy, None, timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::TemplateNotFound { name }] if name == "review-and-fix"
    ));
}

#[test]
fn invalid_template_fails_init_with_validation_error_event() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let mut template = valid_workflow_template_object("review-and-fix");
    template.spec.vessels[1].depends_on = vec!["missing".to_string()];

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::TemplateInvalid { name, errors }]
            if name == "review-and-fix"
                && matches!(errors.as_slice(), [ValidationError::UnknownDependency { vessel, missing }] if vessel == "review" && missing == "missing")
    ));
}

#[test]
fn missing_required_input_fails_init() {
    let mut spec = valid_convoy_spec();
    spec.inputs.remove("branch");
    let convoy = convoy_object("convoy-a", spec, None);
    let template = tool_only_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::MissingInput { name }] if name == "branch"
    ));
}

#[test]
fn extra_input_is_allowed() {
    let mut spec = valid_convoy_spec();
    spec.inputs.insert("extra".to_string(), InputValue::String("ignored".to_string()));
    let convoy = convoy_object("convoy-a", spec, None);
    let template = tool_only_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::Bootstrap { .. })));
    assert!(outcome.events.is_empty());
}

#[test]
fn fan_out_advances_all_newly_ready_tasks() {
    let spec = valid_convoy_spec();
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        exit: None,
        vessels: vec![
            flotilla_resources::VesselRequirement {
                name: "a".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "b".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "c".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
        ],
    });
    status.crew_work =
        BTreeMap::from([("a".to_string(), BTreeMap::new()), ("b".to_string(), BTreeMap::new()), ("c".to_string(), BTreeMap::new())]);
    status.work =
        [("a".to_string(), pending_task_state()), ("b".to_string(), pending_task_state()), ("c".to_string(), pending_task_state())]
            .into_iter()
            .collect();

    let convoy = convoy_object("convoy-a", spec, Some(status));
    let outcome = reconcile(&convoy, None, timestamp(20));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_work_to_ready(
            [("a".to_string(), timestamp(20)), ("b".to_string(), timestamp(20)), ("c".to_string(), timestamp(20)),].into_iter().collect()
        ))
    );
}

#[test]
fn fan_in_waits_until_all_dependencies_complete() {
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        exit: None,
        vessels: vec![
            flotilla_resources::VesselRequirement {
                name: "implement".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "verify".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "review".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: vec!["implement".to_string(), "verify".to_string()],
                crew: Vec::new(),
            },
        ],
    });
    status.crew_work = BTreeMap::from([
        ("implement".to_string(), BTreeMap::new()),
        ("verify".to_string(), BTreeMap::new()),
        ("review".to_string(), BTreeMap::new()),
    ]);
    status.work.insert("verify".to_string(), pending_task_state());
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Complete;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    status.work.get_mut("verify").expect("verify").phase = WorkPhase::Running;
    status.work.get_mut("verify").expect("verify").started_at = Some(timestamp(9));
    status.work.get_mut("review").expect("review").phase = WorkPhase::Pending;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status.clone()));

    let first = reconcile(&convoy, None, timestamp(20));
    assert_eq!(first.patch, Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(timestamp(20)), None)));

    status.work.get_mut("verify").expect("verify").phase = WorkPhase::Complete;
    status.work.get_mut("verify").expect("verify").finished_at = Some(timestamp(10));
    status.phase = ConvoyPhase::Active;

    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));
    let second = reconcile(&convoy, None, timestamp(21));

    assert_eq!(
        second.patch,
        Some(controller_patches::advance_work_to_ready([("review".to_string(), timestamp(21))].into_iter().collect()))
    );
}

#[test]
fn failed_task_triggers_fail_fast() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Failed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.work.get_mut("implement").expect("implement").message = Some("agent adapter codex unavailable".to_string());
    status.work.get_mut("review").expect("review").phase = WorkPhase::Running;
    status.work.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::fail_convoy(
            [("review".to_string(), timestamp(30))].into_iter().collect(),
            timestamp(30),
            Some("agent adapter codex unavailable".to_string())
        ))
    );
}

#[test]
fn reconciler_does_not_write_the_completion_claim_edge() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(12));
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(40));

    assert_eq!(outcome.patch, None);
}

#[test]
fn landing_convoy_with_no_declared_exit_never_settles() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.workflow_snapshot.as_mut().expect("workflow snapshot").exit = None;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    let convoy = convoy_object("standing", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(40));

    assert_eq!(outcome.patch, None, "an absent exit must not synthesize a Landed transition");
}

#[tokio::test]
async fn landing_with_open_change_request_stays_warm() {
    let outcome = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(ConditionValue::False), None, timestamp(40)).await;

    assert_eq!(outcome.patch, None);
    assert!(!outcome.actuations.iter().any(|actuation| matches!(
        actuation,
        Actuation::DeletePresentation { .. } | Actuation::DeleteVessel { .. } | Actuation::DeleteCheckout { .. }
    )));
}

#[tokio::test]
async fn landing_with_settled_change_request_becomes_landed() {
    let outcome =
        reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(ConditionValue::True), Some("main"), timestamp(40)).await;

    assert_eq!(outcome.patch, Some(controller_patches::settle("merged".to_string(), Vec::new(), timestamp(40))));
}

#[tokio::test]
async fn landing_on_a_different_target_records_a_fact_and_still_becomes_landed() {
    let outcome =
        reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(ConditionValue::True), Some("release"), timestamp(40)).await;

    let expected_mismatch = TargetMismatch::builder()
        .repo_ref(RepositoryKey("repo-a".to_string()))
        .change_request_id("42".to_string())
        .declared_target_ref("main".to_string())
        .observed_target_ref("release".to_string())
        .build();
    assert_eq!(outcome.patch, Some(controller_patches::settle("merged".to_string(), vec![expected_mismatch.clone()], timestamp(40))));
    let mut status = ConvoyStatus { phase: ConvoyPhase::Landing, ..Default::default() };
    outcome.patch.expect("settlement patch").apply(&mut status);
    assert_eq!(status.phase, ConvoyPhase::Landed);
    assert_eq!(status.target_mismatches, [expected_mismatch]);
}

#[tokio::test]
async fn landing_without_checkout_evidence_stays_landing() {
    let outcome = reconcile_with_observed_change_request(ConvoyPhase::Landing, None, None, timestamp(40)).await;

    assert_eq!(outcome.patch, None);
}

#[tokio::test]
async fn landing_holds_on_stale_vacuous_landed_evidence() {
    let outcome = reconcile_with_observed_change_request(ConvoyPhase::Landing, Some(ConditionValue::True), None, timestamp(9)).await;

    assert_eq!(outcome.patch, None);
}

#[tokio::test]
async fn terminal_bound_change_request_settles_checkout_without_own_landed_evidence() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let checkouts = backend.clone().using::<Checkout>("flotilla");
    let change_requests = backend.clone().using::<ChangeRequest>("flotilla");
    let repo_ref = RepositoryKey("repo-a".to_string());

    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(repo_ref.clone(), "checkout-a".to_string())])),
        )]),
    });

    let mut spec = valid_convoy_spec();
    spec.repositories = vec![flotilla_resources::ConvoyRepositorySpec::builder()
        .url("https://example.com/repo-a".to_string())
        .repo_ref(repo_ref.clone())
        .source_ref("main".to_string())
        .target_ref("main".to_string())
        .workspace_slug("repo-a".to_string())
        .subpaths(Vec::new())
        .build()];
    spec.change_request = Some(
        BoundChangeRequest::builder()
            .id("42".to_string())
            .repository_ref(repo_ref.clone())
            .title("bound terminal change request".to_string())
            .build(),
    );
    let source = convoy_object("convoy-a", spec, Some(status));
    let created = convoys.create(&convoy_meta("convoy-a"), &source.spec).await.expect("convoy create");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, source.status.as_ref().expect("convoy status"))
        .await
        .expect("convoy status update");

    let checkout = checkouts
        .create(
            &InputMeta {
                name: "checkout-a".to_string(),
                labels: BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]),
                ..Default::default()
            },
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "feature/bound-cr".to_string(),
                path: "/tmp/checkout-a".to_string(),
                repo_ref: repo_ref.clone(),
                host_ref: "host-a".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("checkout create");
    checkouts
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/tmp/checkout-a".to_string()),
            commit: None,
            branch_provenance: Default::default(),
            integration: CheckoutIntegrationStatus {
                landed: IntegrationCondition::builder().value(ConditionValue::False).observed_at(timestamp(40).to_rfc3339()).build(),
                ..Default::default()
            },
            message: None,
        })
        .await
        .expect("checkout status update");

    let record_name = change_request_record_name("example.com", "repo-a", 42);
    let record = change_requests
        .create(
            &InputMeta::builder().name(record_name.clone()).build(),
            &ChangeRequestSpec::builder()
                .service("example.com".to_string())
                .scope("repo-a".to_string())
                .number(42)
                .observing_authority("host-a".to_string())
                .build(),
        )
        .await
        .expect("change request create");
    change_requests
        .update_status(&record_name, &record.metadata.resource_version, &merged_change_request_status(timestamp(40)))
        .await
        .expect("publish terminal change request");

    let current = convoys.get("convoy-a").await.expect("convoy get");
    let reconciler = ConvoyReconciler::new(templates)
        .with_checkouts(checkouts)
        .with_change_requests(backend.including_replicas::<ChangeRequest>("flotilla"), std::time::Duration::from_secs(180))
        .with_clock(Arc::new(FixedClock(timestamp(40))));
    let deps = reconciler.fetch_dependencies(&current).await.expect("dependencies");
    let outcome = reconciler.reconcile(&current, &deps, timestamp(40));

    assert_eq!(outcome.patch, Some(controller_patches::settle("merged".to_string(), Vec::new(), timestamp(40))));
}

#[tokio::test]
async fn landed_with_reopened_change_request_does_not_write_phase() {
    let outcome = reconcile_with_observed_change_request(ConvoyPhase::Landed, Some(ConditionValue::False), None, timestamp(40)).await;

    assert_eq!(outcome.patch, None);
    assert!(outcome.events.is_empty());
}

#[tokio::test]
async fn federated_open_checkout_holds_landing_on_authority_host() {
    let authority = ResourceBackend::InMemory(InMemoryBackend::default());
    let remote = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = authority.clone().using::<Convoy>("flotilla");
    let remote_checkouts = remote.clone().using::<Checkout>("flotilla");
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    for work in status.work.values_mut() {
        work.phase = WorkPhase::Complete;
    }
    for crew in status.crew_work.values_mut() {
        for member in crew.values_mut() {
            member.phase = CrewWorkPhase::Done;
        }
    }
    status.work.get_mut("implement").expect("implement work").placement = Some(flotilla_resources::PlacementStatus {
        fields: BTreeMap::from([(
            "checkout_refs".to_string(),
            serde_json::json!(BTreeMap::from([(RepositoryKey("repo-a".to_string()), "remote-checkout".to_string())])),
        )]),
    });
    let source = convoy_object("cross-host", valid_convoy_spec(), Some(status));
    let created = convoys.create(&convoy_meta("cross-host"), &source.spec).await.expect("create authority convoy");
    convoys
        .update_status("cross-host", &created.metadata.resource_version, source.status.as_ref().expect("convoy status"))
        .await
        .expect("set landing status");

    let checkout = remote_checkouts
        .create(
            &InputMeta::builder()
                .name("remote-checkout".to_string())
                .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "cross-host".to_string())]))
                .build(),
            &CheckoutSpec::Observed(ObservedCheckoutSpec {
                r#ref: "feature/open-pr".to_string(),
                path: "/remote/worktree".to_string(),
                repo_ref: RepositoryKey("repo-a".to_string()),
                host_ref: "feta".to_string(),
                is_main: false,
            }),
        )
        .await
        .expect("create remote checkout");
    remote_checkouts
        .update_status(&checkout.metadata.name, &checkout.metadata.resource_version, &CheckoutStatus {
            phase: CheckoutPhase::Ready,
            path: Some("/remote/worktree".to_string()),
            commit: None,
            branch_provenance: Default::default(),
            integration: CheckoutIntegrationStatus {
                landed: IntegrationCondition::builder().value(ConditionValue::False).build(),
                ..Default::default()
            },
            message: None,
        })
        .await
        .expect("record open change request");
    authority
        .replica_writer::<Checkout>(flotilla_protocol::NodeId::new("feta"), "flotilla")
        .replace(&remote_checkouts.list().await.expect("list remote checkouts"), chrono::Utc::now())
        .await
        .expect("replicate remote checkout");

    let current = convoys.get("cross-host").await.expect("get authority convoy");
    let reconciler = ConvoyReconciler::new(authority.clone().using::<WorkflowTemplate>("flotilla"))
        .with_federated_checkouts(authority.including_replicas::<Checkout>("flotilla"));
    let deps = reconciler.fetch_dependencies(&current).await.expect("resolve federated dependencies");
    let outcome = reconciler.reconcile(&current, &deps, timestamp(40));

    assert_eq!(outcome.patch, None, "an open remote change request must hold Landing");
}

#[test]
fn terminal_landed_convoy_reconciles_to_noop() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landed;
    status.finished_at = Some(timestamp(40));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(12));
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(41));

    assert_eq!(outcome.patch, None);
    assert!(outcome.events.is_empty());
}

#[test]
fn terminal_failed_convoy_reconciles_to_noop() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Failed;
    status.finished_at = Some(timestamp(30));
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Failed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.work.get_mut("review").expect("review").phase = WorkPhase::Cancelled;
    status.work.get_mut("review").expect("review").finished_at = Some(timestamp(30));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(31));

    assert_eq!(outcome.patch, None);
    assert!(outcome.events.is_empty());
}

#[test]
fn terminal_failed_init_convoy_reconciles_to_noop() {
    let mut status = common::convoy_status(ConvoyPhase::Failed);
    status.message = Some("missing input 'branch'".to_string());
    status.finished_at = Some(timestamp(30));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, Some(&tool_only_workflow_template_object("review-and-fix")), timestamp(31));

    assert_eq!(outcome.patch, None);
    assert!(outcome.events.is_empty());
}

#[test]
fn advancing_ready_tasks_emits_task_phase_change_events() {
    let spec = valid_convoy_spec();
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        exit: None,
        vessels: vec![
            flotilla_resources::VesselRequirement {
                name: "a".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "b".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
            flotilla_resources::VesselRequirement {
                name: "c".to_string(),
                stance: Default::default(),
                repository_refs: None,
                credential_refs: Default::default(),
                credential_scopes: Default::default(),
                depends_on: Vec::new(),
                crew: Vec::new(),
            },
        ],
    });
    status.work =
        [("a".to_string(), pending_task_state()), ("b".to_string(), pending_task_state()), ("c".to_string(), pending_task_state())]
            .into_iter()
            .collect();

    let convoy = convoy_object("convoy-a", spec, Some(status));
    let outcome = reconcile(&convoy, None, timestamp(20));

    assert!(matches!(
        outcome.events.as_slice(),
        [
            ConvoyEvent::WorkPhaseChanged { work: a, from: WorkPhase::Pending, to: WorkPhase::Ready },
            ConvoyEvent::WorkPhaseChanged { work: b, from: WorkPhase::Pending, to: WorkPhase::Ready },
            ConvoyEvent::WorkPhaseChanged { work: c, from: WorkPhase::Pending, to: WorkPhase::Ready },
        ] if a == "a" && b == "b" && c == "c"
    ));
}

#[test]
fn fail_fast_emits_phase_and_task_phase_change_events() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Failed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.work.get_mut("review").expect("review").phase = WorkPhase::Running;
    status.work.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert!(matches!(
        outcome.events.as_slice(),
        [
            ConvoyEvent::PhaseChanged { from: ConvoyPhase::Active, to: ConvoyPhase::Failed },
            ConvoyEvent::WorkPhaseChanged { work, from: WorkPhase::Running, to: WorkPhase::Cancelled },
        ] if work == "review"
    ));
}

#[test]
fn roll_up_to_active_emits_phase_change_event() {
    let mut status = bootstrapped_convoy_status();
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Complete;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    mark_crew_done(&mut status, "implement", "coder");
    status.work.get_mut("review").expect("review").phase = WorkPhase::Running;
    status.work.get_mut("review").expect("review").started_at = Some(timestamp(9));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(20));

    assert!(matches!(outcome.events.as_slice(), [ConvoyEvent::PhaseChanged { from: ConvoyPhase::Pending, to: ConvoyPhase::Active }]));
}

#[test]
fn workflow_ref_change_after_init_fails_defensively() {
    let mut spec = valid_convoy_spec();
    spec.workflow_ref = "new-template".to_string();
    let convoy = convoy_object("convoy-a", spec, Some(bootstrapped_convoy_status()));

    let outcome = reconcile(&convoy, None, timestamp(50));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::WorkflowRefChanged { from, to }] if from == "review-and-fix" && to == "new-template"
    ));
}

#[test]
fn snapshot_state_allows_advancement_without_template() {
    let mut status = bootstrapped_convoy_status();
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Complete;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    mark_crew_done(&mut status, "implement", "coder");
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(60));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_work_to_ready([("review".to_string(), timestamp(60))].into_iter().collect()))
    );
}

#[test]
fn bootstrap_preserves_agent_processes_for_runtime_resolution() {
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let Some(ConvoyStatusPatch::Bootstrap { workflow_snapshot, .. }) = outcome.patch else {
        panic!("agent workflow should bootstrap");
    };
    let CrewSource::Agent { selector, prompt, .. } = &workflow_snapshot.vessels[0].crew[0].source else {
        panic!("agent source should survive in the workflow snapshot");
    };
    assert_eq!(selector.capability, "code");
    assert_eq!(prompt.as_deref(), Some("Convoy convoy-a - implement Retry logic on branch fix-retry-logic."));
}

#[test]
fn bootstrap_seeds_pending_work_for_agents_but_not_support_processes() {
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let Some(ConvoyStatusPatch::Bootstrap { crew_work, .. }) = outcome.patch else {
        panic!("agent workflow should bootstrap");
    };
    assert_eq!(crew_work["implement"]["coder"].phase, CrewWorkPhase::Pending);
    assert!(!crew_work["implement"].contains_key("build"));
    assert_eq!(crew_work["review"]["reviewer"].phase, CrewWorkPhase::Pending);
    assert!(!crew_work["review"].contains_key("tests"));
}

#[test]
fn all_agent_crew_done_rolls_vessel_work_complete() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
    let coder = status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder work");
    coder.phase = CrewWorkPhase::Done;
    coder.started_at = Some(timestamp(10));
    coder.finished_at = Some(timestamp(20));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert_eq!(outcome.patch, Some(controller_patches::roll_up_work("implement".to_string(), WorkPhase::Complete, timestamp(21), None,)));
}

#[test]
fn interactive_convoy_stays_active_until_crew_reports_complete() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.workflow_snapshot = Some(WorkflowSnapshot { exit: None, vessels: interactive_single_workflow_spec().vessels });
    let mut work = status.work.remove("implement").expect("seed work");
    work.phase = WorkPhase::Running;
    status.work = BTreeMap::from([("work".to_string(), work)]);
    let mut crew = status.crew_work.remove("implement").expect("seed crew");
    crew.get_mut("coder").expect("coder work").phase = CrewWorkPhase::Working;
    status.crew_work = BTreeMap::from([("work".to_string(), crew)]);
    status.observed_workflow_ref = Some("interactive-single".to_string());
    status.observed_workflows = Some(BTreeMap::from([("interactive-single".to_string(), "42".to_string())]));
    let mut spec = valid_convoy_spec();
    spec.workflow_ref = "interactive-single".to_string();

    let waiting = convoy_object("interactive", spec.clone(), Some(status.clone()));
    assert_eq!(reconcile(&waiting, None, timestamp(21)).patch, None);

    status.crew_work.get_mut("work").expect("work crew").get_mut("coder").expect("coder work").phase = CrewWorkPhase::Done;
    let completed = convoy_object("interactive", spec, Some(status));
    assert_eq!(
        reconcile(&completed, None, timestamp(22)).patch,
        Some(controller_patches::roll_up_work("work".to_string(), WorkPhase::Complete, timestamp(22), None))
    );
}

#[test]
fn failed_agent_crew_rolls_vessel_work_failed() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
    let coder = status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder work");
    coder.phase = CrewWorkPhase::Failed;
    coder.finished_at = Some(timestamp(20));
    coder.message = Some("blocked by credentials".to_string());
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::roll_up_work(
            "implement".to_string(),
            WorkPhase::Failed,
            timestamp(21),
            Some("blocked by credentials".to_string()),
        ))
    );
}

#[test]
fn support_only_work_does_not_complete_vacuously() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert_eq!(outcome.patch, None);
}

#[test]
fn human_completion_override_is_not_reopened_by_crew_rollup() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    let implement = status.work.get_mut("implement").expect("implement work");
    implement.phase = WorkPhase::Complete;
    implement.completion_authority = WorkCompletionAuthority::HumanOverride;
    implement.finished_at = Some(timestamp(20));
    status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert!(!matches!(outcome.patch, Some(ConvoyStatusPatch::RollUpWork { phase: WorkPhase::Running, .. })));
}

#[test]
fn handed_back_crew_reopens_completed_vessel_work() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.finished_at = Some(timestamp(20));
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Complete;
    status.work.get_mut("implement").expect("implement work").finished_at = Some(timestamp(20));
    status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder work").phase = CrewWorkPhase::HandedBack;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert_eq!(outcome.patch, Some(controller_patches::roll_up_work("implement".to_string(), WorkPhase::Running, timestamp(21), None,)));
}

#[test]
fn landing_convoy_stays_landing_when_vessel_work_reopens() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Landing;
    status.finished_at = Some(timestamp(20));
    status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(21));

    assert_eq!(outcome.patch, None);
    assert!(outcome.events.is_empty());
}

#[tokio::test]
async fn ready_task_emits_vessel_creation_actuation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Ready;
    status.work.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), Vec::new(), timestamp(20)).await;

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(started_at), finished_at: None })
            if started_at == timestamp(20)
    ));
    assert_eq!(outcome.actuations.len(), 2);
    match outcome
        .actuations
        .iter()
        .find(|actuation| matches!(actuation, Actuation::CreateVessel { .. }))
        .expect("task workspace actuation should be present")
    {
        Actuation::CreateVessel { meta, spec } => {
            assert_eq!(meta.name, "convoy-a-implement");
            assert_eq!(meta.labels.get("flotilla.work/convoy").map(String::as_str), Some("convoy-a"));
            assert_eq!(meta.labels.get("flotilla.work/vessel").map(String::as_str), Some("implement"));
            assert!(!meta.labels.contains_key("flotilla.work/repo-key"));
            assert_eq!(meta.owner_references.len(), 1);
            assert_eq!(meta.owner_references[0].kind, "Convoy");
            assert_eq!(meta.owner_references[0].name, "convoy-a");
            assert_eq!(spec.convoy_ref, "convoy-a");
            assert_eq!(spec.vessel_name, "implement");
            assert_eq!(spec.placement_policy_ref, "laptop-docker");
        }
        other => panic!("expected task workspace actuation, got {other:?}"),
    }
    assert!(outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreatePresentation { .. })));
}

#[tokio::test]
async fn ready_task_with_ready_workspace_moves_to_launching() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Ready;
    status.work.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        Vec::new(),
        timestamp(20),
    )
    .await;
    let expected_checkout_refs = serde_json::to_value(BTreeMap::from([(
        flotilla_resources::RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository identity").key(),
        "checkout-implement".to_string(),
    )]))
    .expect("checkout refs should serialize");

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::WorkLaunching { ref work, started_at, ref placement })
            if work == "implement"
                && started_at == timestamp(20)
                && placement.fields.get("environment_ref") == Some(&serde_json::Value::String("env-implement".to_string()))
                && placement.fields.get("image_ref")
                    == Some(&serde_json::Value::String("registry.example/crew:latest".to_string()))
                && placement.fields.get("image_digest") == Some(&serde_json::Value::String("sha256:first".to_string()))
                && placement.fields.get("checkout_refs") == Some(&expected_checkout_refs)
    ));
}

#[tokio::test]
async fn same_image_tag_moving_between_convoys_produces_distinct_settlement_testimony() {
    async fn testimony(convoy_name: &str, digest: &str) -> flotilla_resources::PlacementStatus {
        let mut status = bootstrapped_tool_only_convoy_status();
        status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Ready;
        status.work.get_mut("implement").expect("implement work").ready_at = Some(timestamp(12));
        let convoy = convoy_object(convoy_name, task_provisioning_convoy_spec(), Some(status));
        let outcome = reconcile_once_with_resources(
            &convoy,
            None,
            vec![vessel_object_with_image_digest(convoy_name, "implement", VesselPhase::Ready, None, digest)],
            Vec::new(),
            timestamp(20),
        )
        .await;
        match outcome.patch {
            Some(ConvoyStatusPatch::WorkLaunching { placement, .. }) => placement,
            other => panic!("expected placement testimony, got {other:?}"),
        }
    }

    let first = testimony("convoy-first", "sha256:first").await;
    let second = testimony("convoy-second", "sha256:second").await;

    assert_eq!(first.fields.get("image_ref"), second.fields.get("image_ref"));
    assert_eq!(first.fields.get("image_digest"), Some(&serde_json::json!("sha256:first")));
    assert_eq!(second.fields.get("image_digest"), Some(&serde_json::json!("sha256:second")));
    assert_ne!(first.fields.get("image_digest"), second.fields.get("image_digest"));
}

#[tokio::test]
async fn launching_task_with_ready_workspace_moves_to_running() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Launching;
    status.work.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        Vec::new(),
        timestamp(20),
    )
    .await;

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::WorkRunning { ref work, .. }) if work == "implement"));
}

#[tokio::test]
async fn running_task_with_failed_workspace_marks_task_failed() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Failed, Some("terminal session crashed"))],
        Vec::new(),
        timestamp(21),
    )
    .await;

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::MarkWorkFailed { ref work, finished_at, ref message })
            if work == "implement" && finished_at == timestamp(21) && message == "terminal session crashed"
    ));
}

#[tokio::test]
async fn running_agent_work_with_an_interrupted_vessel_becomes_recoverable() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Working;
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Interrupted, Some("crew terminal session disappeared; relaunching"))],
        Vec::new(),
        timestamp(21),
    )
    .await;

    let Some(ConvoyStatusPatch::WorkInterrupted { work, roles, message }) = outcome.patch else {
        panic!("expected interrupted work patch");
    };
    assert_eq!(work, "implement");
    assert_eq!(roles, ["coder".to_string()].into_iter().collect());
    assert_eq!(message, "crew terminal session disappeared; relaunching");
}

#[tokio::test]
async fn interrupted_agent_work_returns_to_running_only_after_its_vessel_is_ready() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Interrupted;
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Interrupted;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").message = Some("crew terminal session disappeared; relaunching".to_string());
    status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder").phase = CrewWorkPhase::Interrupted;
    status.crew_work.get_mut("implement").expect("implement crew").get_mut("coder").expect("coder").message =
        Some("crew session for `coder` was interrupted".to_string());
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        Vec::new(),
        timestamp(22),
    )
    .await;

    let patch = outcome.patch.expect("running work patch");
    assert!(matches!(patch, ConvoyStatusPatch::WorkRunning { ref work, .. } if work == "implement"));
    let mut recovered = convoy.status.expect("convoy status");
    patch.apply(&mut recovered);
    assert_eq!(recovered.work["implement"].phase, WorkPhase::Running);
    assert_eq!(recovered.work["implement"].message, None);
    assert_eq!(recovered.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(recovered.crew_work["implement"]["coder"].message, None);
}

#[tokio::test]
async fn active_convoy_creates_presentation_when_missing() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), Vec::new(), timestamp(20)).await;

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(started_at), finished_at: None })
            if started_at == timestamp(20)
    ));
    assert!(outcome.actuations.iter().any(|actuation| {
        matches!(
            actuation,
            Actuation::CreatePresentation { meta, spec }
                if meta.name == "convoy-a-implement"
                    && meta.labels.get(CONVOY_LABEL).map(String::as_str) == Some("convoy-a")
                    && meta.labels.get(VESSEL_LABEL).map(String::as_str) == Some("implement")
                    && meta.owner_references.len() == 1
                    && meta.owner_references[0].kind == "Convoy"
                    && meta.owner_references[0].name == "convoy-a"
                    && spec.convoy_ref == "convoy-a"
                    && spec.presentation_policy_ref == "default"
                    && spec.name == "convoy-a:implement"
                    && spec.process_selector == BTreeMap::from([
                        (CONVOY_LABEL.to_string(), "convoy-a".to_string()),
                        (VESSEL_LABEL.to_string(), "implement".to_string()),
                    ])
        )
    }));
}

#[tokio::test]
async fn active_convoy_does_not_recreate_existing_presentation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome =
        reconcile_once_with_resources(&convoy, None, Vec::new(), vec![presentation_object("convoy-a", "implement")], timestamp(20)).await;

    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreatePresentation { .. })));
}

#[tokio::test]
async fn completed_work_without_a_landing_claim_keeps_resources_warm() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![
            vessel_object("convoy-a", "implement", VesselPhase::Ready, None),
            vessel_object("convoy-a", "review", VesselPhase::Ready, None),
        ],
        vec![presentation_object("convoy-a", "implement"), presentation_object("convoy-a", "review")],
        timestamp(20),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(!outcome.actuations.iter().any(|actuation| matches!(
        actuation,
        Actuation::DeletePresentation { .. } | Actuation::DeleteVessel { .. } | Actuation::DeleteCheckout { .. }
    )));
}

#[tokio::test]
async fn completed_agent_work_without_a_landing_claim_keeps_vessel_available_for_hand_back() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(19));
    }
    mark_crew_done(&mut status, "implement", "coder");
    mark_crew_done(&mut status, "review", "reviewer");
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![
            vessel_object("convoy-a", "implement", VesselPhase::Ready, None),
            vessel_object("convoy-a", "review", VesselPhase::Ready, None),
        ],
        vec![presentation_object("convoy-a", "implement"), presentation_object("convoy-a", "review")],
        timestamp(20),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeletePresentation { .. })));
    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteVessel { .. })));
}

#[tokio::test]
async fn terminal_completed_convoy_still_emits_cleanup_actuations() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Landed;
    status.finished_at = Some(timestamp(20));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        vec![presentation_object("convoy-a", "implement"), presentation_object("convoy-a", "review")],
        timestamp(21),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeletePresentation { name } if name == "convoy-a-implement")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeletePresentation { name } if name == "convoy-a-review")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-a-implement")));
}

#[tokio::test]
async fn abandoned_convoy_reclaims_managed_checkout_but_retains_adopted_owner_record() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let checkouts = backend.clone().using::<Checkout>("flotilla");
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Abandoned;
    status.finished_at = Some(timestamp(20));
    let source = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));
    let created = convoys.create(&convoy_meta("convoy-a"), &source.spec).await.expect("convoy create");
    convoys
        .update_status("convoy-a", &created.metadata.resource_version, source.status.as_ref().expect("convoy status"))
        .await
        .expect("convoy status update");

    let checkout_spec = CheckoutSpec::Worktree(CheckoutWorktreeSpec {
        repo_ref: RepositoryKey("repo-a".to_string()),
        env_ref: "host-direct-a".to_string(),
        r#ref: "feature/abandon".to_string(),
        base_ref: Some("main".to_string()),
        target_path: "/checkouts/convoy-a/repo-a".to_string(),
        clone_ref: "clone-a".to_string(),
    });
    for (name, authority) in [("managed-checkout", LifecycleAuthority::Managed), ("adopted-checkout", LifecycleAuthority::Adopted)] {
        checkouts
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())]))
                    .build()
                    .with_lifecycle_authority(authority),
                &checkout_spec,
            )
            .await
            .expect("checkout create");
    }

    let convoy = convoys.get("convoy-a").await.expect("convoy get");
    let reconciler = ConvoyReconciler::new(templates).with_checkouts(checkouts).with_teardown_runtime(Arc::new(AlwaysEligible));
    let deps = reconciler.fetch_dependencies(&convoy).await.expect("dependencies");
    let outcome = reconciler.reconcile(&convoy, &deps, timestamp(21));

    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteCheckout { name } if name == "managed-checkout")));
    assert!(!outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteCheckout { name } if name == "adopted-checkout")));
}

#[tokio::test]
async fn terminal_completed_convoy_without_observed_presentation_does_not_emit_speculative_delete() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Landed;
    status.finished_at = Some(timestamp(20));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Complete;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        Vec::new(),
        timestamp(21),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeletePresentation { .. })));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-a-implement")));
}

#[tokio::test]
async fn multi_task_convoy_creates_presentations_only_for_active_tasks() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    // `review` intentionally stays in Pending — covers the `WorkPhase::Pending => {}` arm.
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), Vec::new(), timestamp(20)).await;

    let creates: Vec<_> = outcome
        .actuations
        .iter()
        .filter_map(|actuation| match actuation {
            Actuation::CreatePresentation { meta, spec } => Some((meta.name.clone(), spec.name.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(creates, vec![("convoy-a-implement".to_string(), "convoy-a:implement".to_string())]);
    assert!(!outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::CreatePresentation { meta, .. } if meta.name == "convoy-a-review")));
}

#[tokio::test]
async fn single_vessel_convoys_name_presentations_after_the_convoy() {
    let mut presentation_names = Vec::new();

    for convoy_name in ["convoy-a", "convoy-b"] {
        let mut status = bootstrapped_tool_only_convoy_status();
        status.phase = ConvoyPhase::Active;
        status.started_at = Some(timestamp(18));
        status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels.retain(|vessel| vessel.name == "implement");
        status.work.retain(|vessel, _| vessel == "implement");
        status.crew_work.retain(|vessel, _| vessel == "implement");
        status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Running;
        status.work.get_mut("implement").expect("implement work").started_at = Some(timestamp(18));
        let convoy = convoy_object(convoy_name, task_provisioning_convoy_spec(), Some(status));

        let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), Vec::new(), timestamp(20)).await;
        let presentation_name = outcome
            .actuations
            .iter()
            .find_map(|actuation| match actuation {
                Actuation::CreatePresentation { spec, .. } => Some(spec.name.clone()),
                _ => None,
            })
            .expect("presentation creation");
        presentation_names.push(presentation_name);
    }

    assert_eq!(presentation_names, vec!["convoy-a".to_string(), "convoy-b".to_string()]);
}

#[tokio::test]
async fn ready_and_running_tasks_both_create_presentations_when_missing() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Running;
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    status.work.get_mut("review").expect("review task").phase = WorkPhase::Ready;
    status.work.get_mut("review").expect("review task").ready_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), Vec::new(), timestamp(20)).await;

    let mut create_names: Vec<_> = outcome
        .actuations
        .iter()
        .filter_map(|actuation| match actuation {
            Actuation::CreatePresentation { meta, .. } => Some(meta.name.clone()),
            _ => None,
        })
        .collect();
    create_names.sort();
    assert_eq!(create_names, vec!["convoy-a-implement".to_string(), "convoy-a-review".to_string()]);
}

#[tokio::test]
async fn launching_task_creates_presentation_when_missing() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Launching;
    status.work.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    status.work.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        Vec::new(),
        timestamp(20),
    )
    .await;

    assert!(outcome.actuations.iter().any(|actuation| matches!(
        actuation,
        Actuation::CreatePresentation { meta, spec }
            if meta.name == "convoy-a-implement"
                && spec.name == "convoy-a:implement"
                && spec.process_selector.get(VESSEL_LABEL).map(String::as_str) == Some("implement")
    )));
}

#[tokio::test]
async fn one_task_completed_deletes_only_that_presentation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Complete;
    status.work.get_mut("implement").expect("implement task").finished_at = Some(timestamp(19));
    status.work.get_mut("review").expect("review task").phase = WorkPhase::Running;
    status.work.get_mut("review").expect("review task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![vessel_object("convoy-a", "implement", VesselPhase::Ready, None)],
        vec![presentation_object("convoy-a", "implement"), presentation_object("convoy-a", "review")],
        timestamp(20),
    )
    .await;

    let deletes: Vec<_> = outcome
        .actuations
        .iter()
        .filter_map(|actuation| match actuation {
            Actuation::DeletePresentation { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(deletes.is_empty(), "per-vessel warmth remains until the convoy reaches a terminal phase");
}
