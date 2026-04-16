use std::{collections::BTreeMap, marker::PhantomData};

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{
    controller_patches, provisioning_patches, Convoy, ConvoyPhase, ConvoyStatusPatch, SnapshotTask, TaskPhase, TaskState, WorkflowSnapshot,
};
use crate::{
    canonicalize_repo_url,
    controller::{Actuation, LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler, SecondaryWatch},
    labels::{CONVOY_LABEL, TASK_LABEL},
    presentation::{Presentation, PresentationSpec},
    resource::ResourceObject,
    status_patch::StatusPatch,
    task_workspace::{TaskWorkspace, TaskWorkspacePhase},
    workflow_template::{validate, ValidationError, WorkflowTemplate},
    InputMeta, OwnerReference, PlacementStatus, Resource, ResourceError, TypedResolver,
};

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
const STAGE_4A_AGENT_MESSAGE: &str = "Stage 4a supports tool processes only; agent processes require selector resolution (Stage 4b).";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>,
    pub events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone)]
struct InternalReconcileOutcome {
    patch: Option<ConvoyStatusPatch>,
    actuations: Vec<Actuation>,
    events: Vec<ConvoyEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyEvent {
    PhaseChanged { from: ConvoyPhase, to: ConvoyPhase },
    TaskPhaseChanged { task: String, from: TaskPhase, to: TaskPhase },
    TemplateNotFound { name: String },
    TemplateInvalid { name: String, errors: Vec<ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput { name: String },
}

#[derive(Debug, Clone)]
pub struct ConvoyReconciler {
    templates: TypedResolver<WorkflowTemplate>,
    task_workspaces: Option<TypedResolver<TaskWorkspace>>,
    presentations: Option<TypedResolver<Presentation>>,
}

#[derive(Debug, Clone)]
pub struct ConvoyDependencies {
    template: Option<ResourceObject<WorkflowTemplate>>,
    task_workspaces: BTreeMap<String, ResourceObject<TaskWorkspace>>,
    presentations: BTreeMap<String, ResourceObject<Presentation>>,
}

impl ConvoyReconciler {
    pub fn new(templates: TypedResolver<WorkflowTemplate>) -> Self {
        Self { templates, task_workspaces: None, presentations: None }
    }

    pub fn with_task_workspaces(mut self, task_workspaces: TypedResolver<TaskWorkspace>) -> Self {
        self.task_workspaces = Some(task_workspaces);
        self
    }

    pub fn with_presentations(mut self, presentations: TypedResolver<Presentation>) -> Self {
        self.presentations = Some(presentations);
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(LabelMappedWatch::<TaskWorkspace, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Presentation, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
        ]
    }
}

impl Reconciler for ConvoyReconciler {
    type Resource = Convoy;
    type Dependencies = ConvoyDependencies;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        let template = if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() {
            None
        } else {
            match self.templates.get(&obj.spec.workflow_ref).await {
                Ok(template) => Some(template),
                Err(ResourceError::NotFound { .. }) => None,
                Err(err) => return Err(err),
            }
        };
        let task_workspaces = match &self.task_workspaces {
            Some(task_workspaces) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => {
                task_workspaces
                    .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                    .await?
                    .items
                    .into_iter()
                    .map(|workspace| (workspace.metadata.name.clone(), workspace))
                    .collect()
            }
            _ => BTreeMap::new(),
        };
        let presentations = match &self.presentations {
            Some(presentations) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => presentations
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|presentation| (presentation.metadata.name.clone(), presentation))
                .collect(),
            _ => BTreeMap::new(),
        };
        Ok(ConvoyDependencies { template, task_workspaces, presentations })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        let outcome = reconcile_internal(obj, deps.template.as_ref(), &deps.task_workspaces, &deps.presentations, now);
        ControllerReconcileOutcome {
            patch: outcome.patch,
            actuations: outcome.actuations,
            events: outcome.events.into_iter().map(|event| format!("{event:?}")).collect(),
            requeue_after: None,
        }
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]);
        if let Some(presentations) = &self.presentations {
            delete_matching(presentations, &selector).await?;
        }
        if let Some(task_workspaces) = &self.task_workspaces {
            delete_matching(task_workspaces, &selector).await?;
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/convoy-teardown")
    }
}

