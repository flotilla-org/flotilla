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
    controller_patches, reconcile, repo_key, Convoy, ConvoyEvent, ConvoyPhase, ConvoyReconciler, ConvoyStatusPatch, InMemoryBackend,
    InputMeta, InputValue, OwnerReference, Presentation, PresentationSpec, ResourceBackend, TaskPhase, TaskWorkspace, TaskWorkspacePhase,
    TaskWorkspaceSpec, TaskWorkspaceStatus, ValidationError, WorkflowTemplate, CONVOY_LABEL,
};

async fn reconcile_once_with_resources(
    convoy: &flotilla_resources::ResourceObject<Convoy>,
    template: Option<&flotilla_resources::ResourceObject<WorkflowTemplate>>,
    workspaces: Vec<flotilla_resources::ResourceObject<TaskWorkspace>>,
    presentations: Vec<flotilla_resources::ResourceObject<Presentation>>,
    now: chrono::DateTime<chrono::Utc>,
) -> flotilla_resources::controller::ReconcileOutcome<Convoy> {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let task_workspaces = backend.clone().using::<TaskWorkspace>("flotilla");
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
        let created = task_workspaces
            .create(&task_workspace_meta(&workspace.metadata.name, &workspace.spec.convoy_ref, &workspace.spec.task), &workspace.spec)
            .await
            .expect("workspace create should succeed");
        if let Some(status) = workspace.status.as_ref() {
            task_workspaces
                .update_status(&workspace.metadata.name, &created.metadata.resource_version, status)
                .await
                .expect("workspace status update should succeed");
        }
    }

    for presentation in presentations {
        let created = presentations_resolver
            .create(&presentation_meta(&presentation.metadata.name, &presentation.spec.convoy_ref), &presentation.spec)
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
        .with_task_workspaces(task_workspaces.clone())
        .with_presentations(presentations_resolver.clone());
    let deps = reconciler.fetch_dependencies(&current).await.expect("dependency fetch should succeed");
    reconciler.reconcile(&current, &deps, now)
}

fn task_workspace_meta(name: &str, convoy_name: &str, task: &str) -> InputMeta {
    let canonical_repo = canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("repo url should canonicalize");
    InputMeta {
        name: name.to_string(),
        labels: [
            ("flotilla.work/convoy".to_string(), convoy_name.to_string()),
            ("flotilla.work/task".to_string(), task.to_string()),
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

fn task_workspace_object(
    convoy_name: &str,
    task: &str,
    phase: TaskWorkspacePhase,
    message: Option<&str>,
) -> flotilla_resources::ResourceObject<TaskWorkspace> {
    flotilla_resources::ResourceObject {
        metadata: common::object_meta(&format!("{convoy_name}-{task}"), "flotilla", "17"),
        spec: TaskWorkspaceSpec {
            convoy_ref: convoy_name.to_string(),
            task: task.to_string(),
            placement_policy_ref: "laptop-docker".to_string(),
        },
        status: Some(TaskWorkspaceStatus {
            phase,
            message: message.map(str::to_string),
            observed_policy_ref: Some("laptop-docker".to_string()),
            observed_policy_version: Some("19".to_string()),
            environment_ref: Some(format!("env-{task}")),
            checkout_ref: Some(format!("checkout-{task}")),
            terminal_session_refs: vec![format!("terminal-{task}-coder")],
            started_at: Some(timestamp(16)),
            ready_at: (phase == TaskWorkspacePhase::Ready).then(|| timestamp(18)),
        }),
    }
}

fn presentation_meta(name: &str, convoy_name: &str) -> InputMeta {
    InputMeta::builder()
        .name(name.to_string())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
        .owner_references(vec![OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Convoy".to_string(),
            name: convoy_name.to_string(),
            controller: true,
        }])
        .build()
}

fn presentation_object(convoy_name: &str) -> flotilla_resources::ResourceObject<Presentation> {
    flotilla_resources::ResourceObject {
        metadata: common::object_meta(&format!("{convoy_name}-presentation"), "flotilla", "23"),
        spec: PresentationSpec {
            convoy_ref: convoy_name.to_string(),
            presentation_policy_ref: "default".to_string(),
            name: convoy_name.to_string(),
            process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]),
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
        tasks: template
            .spec
            .tasks
            .iter()
            .map(|task| flotilla_resources::SnapshotTask {
                name: task.name.clone(),
                depends_on: task.depends_on.clone(),
                processes: task.processes.clone(),
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
    template.spec.tasks[1].depends_on = vec!["missing".to_string()];

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::TemplateInvalid { name, errors }]
            if name == "review-and-fix"
                && matches!(errors.as_slice(), [ValidationError::UnknownDependency { task, missing }] if task == "review" && missing == "missing")
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
        tasks: vec![
            flotilla_resources::SnapshotTask { name: "a".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "b".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "c".to_string(), depends_on: Vec::new(), processes: Vec::new() },
        ],
    });
    status.tasks =
        [("a".to_string(), pending_task_state()), ("b".to_string(), pending_task_state()), ("c".to_string(), pending_task_state())]
            .into_iter()
            .collect();

    let convoy = convoy_object("convoy-a", spec, Some(status));
    let outcome = reconcile(&convoy, None, timestamp(20));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_tasks_to_ready(
            [("a".to_string(), timestamp(20)), ("b".to_string(), timestamp(20)), ("c".to_string(), timestamp(20)),].into_iter().collect()
        ))
    );
}

