use std::{collections::BTreeMap, marker::PhantomData};

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{
    controller_patches, provisioning_patches, Convoy, ConvoyPhase, ConvoyStatusPatch, VesselRequirement, WorkPhase, WorkState,
    WorkflowSnapshot,
};
use crate::{
    canonicalize_repo_url,
    controller::{
        delete_lifecycle_owned_matching, Actuation, LabelMappedWatch, ReconcileOutcome as ControllerReconcileOutcome, Reconciler,
        SecondaryWatch,
    },
    labels::{CONVOY_LABEL, VESSEL_LABEL},
    presentation::{Presentation, PresentationSpec},
    resource::ResourceObject,
    status_patch::StatusPatch,
    vessel::{Vessel, VesselPhase},
    workflow_template::{validate, visit_template_tokens, CrewSource, CrewSpec, ValidationError, WorkflowTemplate},
    InputMeta, InputValue, OwnerReference, PlacementStatus, Resource, ResourceError, TypedResolver,
};

const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";

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
    WorkPhaseChanged { work: String, from: WorkPhase, to: WorkPhase },
    TemplateNotFound { name: String },
    TemplateInvalid { name: String, errors: Vec<ValidationError> },
    WorkflowRefChanged { from: String, to: String },
    MissingInput { name: String },
}

#[derive(Debug, Clone)]
pub struct ConvoyReconciler {
    templates: TypedResolver<WorkflowTemplate>,
    vessels: Option<TypedResolver<Vessel>>,
    presentations: Option<TypedResolver<Presentation>>,
}

#[derive(Debug, Clone)]
pub struct ConvoyDependencies {
    template: Option<ResourceObject<WorkflowTemplate>>,
    vessels: BTreeMap<String, ResourceObject<Vessel>>,
    presentations: BTreeMap<String, ResourceObject<Presentation>>,
}

impl ConvoyReconciler {
    pub fn new(templates: TypedResolver<WorkflowTemplate>) -> Self {
        Self { templates, vessels: None, presentations: None }
    }

    pub fn with_vessels(mut self, vessels: TypedResolver<Vessel>) -> Self {
        self.vessels = Some(vessels);
        self
    }

    pub fn with_presentations(mut self, presentations: TypedResolver<Presentation>) -> Self {
        self.presentations = Some(presentations);
        self
    }