pub fn reconcile(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome {
    let outcome = reconcile_internal(convoy, template, &BTreeMap::new(), &BTreeMap::new(), now);
    ReconcileOutcome { patch: outcome.patch, events: outcome.events }
}

fn reconcile_internal(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    task_workspaces: &BTreeMap<String, ResourceObject<TaskWorkspace>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let status = convoy.status.clone().unwrap_or_default();

    if matches!(status.phase, ConvoyPhase::Completed | ConvoyPhase::Failed | ConvoyPhase::Cancelled) {
        return with_cleanup(convoy, &status, task_workspaces, presentations, InternalReconcileOutcome {
            patch: None,
            actuations: Vec::new(),
            events: Vec::new(),
        });
    }

    if let Some(observed) = status.observed_workflow_ref.as_ref() {
        if observed != &convoy.spec.workflow_ref {
            return with_cleanup(convoy, &status, task_workspaces, presentations, InternalReconcileOutcome {
                patch: Some(controller_patches::fail_init(
                    ConvoyPhase::Failed,
                    "workflow_ref changed after init; not supported".to_string(),
                    now,
                )),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::WorkflowRefChanged { from: observed.clone(), to: convoy.spec.workflow_ref.clone() }],
            });
        }
    }

    if status.observed_workflow_ref.is_none() {
        return bootstrap_outcome(convoy, template, now);
    }

    if let Some(outcome) = fail_fast_outcome(&status, now) {
        return with_cleanup(convoy, &status, task_workspaces, presentations, outcome);
    }

    let provisioning = task_workspace_outcome(convoy, &status, task_workspaces, now);
    if provisioning.patch.is_some() {
        return with_cleanup(convoy, &status, task_workspaces, presentations, provisioning);
    }

    if let Some(outcome) = advance_ready_outcome(&status, now) {
        return with_cleanup(convoy, &status, task_workspaces, presentations, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    if let Some(outcome) = roll_up_phase_outcome(&status, now) {
        return with_cleanup(convoy, &status, task_workspaces, presentations, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    with_cleanup(convoy, &status, task_workspaces, presentations, provisioning)
}

fn bootstrap_outcome(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(template) = template else {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' not found", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateNotFound { name: convoy.spec.workflow_ref.clone() }],
        };
    };

    if let Err(errors) = validate(&template.spec) {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' is invalid: {errors:?}", convoy.spec.workflow_ref),
                now,
            )),
            actuations: Vec::new(),
            events: vec![ConvoyEvent::TemplateInvalid { name: template.metadata.name.clone(), errors }],
        };
    }

    if template
        .spec
        .tasks
        .iter()
        .flat_map(|task| task.processes.iter())
        .any(|process| matches!(process.source, crate::ProcessSource::Agent { .. }))
    {
        return InternalReconcileOutcome {
            patch: Some(controller_patches::fail_init(ConvoyPhase::Failed, STAGE_4A_AGENT_MESSAGE.to_string(), now)),
            actuations: Vec::new(),
            events: Vec::new(),
        };
    }

    for input in &template.spec.inputs {
        if !convoy.spec.inputs.contains_key(&input.name) {
            return InternalReconcileOutcome {
                patch: Some(controller_patches::fail_init(ConvoyPhase::Failed, format!("missing input '{}'", input.name), now)),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::MissingInput { name: input.name.clone() }],
            };
        }
    }

    let workflow_snapshot = WorkflowSnapshot {
        tasks: template
            .spec
            .tasks
            .iter()
            .map(|task| SnapshotTask { name: task.name.clone(), depends_on: task.depends_on.clone(), processes: task.processes.clone() })
            .collect(),
    };
    let tasks = template
        .spec
        .tasks
        .iter()
        .map(|task| {
            (task.name.clone(), TaskState {
                phase: TaskPhase::Pending,
                ready_at: None,
                started_at: None,
                finished_at: None,
                message: None,
                placement: None,
            })
        })
        .collect();

    InternalReconcileOutcome {
        patch: Some(controller_patches::bootstrap(
            workflow_snapshot,
            convoy.spec.workflow_ref.clone(),
            [(convoy.spec.workflow_ref.clone(), template.metadata.resource_version.clone())].into_iter().collect(),
            tasks,
            ConvoyPhase::Pending,
            None,
        )),
        actuations: Vec::new(),
        events: Vec::new(),
    }
}

fn fail_fast_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let any_failed = status.tasks.values().any(|task| task.phase == TaskPhase::Failed);
    if !any_failed {
        return None;
    }

    let cancelled_tasks = status
        .tasks
        .iter()
        .filter_map(|(name, task)| match task.phase {
            TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled => None,
            _ => Some((name.clone(), now)),
        })
        .collect::<BTreeMap<_, _>>();

    let mut events = Vec::new();
    if status.phase != ConvoyPhase::Failed {
        events.push(ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Failed });
    }
    for task in cancelled_tasks.keys() {
        if let Some(state) = status.tasks.get(task) {
            events.push(ConvoyEvent::TaskPhaseChanged { task: task.clone(), from: state.phase, to: TaskPhase::Cancelled });
        }
    }

    Some(InternalReconcileOutcome {
        patch: Some(controller_patches::fail_convoy(cancelled_tasks, now, Some("task failure detected".to_string()))),
        actuations: Vec::new(),
        events,
    })
}

