use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::{
    controller_patches, Convoy, ConvoyPhase, ConvoyStatusPatch, SnapshotTask, TaskPhase, TaskState, WorkflowSnapshot,
};
use crate::{
    resource::ResourceObject,
    workflow_template::{ValidationError, WorkflowTemplate, validate},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub patch: Option<ConvoyStatusPatch>,
    pub events: Vec<ConvoyEvent>,
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

pub fn reconcile(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome {
    let status = convoy.status.clone().unwrap_or_default();

    if let Some(observed) = status.observed_workflow_ref.as_ref() {
        if observed != &convoy.spec.workflow_ref {
            return ReconcileOutcome {
                patch: Some(controller_patches::fail_init(
                    ConvoyPhase::Failed,
                    "workflow_ref changed after init; not supported".to_string(),
                    now,
                )),
                events: vec![ConvoyEvent::WorkflowRefChanged { from: observed.clone(), to: convoy.spec.workflow_ref.clone() }],
            };
        }
    }

    if status.observed_workflow_ref.is_none() {
        return bootstrap_outcome(convoy, template, now);
    }

    if let Some(patch) = fail_fast_patch(&status.tasks, now) {
        return ReconcileOutcome { patch: Some(patch), events: Vec::new() };
    }

    if let Some(patch) = advance_ready_patch(&status, now) {
        return ReconcileOutcome { patch: Some(patch), events: Vec::new() };
    }

    if let Some(patch) = roll_up_phase_patch(&status, now) {
        return ReconcileOutcome { patch: Some(patch), events: Vec::new() };
    }

    ReconcileOutcome { patch: None, events: Vec::new() }
}

fn bootstrap_outcome(
    convoy: &ResourceObject<Convoy>,
    template: Option<&ResourceObject<WorkflowTemplate>>,
    now: DateTime<Utc>,
) -> ReconcileOutcome {
    let Some(template) = template else {
        return ReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' not found", convoy.spec.workflow_ref),
                now,
            )),
            events: vec![ConvoyEvent::TemplateNotFound { name: convoy.spec.workflow_ref.clone() }],
        };
    };

    if let Err(errors) = validate(&template.spec) {
        return ReconcileOutcome {
            patch: Some(controller_patches::fail_init(
                ConvoyPhase::Failed,
                format!("WorkflowTemplate '{}' is invalid: {errors:?}", convoy.spec.workflow_ref),
                now,
            )),
            events: vec![ConvoyEvent::TemplateInvalid { name: template.metadata.name.clone(), errors }],
        };
    }

    for input in &template.spec.inputs {
        if !convoy.spec.inputs.contains_key(&input.name) {
            return ReconcileOutcome {
                patch: Some(controller_patches::fail_init(
                    ConvoyPhase::Failed,
                    format!("missing input '{}'", input.name),
                    now,
                )),
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
            (
                task.name.clone(),
                TaskState { phase: TaskPhase::Pending, ready_at: None, started_at: None, finished_at: None, message: None, placement: None },
            )
        })
        .collect();

    ReconcileOutcome {
        patch: Some(controller_patches::bootstrap(
            workflow_snapshot,
            convoy.spec.workflow_ref.clone(),
            [(convoy.spec.workflow_ref.clone(), template.metadata.resource_version.clone())].into_iter().collect(),
            tasks,
            ConvoyPhase::Pending,
            None,
        )),
        events: Vec::new(),
    }
}

fn fail_fast_patch(tasks: &BTreeMap<String, TaskState>, now: DateTime<Utc>) -> Option<ConvoyStatusPatch> {
    let any_failed = tasks.values().any(|task| task.phase == TaskPhase::Failed);
    if !any_failed {
        return None;
    }

    let cancelled_tasks = tasks
        .iter()
        .filter_map(|(name, task)| match task.phase {
            TaskPhase::Completed | TaskPhase::Failed | TaskPhase::Cancelled => None,
            _ => Some((name.clone(), now)),
        })
        .collect();

    Some(controller_patches::fail_convoy(cancelled_tasks, now, Some("task failure detected".to_string())))
}

fn advance_ready_patch(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ConvoyStatusPatch> {
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

    (!ready.is_empty()).then(|| controller_patches::advance_tasks_to_ready(ready))
}

fn roll_up_phase_patch(status: &super::ConvoyStatus, now: DateTime<Utc>) -> Option<ConvoyStatusPatch> {
    if !status.tasks.is_empty() && status.tasks.values().all(|task| task.phase == TaskPhase::Completed) {
        return Some(controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(now)));
    }

    let any_progressed = status.tasks.values().any(|task| task.phase != TaskPhase::Pending);
    if any_progressed && status.phase == ConvoyPhase::Pending {
        return Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(now), None));
    }

    None
}