    pub fn secondary_watches() -> Vec<Box<dyn SecondaryWatch<Primary = Convoy>>> {
        vec![
            Box::new(LabelMappedWatch::<Vessel, Convoy> { label_key: CONVOY_LABEL, _marker: PhantomData }),
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
        let vessels = match &self.vessels {
            Some(vessels) if obj.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_some() => vessels
                .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), obj.metadata.name.clone())]))
                .await?
                .items
                .into_iter()
                .map(|workspace| (workspace.metadata.name.clone(), workspace))
                .collect(),
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
        Ok(ConvoyDependencies { template, vessels, presentations })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        now: DateTime<Utc>,
    ) -> ControllerReconcileOutcome<Self::Resource> {
        let outcome = reconcile_internal(obj, deps.template.as_ref(), &deps.vessels, &deps.presentations, now);
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
            delete_lifecycle_owned_matching(presentations, &selector).await?;
        }
        if let Some(vessels) = &self.vessels {
            delete_lifecycle_owned_matching(vessels, &selector).await?;
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
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let status = convoy.status.clone().unwrap_or_default();

    if matches!(status.phase, ConvoyPhase::Completed | ConvoyPhase::Failed | ConvoyPhase::Cancelled) {
        return with_cleanup(convoy, &status, vessels, presentations, InternalReconcileOutcome {
            patch: None,
            actuations: Vec::new(),
            events: Vec::new(),
        });
    }

    if let Some(observed) = status.observed_workflow_ref.as_ref() {
        if observed != &convoy.spec.workflow_ref {
            return with_cleanup(convoy, &status, vessels, presentations, InternalReconcileOutcome {
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
        return with_cleanup(convoy, &status, vessels, presentations, outcome);
    }

    let provisioning = vessel_outcome(convoy, &status, vessels, now);
    if provisioning.patch.is_some() {
        return with_cleanup(convoy, &status, vessels, presentations, provisioning);
    }

    if let Some(outcome) = advance_ready_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    if let Some(outcome) = roll_up_phase_outcome(&status, now) {
        return with_cleanup(convoy, &status, vessels, presentations, InternalReconcileOutcome {
            patch: outcome.patch,
            actuations: provisioning.actuations,
            events: outcome.events,
        });
    }

    with_cleanup(convoy, &status, vessels, presentations, provisioning)
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
        vessels: template
            .spec
            .vessels
            .iter()
            .map(|vessel| VesselRequirement {
                name: vessel.name.clone(),
                depends_on: vessel.depends_on.clone(),
                crew: vessel.crew.iter().map(|member| instantiate_process(convoy, member)).collect(),
            })
            .collect(),
    };
    let work = template
        .spec
        .vessels
        .iter()
        .map(|vessel| {
            (vessel.name.clone(), WorkState {
                phase: WorkPhase::Pending,
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
            work,
            ConvoyPhase::Pending,
            None,
        )),
        actuations: Vec::new(),
        events: Vec::new(),
    }
}

fn instantiate_process(convoy: &ResourceObject<Convoy>, process: &CrewSpec) -> CrewSpec {
    let mut process = process.clone();
    match &mut process.source {
        CrewSource::Agent { prompt, .. } => {
            if let Some(prompt) = prompt {
                *prompt = interpolate_template_text(convoy, prompt);
            }
        }
        CrewSource::Tool { command } => {
            *command = interpolate_template_text(convoy, command);
        }
    }
    process
}

fn interpolate_template_text(convoy: &ResourceObject<Convoy>, text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut search_from = 0;
    visit_template_tokens(text, |token| {
        output.push_str(&text[search_from..token.open]);
        match token.end {
            Some(end) => {
                if let Some(value) = interpolation_value(convoy, token.text) {
                    output.push_str(&value);
                } else {
                    output.push_str(&text[token.open..end]);
                }
                search_from = end;
            }
            None => {
                output.push_str(&text[token.open..]);
                search_from = text.len();
            }
        }
    });
    output.push_str(&text[search_from..]);
    output
}

fn interpolation_value(convoy: &ResourceObject<Convoy>, token: &str) -> Option<String> {
    let segments = token.split('.').collect::<Vec<_>>();
    match segments.as_slice() {
        ["inputs", input_name] => convoy.spec.inputs.get(*input_name).map(input_value_string),
        ["workflow", "name"] => Some(convoy.metadata.name.clone()),
        ["workflow", "namespace"] => Some(convoy.metadata.namespace.clone()),
        _ => None,
    }
}

fn input_value_string(value: &InputValue) -> String {
    match value {
        InputValue::String(value) => value.clone(),
    }
}

fn fail_fast_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let any_failed = status.work.values().any(|task| task.phase == WorkPhase::Failed);
    if !any_failed {
        return None;
    }

    let cancelled_work = status
        .work
        .iter()
        .filter_map(|(name, state)| match state.phase {
            WorkPhase::Completed | WorkPhase::Failed | WorkPhase::Cancelled => None,
            _ => Some((name.clone(), now)),
        })
        .collect::<BTreeMap<_, _>>();

    let mut events = Vec::new();
    if status.phase != ConvoyPhase::Failed {
        events.push(ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Failed });
    }
    for task in cancelled_work.keys() {
        if let Some(state) = status.work.get(task) {
            events.push(ConvoyEvent::WorkPhaseChanged { work: task.clone(), from: state.phase, to: WorkPhase::Cancelled });
        }
    }

    Some(InternalReconcileOutcome {
        patch: Some(controller_patches::fail_convoy(cancelled_work, now, Some("task failure detected".to_string()))),
        actuations: Vec::new(),
        events,
    })
}

fn advance_ready_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    let snapshot = status.workflow_snapshot.as_ref()?;
    let ready = snapshot
        .vessels
        .iter()
        .filter_map(|task| {
            let state = status.work.get(&task.name)?;
            if state.phase != WorkPhase::Pending {
                return None;
            }
            let all_complete = task
                .depends_on
                .iter()
                .all(|dependency| matches!(status.work.get(dependency), Some(dep_state) if dep_state.phase == WorkPhase::Completed));
            all_complete.then(|| (task.name.clone(), now))
        })
        .collect::<BTreeMap<_, _>>();

    if ready.is_empty() {
        return None;
    }

    let events =
        ready.keys().cloned().map(|work| ConvoyEvent::WorkPhaseChanged { work, from: WorkPhase::Pending, to: WorkPhase::Ready }).collect();

    Some(ReconcileOutcome { patch: Some(controller_patches::advance_work_to_ready(ready)), events })
}