fn advance_ready_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let ready = snapshot
        .tasks
        .iter()
        .filter_map(|task| {
            let state = status.tasks.get(&task.name)?;
            if state.phase != TaskPhase::Pending {
                return None;
            }
            let all_complete = task
                .depends_on
                .iter()
                .all(|dependency| matches!(status.tasks.get(dependency), Some(dep_state) if dep_state.phase == TaskPhase::Completed));
            all_complete.then(|| (task.name.clone(), now))
        })
        .collect::<BTreeMap<_, _>>();

    if ready.is_empty() {
        return None;
    }

    let events =
        ready.keys().cloned().map(|task| ConvoyEvent::TaskPhaseChanged { task, from: TaskPhase::Pending, to: TaskPhase::Ready }).collect();

    Some(ReconcileOutcome { patch: Some(controller_patches::advance_tasks_to_ready(ready)), events })
}

fn roll_up_phase_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    if !status.tasks.is_empty() && status.tasks.values().all(|task| task.phase == TaskPhase::Completed) {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(now))),
            events: vec![ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Completed }],
        });
    }

    let any_progressed = status.tasks.values().any(|task| task.phase != TaskPhase::Pending);
    if any_progressed && status.phase == ConvoyPhase::Pending {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(now), None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Pending, to: ConvoyPhase::Active }],
        });
    }

    None
}

fn task_workspace_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    task_workspaces: &BTreeMap<String, ResourceObject<TaskWorkspace>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(snapshot) = status.workflow_snapshot.as_ref() else {
        return InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() };
    };

    let mut actuations = Vec::new();
    for task in &snapshot.tasks {
        let Some(state) = status.tasks.get(&task.name) else {
            continue;
        };
        let workspace = task_workspaces.get(&task_workspace_name(&convoy.metadata.name, &task.name));
        match state.phase {
            TaskPhase::Ready => {
                if let Some(workspace) = workspace {
                    if workspace.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Failed) {
                        return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                    }
                    if workspace.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::task_launching(task.name.clone(), now, placement_status(workspace))),
                            actuations,
                            events: vec![ConvoyEvent::TaskPhaseChanged {
                                task: task.name.clone(),
                                from: TaskPhase::Ready,
                                to: TaskPhase::Launching,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_task_workspace_outcome(convoy, &task.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            TaskPhase::Launching => {
                if let Some(workspace) = workspace {
                    if workspace.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Failed) {
                        return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                    }
                    if workspace.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::task_running(task.name.clone())),
                            actuations,
                            events: vec![ConvoyEvent::TaskPhaseChanged {
                                task: task.name.clone(),
                                from: TaskPhase::Launching,
                                to: TaskPhase::Running,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_task_workspace_outcome(convoy, &task.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            TaskPhase::Running => {
                if let Some(workspace) =
                    workspace.filter(|workspace| workspace.status.as_ref().map(|status| status.phase) == Some(TaskWorkspacePhase::Failed))
                {
                    return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                }
            }
            TaskPhase::Pending | TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled => {}
        }
    }

    InternalReconcileOutcome { patch: None, actuations, events: Vec::new() }
}

fn with_cleanup(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    task_workspaces: &BTreeMap<String, ResourceObject<TaskWorkspace>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    mut outcome: InternalReconcileOutcome,
) -> InternalReconcileOutcome {
    outcome.actuations.extend(cleanup_actuations(convoy, status, task_workspaces, presentations, outcome.patch.as_ref()));
    outcome
}

fn cleanup_actuations(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    task_workspaces: &BTreeMap<String, ResourceObject<TaskWorkspace>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    patch: Option<&ConvoyStatusPatch>,
) -> Vec<Actuation> {
    let mut predicted_status = status.clone();
    if let Some(patch) = patch {
        patch.apply(&mut predicted_status);
    }

    let mut actuations = Vec::new();

    if predicted_status.phase == ConvoyPhase::Active && presentations.is_empty() {
        actuations.push(create_presentation_actuation(convoy));
    }

    if matches!(predicted_status.phase, ConvoyPhase::Completed | ConvoyPhase::Failed | ConvoyPhase::Cancelled)
        && (!presentations.is_empty() || !matches!(status.phase, ConvoyPhase::Completed | ConvoyPhase::Failed | ConvoyPhase::Cancelled))
    {
        if presentations.is_empty() {
            actuations.push(Actuation::DeletePresentation { name: presentation_name(&convoy.metadata.name) });
        } else {
            actuations.extend(presentations.keys().cloned().map(|name| Actuation::DeletePresentation { name }));
        }
    }

    for (task, state) in &predicted_status.tasks {
        if matches!(state.phase, TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled) {
            let name = task_workspace_name(&convoy.metadata.name, task);
            if task_workspaces.contains_key(&name) {
                actuations.push(Actuation::DeleteTaskWorkspace { name });
            }
        }
    }

    actuations
}

fn create_task_workspace_outcome(convoy: &ResourceObject<Convoy>, task: &str, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let placement_policy_ref = convoy.spec.placement_policy.clone()?;
    let repo_url = convoy.spec.repository.as_ref()?.url.clone();
    let canonical_repo = match canonicalize_repo_url(&repo_url) {
        Ok(canonical_repo) => canonical_repo,
        Err(message) => {
            return Some(InternalReconcileOutcome {
                patch: Some(ConvoyStatusPatch::MarkTaskFailed { task: task.to_string(), finished_at: now, message }),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::TaskPhaseChanged { task: task.to_string(), from: TaskPhase::Ready, to: TaskPhase::Failed }],
            })
        }
    };

    Some(InternalReconcileOutcome {
        patch: None,
        actuations: vec![Actuation::CreateTaskWorkspace {
            meta: crate::InputMeta::builder()
                .name(task_workspace_name(&convoy.metadata.name, task))
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                    (TASK_LABEL.to_string(), task.to_string()),
                    (REPO_KEY_LABEL.to_string(), crate::repo_key(&canonical_repo)),
                ]))
                .owner_references(vec![OwnerReference {
                    api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                    kind: Convoy::API_PATHS.kind.to_string(),
                    name: convoy.metadata.name.clone(),
                    controller: true,
                }])
                .build(),
            spec: crate::TaskWorkspaceSpec { convoy_ref: convoy.metadata.name.clone(), task: task.to_string(), placement_policy_ref },
        }],
        events: Vec::new(),
    })
}

fn create_presentation_actuation(convoy: &ResourceObject<Convoy>) -> Actuation {
    Actuation::CreatePresentation {
        meta: InputMeta::builder()
            .name(presentation_name(&convoy.metadata.name))
            .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.metadata.name.clone())]))
            .owner_references(vec![OwnerReference {
                api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                kind: Convoy::API_PATHS.kind.to_string(),
                name: convoy.metadata.name.clone(),
                controller: true,
            }])
            .build(),
        spec: PresentationSpec {
            convoy_ref: convoy.metadata.name.clone(),
            presentation_policy_ref: "default".to_string(),
            name: convoy.metadata.name.clone(),
            process_selector: BTreeMap::from([(CONVOY_LABEL.to_string(), convoy.metadata.name.clone())]),
        },
    }
}

