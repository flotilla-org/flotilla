mod common;

use std::collections::BTreeMap;

use common::{
    bootstrapped_convoy_status, bootstrapped_tool_only_convoy_status, convoy_meta, convoy_object, pending_task_state,
    task_provisioning_convoy_spec, timestamp, tool_only_workflow_template_object, valid_convoy_spec, valid_workflow_template_object,
    workflow_template_meta,
};
use flotilla_resources::{
    canonicalize_repo_url,
    controller::{Actuation, Reconciler},
    controller_patches, reconcile, repo_key, Convoy, ConvoyEvent, ConvoyPhase, ConvoyReconciler, ConvoyStatusPatch, CrewSource,
    InMemoryBackend, InputMeta, InputValue, OwnerReference, Presentation, PresentationSpec, ResourceBackend, ValidationError, Vessel,
    VesselPhase, VesselSpec, VesselStatus, WorkPhase, WorkflowTemplate, CONVOY_LABEL, VESSEL_LABEL,
};

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
    let reconciler =
        ConvoyReconciler::new(templates.clone()).with_vessels(vessels.clone()).with_presentations(presentations_resolver.clone());
    let deps = reconciler.fetch_dependencies(&current).await.expect("dependency fetch should succeed");
    reconciler.reconcile(&current, &deps, now)
}

fn vessel_meta(name: &str, convoy_name: &str, task: &str) -> InputMeta {
    let canonical_repo = canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("repo url should canonicalize");
    InputMeta {
        name: name.to_string(),
        labels: [
            ("flotilla.work/convoy".to_string(), convoy_name.to_string()),
            ("flotilla.work/vessel".to_string(), task.to_string()),
            ("flotilla.work/repo-key".to_string(), repo_key(&canonical_repo)),
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
    flotilla_resources::ResourceObject {
        metadata: common::object_meta(&format!("{convoy_name}-{task}"), "flotilla", "17"),
        spec: VesselSpec {
            convoy_ref: convoy_name.to_string(),
            vessel_name: task.to_string(),
            placement_policy_ref: "laptop-docker".to_string(),
            adopted_checkout_ref: None,
        },
        status: Some(VesselStatus {
            phase,
            message: message.map(str::to_string),
            observed_policy_ref: Some("laptop-docker".to_string()),
            observed_policy_version: Some("19".to_string()),
            environment_ref: Some(format!("env-{task}")),
            checkout_ref: Some(format!("checkout-{task}")),
            terminal_session_refs: vec![format!("terminal-{task}-coder")],
            started_at: Some(timestamp(16)),
            ready_at: (phase == VesselPhase::Ready).then(|| timestamp(18)),
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

#[test]
fn bootstrap_from_valid_template_returns_bootstrap_patch() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let template = tool_only_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let expected_snapshot = flotilla_resources::WorkflowSnapshot {
        vessels: template
            .spec
            .vessels
            .iter()
            .map(|task| flotilla_resources::VesselRequirement {
                name: task.name.clone(),
                depends_on: task.depends_on.clone(),
                crew: task.crew.clone(),
            })
            .collect(),
    };
    let expected_tasks =
        [("implement".to_string(), pending_task_state()), ("review".to_string(), pending_task_state())].into_iter().collect();
    let expected_patch = controller_patches::bootstrap(
        expected_snapshot,
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        expected_tasks,
        ConvoyPhase::Pending,
        None,
    );

    assert_eq!(outcome.patch, Some(expected_patch));
    assert!(outcome.events.is_empty());
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
        vessels: vec![
            flotilla_resources::VesselRequirement { name: "a".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement { name: "b".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement { name: "c".to_string(), depends_on: Vec::new(), crew: Vec::new() },
        ],
    });
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
        vessels: vec![
            flotilla_resources::VesselRequirement { name: "implement".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement { name: "verify".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string(), "verify".to_string()],
                crew: Vec::new(),
            },
        ],
    });
    status.work.insert("verify".to_string(), pending_task_state());
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Completed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    status.work.get_mut("verify").expect("verify").phase = WorkPhase::Running;
    status.work.get_mut("verify").expect("verify").started_at = Some(timestamp(9));
    status.work.get_mut("review").expect("review").phase = WorkPhase::Pending;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status.clone()));

    let first = reconcile(&convoy, None, timestamp(20));
    assert_eq!(first.patch, Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(timestamp(20)), None)));

    status.work.get_mut("verify").expect("verify").phase = WorkPhase::Completed;
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
    status.work.get_mut("review").expect("review").phase = WorkPhase::Running;
    status.work.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::fail_convoy(
            [("review".to_string(), timestamp(30))].into_iter().collect(),
            timestamp(30),
            Some("work failure detected".to_string())
        ))
    );
}