fn roll_up_phase_outcome(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ReconcileOutcome> {
    if !status.work.is_empty() && status.work.values().all(|task| task.phase == WorkPhase::Completed) {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(now))),
            events: vec![ConvoyEvent::PhaseChanged { from: status.phase, to: ConvoyPhase::Completed }],
        });
    }

    let any_progressed = status.work.values().any(|task| task.phase != WorkPhase::Pending);
    if any_progressed && status.phase == ConvoyPhase::Pending {
        return Some(ReconcileOutcome {
            patch: Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(now), None)),
            events: vec![ConvoyEvent::PhaseChanged { from: ConvoyPhase::Pending, to: ConvoyPhase::Active }],
        });
    }

    None
}

fn vessel_outcome(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    now: DateTime<Utc>,
) -> InternalReconcileOutcome {
    let Some(snapshot) = status.workflow_snapshot.as_ref() else {
        return InternalReconcileOutcome { patch: None, actuations: Vec::new(), events: Vec::new() };
    };

    let mut actuations = Vec::new();
    for task in &snapshot.vessels {
        let Some(state) = status.work.get(&task.name) else {
            continue;
        };
        let workspace = vessels.get(&per_task_resource_name(&convoy.metadata.name, &task.name));
        match state.phase {
            WorkPhase::Ready => {
                if let Some(workspace) = workspace {
                    if workspace.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                    }
                    if workspace.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_launching(task.name.clone(), now, placement_status(workspace))),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: task.name.clone(),
                                from: WorkPhase::Ready,
                                to: WorkPhase::Launching,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &task.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Launching => {
                if let Some(workspace) = workspace {
                    if workspace.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed) {
                        return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                    }
                    if workspace.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Ready) {
                        return InternalReconcileOutcome {
                            patch: Some(provisioning_patches::work_running(task.name.clone())),
                            actuations,
                            events: vec![ConvoyEvent::WorkPhaseChanged {
                                work: task.name.clone(),
                                from: WorkPhase::Launching,
                                to: WorkPhase::Running,
                            }],
                        };
                    }
                } else if let Some(outcome) = create_vessel_outcome(convoy, &task.name, now) {
                    if outcome.patch.is_some() {
                        return outcome;
                    }
                    actuations.extend(outcome.actuations);
                }
            }
            WorkPhase::Running => {
                if let Some(workspace) =
                    workspace.filter(|workspace| workspace.status.as_ref().map(|status| status.phase) == Some(VesselPhase::Failed))
                {
                    return task_failed_outcome(task.name.clone(), state.phase, workspace_failure_message(workspace), now, actuations);
                }
            }
            WorkPhase::Pending | WorkPhase::Completed | WorkPhase::Failed | WorkPhase::Cancelled => {}
        }
    }

    InternalReconcileOutcome { patch: None, actuations, events: Vec::new() }
}

fn with_cleanup(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    mut outcome: InternalReconcileOutcome,
) -> InternalReconcileOutcome {
    outcome.actuations.extend(cleanup_actuations(convoy, status, vessels, presentations, outcome.patch.as_ref()));
    outcome
}

fn cleanup_actuations(
    convoy: &ResourceObject<Convoy>,
    status: &super::ConvoyStatus,
    vessels: &BTreeMap<String, ResourceObject<Vessel>>,
    presentations: &BTreeMap<String, ResourceObject<Presentation>>,
    patch: Option<&ConvoyStatusPatch>,
) -> Vec<Actuation> {
    let mut predicted_status = status.clone();
    if let Some(patch) = patch {
        patch.apply(&mut predicted_status);
    }

    let mut actuations = Vec::new();

    for (task, state) in &predicted_status.work {
        let resource_name = per_task_resource_name(&convoy.metadata.name, task);
        match state.phase {
            WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running => {
                if !presentations.contains_key(&resource_name) {
                    actuations.push(create_presentation_actuation(convoy, task));
                }
            }
            WorkPhase::Completed | WorkPhase::Failed | WorkPhase::Cancelled => {
                if presentations.contains_key(&resource_name) {
                    actuations.push(Actuation::DeletePresentation { name: resource_name.clone() });
                }
                if vessels.contains_key(&resource_name) {
                    actuations.push(Actuation::DeleteVessel { name: resource_name });
                }
            }
            WorkPhase::Pending => {}
        }
    }

    actuations
}