#[test]
fn fan_in_waits_until_all_dependencies_complete() {
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        tasks: vec![
            flotilla_resources::SnapshotTask { name: "implement".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "verify".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string(), "verify".to_string()],
                processes: Vec::new(),
            },
        ],
    });
    status.tasks.insert("verify".to_string(), pending_task_state());
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Completed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    status.tasks.get_mut("verify").expect("verify").phase = TaskPhase::Running;
    status.tasks.get_mut("verify").expect("verify").started_at = Some(timestamp(9));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Pending;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status.clone()));

    let first = reconcile(&convoy, None, timestamp(20));
    assert_eq!(first.patch, Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(timestamp(20)), None)));

    status.tasks.get_mut("verify").expect("verify").phase = TaskPhase::Completed;
    status.tasks.get_mut("verify").expect("verify").finished_at = Some(timestamp(10));
    status.phase = ConvoyPhase::Active;

    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));
    let second = reconcile(&convoy, None, timestamp(21));

    assert_eq!(
        second.patch,
        Some(controller_patches::advance_tasks_to_ready([("review".to_string(), timestamp(21))].into_iter().collect()))
    );
}

#[test]
fn failed_task_triggers_fail_fast() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Failed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Running;
    status.tasks.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::fail_convoy(
            [("review".to_string(), timestamp(30))].into_iter().collect(),
            timestamp(30),
            Some("task failure detected".to_string())
        ))
    );
}

#[test]
fn all_completed_rolls_up_to_completed() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
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
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
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
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Failed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Cancelled;
    status.tasks.get_mut("review").expect("review").finished_at = Some(timestamp(30));
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
        tasks: vec![
            flotilla_resources::SnapshotTask { name: "a".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "b".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "c".to_string(), depends_on: Vec::new(), processes: Vec::new() },
        ],
    });
    status.tasks =
        [("a".to_string(), pending_task_state()), ("b".to_string(), pending_task_state()), ("c".to_string(), pending_task_state())]
            .into_iter()
            .collect();

    let convoy = convoy_object("convoy-a", spec, Some(status));
    let outcome = reconcile(&convoy, None, timestamp(20));

    assert!(matches!(
        outcome.events.as_slice(),
        [
            ConvoyEvent::TaskPhaseChanged { task: a, from: TaskPhase::Pending, to: TaskPhase::Ready },
            ConvoyEvent::TaskPhaseChanged { task: b, from: TaskPhase::Pending, to: TaskPhase::Ready },
            ConvoyEvent::TaskPhaseChanged { task: c, from: TaskPhase::Pending, to: TaskPhase::Ready },
        ] if a == "a" && b == "b" && c == "c"
    ));
}

#[test]
fn fail_fast_emits_phase_and_task_phase_change_events() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Failed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Running;
    status.tasks.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert!(matches!(
        outcome.events.as_slice(),
        [
            ConvoyEvent::PhaseChanged { from: ConvoyPhase::Active, to: ConvoyPhase::Failed },
            ConvoyEvent::TaskPhaseChanged { task, from: TaskPhase::Running, to: TaskPhase::Cancelled },
        ] if task == "review"
    ));
}

#[test]
fn roll_up_to_active_emits_phase_change_event() {
    let mut status = bootstrapped_convoy_status();
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Completed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Running;
    status.tasks.get_mut("review").expect("review").started_at = Some(timestamp(9));
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
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Completed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(60));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_tasks_to_ready([("review".to_string(), timestamp(60))].into_iter().collect()))
    );
}

#[test]
fn bootstrap_rejects_agent_processes_in_stage_4a() {
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, ref message, .. })
            if message == "Stage 4a supports tool processes only; agent processes require selector resolution (Stage 4b)."
    ));
}