#[test]
fn all_completed_rolls_up_to_completed() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Completed;
        task.finished_at = Some(timestamp(12));
    }
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(40));

    assert_eq!(outcome.patch, Some(controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(timestamp(40)))));
}

#[test]
fn terminal_completed_convoy_reconciles_to_noop() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(timestamp(40));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Completed;
        task.finished_at = Some(timestamp(12));
    }
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
        vessels: vec![
            flotilla_resources::VesselRequirement { name: "a".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement { name: "b".to_string(), depends_on: Vec::new(), crew: Vec::new() },
            flotilla_resources::VesselRequirement { name: "c".to_string(), depends_on: Vec::new(), crew: Vec::new() },
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
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Completed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
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
    status.work.get_mut("implement").expect("implement").phase = WorkPhase::Completed;
    status.work.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
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
    let CrewSource::Agent { selector, prompt } = &workflow_snapshot.vessels[0].crew[0].source else {
        panic!("agent source should survive in the workflow snapshot");
    };
    assert_eq!(selector.capability, "code");
    assert_eq!(prompt.as_deref(), Some("Convoy convoy-a - implement Retry logic on branch fix-retry-logic."));
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
            let canonical_repo = canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("repo url should canonicalize");
            assert_eq!(meta.name, "convoy-a-implement");
            assert_eq!(meta.labels.get("flotilla.work/convoy").map(String::as_str), Some("convoy-a"));
            assert_eq!(meta.labels.get("flotilla.work/vessel").map(String::as_str), Some("implement"));
            assert_eq!(meta.labels.get("flotilla.work/repo-key").map(String::as_str), Some(repo_key(&canonical_repo).as_str()));
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

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::WorkLaunching { ref work, started_at, ref placement })
            if work == "implement"
                && started_at == timestamp(20)
                && placement.fields.get("environment_ref") == Some(&serde_json::Value::String("env-implement".to_string()))
                && placement.fields.get("checkout_ref") == Some(&serde_json::Value::String("checkout-implement".to_string()))
    ));
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

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::WorkRunning { ref work }) if work == "implement"));
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
                    && spec.name == "implement"
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
async fn completed_convoy_emits_presentation_and_workspace_deletes() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Completed;
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

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, started_at: None, finished_at: Some(finished_at) })
            if finished_at == timestamp(20)
    ));
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
    assert!(outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::DeleteVessel { name } if name == "convoy-a-review")));
}

#[tokio::test]
async fn terminal_completed_convoy_still_emits_cleanup_actuations() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(timestamp(20));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Completed;
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
async fn terminal_completed_convoy_without_observed_presentation_does_not_emit_speculative_delete() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(timestamp(20));
    for task in status.work.values_mut() {
        task.phase = WorkPhase::Completed;
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
    assert_eq!(creates, vec![("convoy-a-implement".to_string(), "implement".to_string())]);
    assert!(!outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::CreatePresentation { meta, .. } if meta.name == "convoy-a-review")));
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
                && spec.name == "implement"
                && spec.process_selector.get(VESSEL_LABEL).map(String::as_str) == Some("implement")
    )));
}

#[tokio::test]
async fn one_task_completed_deletes_only_that_presentation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.work.get_mut("implement").expect("implement task").phase = WorkPhase::Completed;
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
    assert_eq!(deletes, vec!["convoy-a-implement".to_string()]);
}