fn task_failed_outcome(
    task: String,
    from: TaskPhase,
    message: String,
    now: DateTime<Utc>,
    actuations: Vec<Actuation>,
) -> InternalReconcileOutcome {
    InternalReconcileOutcome {
        patch: Some(ConvoyStatusPatch::MarkTaskFailed { task: task.clone(), finished_at: now, message }),
        actuations,
        events: vec![ConvoyEvent::TaskPhaseChanged { task, from, to: TaskPhase::Failed }],
    }
}

fn workspace_failure_message(workspace: &ResourceObject<TaskWorkspace>) -> String {
    workspace
        .status
        .as_ref()
        .and_then(|status| status.message.clone())
        .unwrap_or_else(|| format!("task workspace {} failed", workspace.metadata.name))
}

fn placement_status(workspace: &ResourceObject<TaskWorkspace>) -> PlacementStatus {
    let mut fields = BTreeMap::from([("task_workspace_ref".to_string(), json!(workspace.metadata.name))]);
    if let Some(status) = workspace.status.as_ref() {
        insert_optional_field(&mut fields, "environment_ref", status.environment_ref.clone());
        insert_optional_field(&mut fields, "checkout_ref", status.checkout_ref.clone());
        if !status.terminal_session_refs.is_empty() {
            fields.insert("terminal_session_refs".to_string(), json!(status.terminal_session_refs));
        }
        insert_optional_field(
            &mut fields,
            "placement_policy_ref",
            status.observed_policy_ref.clone().or_else(|| Some(workspace.spec.placement_policy_ref.clone())),
        );
    }
    PlacementStatus { fields }
}

fn insert_optional_field(fields: &mut BTreeMap<String, serde_json::Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        fields.insert(key.to_string(), json!(value));
    }
}

fn task_workspace_name(convoy_name: &str, task: &str) -> String {
    format!("{convoy_name}-{task}")
}

fn presentation_name(convoy_name: &str) -> String {
    format!("{convoy_name}-presentation")
}

async fn delete_matching<T: Resource>(resolver: &TypedResolver<T>, selector: &BTreeMap<String, String>) -> Result<(), ResourceError> {
    let listed = resolver.list_matching_labels(selector).await?;
    for object in listed.items {
        match resolver.delete(&object.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