fn create_vessel_outcome(convoy: &ResourceObject<Convoy>, task: &str, now: DateTime<Utc>) -> Option<InternalReconcileOutcome> {
    let placement_policy_ref = convoy.spec.placement_policy.clone()?;
    let repo_url = convoy.spec.repository.as_ref()?.url.clone();
    let canonical_repo = match canonicalize_repo_url(&repo_url) {
        Ok(canonical_repo) => canonical_repo,
        Err(message) => {
            return Some(InternalReconcileOutcome {
                patch: Some(ConvoyStatusPatch::MarkWorkFailed { work: task.to_string(), finished_at: now, message }),
                actuations: Vec::new(),
                events: vec![ConvoyEvent::WorkPhaseChanged { work: task.to_string(), from: WorkPhase::Ready, to: WorkPhase::Failed }],
            })
        }
    };

    Some(InternalReconcileOutcome {
        patch: None,
        actuations: vec![Actuation::CreateVessel {
            meta: crate::InputMeta::builder()
                .name(per_task_resource_name(&convoy.metadata.name, task))
                .labels(BTreeMap::from([
                    (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                    (VESSEL_LABEL.to_string(), task.to_string()),
                    (REPO_KEY_LABEL.to_string(), crate::repo_key(&canonical_repo)),
                ]))
                .owner_references(vec![OwnerReference {
                    api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                    kind: Convoy::API_PATHS.kind.to_string(),
                    name: convoy.metadata.name.clone(),
                    controller: true,
                }])
                .build(),
            spec: crate::VesselSpec {
                convoy_ref: convoy.metadata.name.clone(),
                vessel_name: task.to_string(),
                placement_policy_ref,
                adopted_checkout_ref: convoy.spec.adopted_checkout_ref.clone(),
            },
        }],
        events: Vec::new(),
    })
}

fn create_presentation_actuation(convoy: &ResourceObject<Convoy>, task: &str) -> Actuation {
    Actuation::CreatePresentation {
        meta: InputMeta::builder()
            .name(per_task_resource_name(&convoy.metadata.name, task))
            .labels(BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                (VESSEL_LABEL.to_string(), task.to_string()),
            ]))
            .owner_references(vec![OwnerReference {
                api_version: format!("{}/{}", Convoy::API_PATHS.group, Convoy::API_PATHS.version),
                kind: Convoy::API_PATHS.kind.to_string(),
                name: convoy.metadata.name.clone(),
                controller: true,
            }])
            .build(),
        spec: PresentationSpec {
            convoy_ref: convoy.metadata.name.clone(),
            // Stage 4a always uses the built-in default policy. Threading a policy ref through
            // ConvoySpec remains follow-up work once convoys can choose among multiple layouts.
            presentation_policy_ref: "default".to_string(),
            name: task.to_string(),
            process_selector: BTreeMap::from([
                (CONVOY_LABEL.to_string(), convoy.metadata.name.clone()),
                (VESSEL_LABEL.to_string(), task.to_string()),
            ]),
        },
    }
}

fn task_failed_outcome(
    task: String,
    from: WorkPhase,
    message: String,
    now: DateTime<Utc>,
    actuations: Vec<Actuation>,
) -> InternalReconcileOutcome {
    InternalReconcileOutcome {
        patch: Some(ConvoyStatusPatch::MarkWorkFailed { work: task.clone(), finished_at: now, message }),
        actuations,
        events: vec![ConvoyEvent::WorkPhaseChanged { work: task.clone(), from, to: WorkPhase::Failed }],
    }
}

fn workspace_failure_message(workspace: &ResourceObject<Vessel>) -> String {
    workspace
        .status
        .as_ref()
        .and_then(|status| status.message.clone())
        .unwrap_or_else(|| format!("task workspace {} failed", workspace.metadata.name))
}

fn placement_status(workspace: &ResourceObject<Vessel>) -> PlacementStatus {
    let mut fields = BTreeMap::from([("vessel_ref".to_string(), json!(workspace.metadata.name))]);
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

/// Per-task convoy resources (`Vessel`, `Presentation`) share the name
/// shape `<convoy>-<task>`. Resource kinds have separate namespaces, so the
/// shared shape causes no collision and keeps both resources discoverable
/// together by name.
fn per_task_resource_name(convoy_name: &str, task: &str) -> String {
    format!("{convoy_name}-{task}")
}