#[tokio::test]
async fn ready_task_emits_task_workspace_creation_actuation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Ready;
    status.tasks.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
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
        .find(|actuation| matches!(actuation, Actuation::CreateTaskWorkspace { .. }))
        .expect("task workspace actuation should be present")
    {
        Actuation::CreateTaskWorkspace { meta, spec } => {
            let canonical_repo = canonicalize_repo_url("git@github.com:flotilla-org/flotilla.git").expect("repo url should canonicalize");
            assert_eq!(meta.name, "convoy-a-implement");
            assert_eq!(meta.labels.get("flotilla.work/convoy").map(String::as_str), Some("convoy-a"));
            assert_eq!(meta.labels.get("flotilla.work/task").map(String::as_str), Some("implement"));
            assert_eq!(meta.labels.get("flotilla.work/repo-key").map(String::as_str), Some(repo_key(&canonical_repo).as_str()));
            assert_eq!(meta.owner_references.len(), 1);
            assert_eq!(meta.owner_references[0].kind, "Convoy");
            assert_eq!(meta.owner_references[0].name, "convoy-a");
            assert_eq!(spec.convoy_ref, "convoy-a");
            assert_eq!(spec.task, "implement");
            assert_eq!(spec.placement_policy_ref, "laptop-docker");
        }
        other => panic!("expected task workspace actuation, got {other:?}"),
    }
    assert!(outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreatePresentation { .. })));
}

#[tokio::test]
async fn ready_task_with_ready_workspace_moves_to_launching() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Ready;
    status.tasks.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Ready, None)],
        Vec::new(),
        timestamp(20),
    )
    .await;

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::TaskLaunching { ref task, started_at, ref placement })
            if task == "implement"
                && started_at == timestamp(20)
                && placement.fields.get("environment_ref") == Some(&serde_json::Value::String("env-implement".to_string()))
                && placement.fields.get("checkout_ref") == Some(&serde_json::Value::String("checkout-implement".to_string()))
    ));
}

#[tokio::test]
async fn launching_task_with_ready_workspace_moves_to_running() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Launching;
    status.tasks.get_mut("implement").expect("implement task").ready_at = Some(timestamp(12));
    status.tasks.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Ready, None)],
        Vec::new(),
        timestamp(20),
    )
    .await;

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::TaskRunning { ref task }) if task == "implement"));
}

#[tokio::test]
async fn running_task_with_failed_workspace_marks_task_failed() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Running;
    status.tasks.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Failed, Some("terminal session crashed"))],
        Vec::new(),
        timestamp(21),
    )
    .await;

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::MarkTaskFailed { ref task, finished_at, ref message })
            if task == "implement" && finished_at == timestamp(21) && message == "terminal session crashed"
    ));
}

#[tokio::test]
async fn active_convoy_creates_presentation_when_missing() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Running;
    status.tasks.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
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
                if meta.name == "convoy-a-presentation"
                    && meta.labels.get(CONVOY_LABEL).map(String::as_str) == Some("convoy-a")
                    && meta.owner_references.len() == 1
                    && meta.owner_references[0].kind == "Convoy"
                    && meta.owner_references[0].name == "convoy-a"
                    && spec.convoy_ref == "convoy-a"
                    && spec.presentation_policy_ref == "default"
                    && spec.name == "convoy-a"
                    && spec.process_selector == BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string())])
        )
    }));
}

#[tokio::test]
async fn active_convoy_does_not_recreate_existing_presentation() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.tasks.get_mut("implement").expect("implement task").phase = TaskPhase::Running;
    status.tasks.get_mut("implement").expect("implement task").started_at = Some(timestamp(18));
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(&convoy, None, Vec::new(), vec![presentation_object("convoy-a")], timestamp(20)).await;

    assert!(!outcome.actuations.iter().any(|actuation| matches!(actuation, Actuation::CreatePresentation { .. })));
}

#[tokio::test]
async fn completed_convoy_emits_presentation_and_workspace_deletes() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![
            task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Ready, None),
            task_workspace_object("convoy-a", "review", TaskWorkspacePhase::Ready, None),
        ],
        vec![presentation_object("convoy-a")],
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
        .any(|actuation| matches!(actuation, Actuation::DeletePresentation { name } if name == "convoy-a-presentation")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteTaskWorkspace { name } if name == "convoy-a-implement")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteTaskWorkspace { name } if name == "convoy-a-review")));
}

#[tokio::test]
async fn terminal_completed_convoy_still_emits_cleanup_actuations() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(timestamp(20));
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Ready, None)],
        vec![presentation_object("convoy-a")],
        timestamp(21),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeletePresentation { name } if name == "convoy-a-presentation")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteTaskWorkspace { name } if name == "convoy-a-implement")));
}

#[tokio::test]
async fn terminal_completed_convoy_without_observed_presentation_does_not_emit_speculative_delete() {
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(timestamp(20));
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
        task.finished_at = Some(timestamp(19));
    }
    let convoy = convoy_object("convoy-a", task_provisioning_convoy_spec(), Some(status));

    let outcome = reconcile_once_with_resources(
        &convoy,
        None,
        vec![task_workspace_object("convoy-a", "implement", TaskWorkspacePhase::Ready, None)],
        Vec::new(),
        timestamp(21),
    )
    .await;

    assert_eq!(outcome.patch, None);
    assert!(!outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeletePresentation { name } if name == "convoy-a-presentation")));
    assert!(outcome
        .actuations
        .iter()
        .any(|actuation| matches!(actuation, Actuation::DeleteTaskWorkspace { name } if name == "convoy-a-implement")));
}
