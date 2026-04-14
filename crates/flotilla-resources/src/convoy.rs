use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    resource::{ApiPaths, Resource},
    status_patch::StatusPatch,
    workflow_template::ProcessDefinition,
};

mod reconcile;

pub use reconcile::{reconcile, ConvoyEvent, ReconcileOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Convoy;

impl Resource for Convoy {
    type Spec = ConvoySpec;
    type Status = ConvoyStatus;
    type StatusPatch = ConvoyStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "convoys", kind: "Convoy" };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySpec {
    pub workflow_ref: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    pub placement_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    // Keep inputs untagged so today's plain strings serialize naturally while leaving room
    // for future structured input sources without changing the field shape.
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConvoyStatus {
    pub phase: ConvoyPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_snapshot: Option<WorkflowSnapshot>,
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflows: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSnapshot {
    pub tasks: Vec<SnapshotTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTask {
    pub name: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub processes: Vec<ProcessDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConvoyPhase {
    #[default]
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskState {
    pub phase: TaskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlacementStatus {
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyStatusPatch {
    Bootstrap {
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        tasks: BTreeMap<String, TaskState>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    },
    FailInit {
        phase: ConvoyPhase,
        message: String,
        finished_at: DateTime<Utc>,
    },
    AdvanceTasksToReady {
        ready: BTreeMap<String, DateTime<Utc>>,
    },
    FailConvoy {
        cancelled_tasks: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    RollUpPhase {
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
    },
    TaskLaunching {
        task: String,
        started_at: DateTime<Utc>,
        placement: PlacementStatus,
    },
    TaskRunning {
        task: String,
    },
    MarkTaskCompleted {
        task: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkTaskFailed {
        task: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    MarkTaskCancelled {
        task: String,
        finished_at: DateTime<Utc>,
    },
}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, status: &mut ConvoyStatus) {
        match self {
            Self::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, tasks, phase, started_at } => {
                status.workflow_snapshot = Some(workflow_snapshot.clone());
                status.observed_workflow_ref = Some(observed_workflow_ref.clone());
                status.observed_workflows = Some(observed_workflows.clone());
                status.tasks = tasks.clone();
                status.phase = *phase;
                status.started_at = *started_at;
            }
            Self::FailInit { phase, message, finished_at } => {
                status.phase = *phase;
                status.message = Some(message.clone());
                status.finished_at = Some(*finished_at);
            }
            Self::AdvanceTasksToReady { ready } => {
                for (task, ready_at) in ready {
                    if let Some(state) = status.tasks.get_mut(task) {
                        state.phase = TaskPhase::Ready;
                        state.ready_at = Some(*ready_at);
                    }
                }
            }
            Self::FailConvoy { cancelled_tasks, finished_at, message } => {
                status.phase = ConvoyPhase::Failed;
                status.finished_at = Some(*finished_at);
                status.message = message.clone();
                for (task, cancelled_at) in cancelled_tasks {
                    if let Some(state) = status.tasks.get_mut(task) {
                        state.phase = TaskPhase::Cancelled;
                        state.finished_at = Some(*cancelled_at);
                    }
                }
            }
            Self::RollUpPhase { phase, started_at, finished_at } => {
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at = Some(*started_at);
                }
                if let Some(finished_at) = finished_at {
                    status.finished_at = Some(*finished_at);
                }
            }
            Self::TaskLaunching { task, started_at, placement } => {
                if let Some(state) = status.tasks.get_mut(task) {
                    state.phase = TaskPhase::Launching;
                    state.started_at = Some(*started_at);
                    state.placement = Some(placement.clone());
                }
            }
            Self::TaskRunning { task } => {
                if let Some(state) = status.tasks.get_mut(task) {
                    state.phase = TaskPhase::Running;
                }
            }
            Self::MarkTaskCompleted { task, finished_at, message } => {
                if let Some(state) = status.tasks.get_mut(task) {
                    state.phase = TaskPhase::Completed;
                    state.finished_at = Some(*finished_at);
                    state.message = message.clone();
                }
            }
            Self::MarkTaskFailed { task, finished_at, message } => {
                if let Some(state) = status.tasks.get_mut(task) {
                    state.phase = TaskPhase::Failed;
                    state.finished_at = Some(*finished_at);
                    state.message = Some(message.clone());
                }
            }
            Self::MarkTaskCancelled { task, finished_at } => {
                if let Some(state) = status.tasks.get_mut(task) {
                    state.phase = TaskPhase::Cancelled;
                    state.finished_at = Some(*finished_at);
                }
            }
        }
    }
}

pub mod controller_patches {
    use super::*;

    pub fn bootstrap(
        workflow_snapshot: WorkflowSnapshot,
        observed_workflow_ref: String,
        observed_workflows: BTreeMap<String, String>,
        tasks: BTreeMap<String, TaskState>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, tasks, phase, started_at }
    }

    pub fn fail_init(phase: ConvoyPhase, message: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailInit { phase, message, finished_at }
    }

    pub fn advance_tasks_to_ready(ready: BTreeMap<String, DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::AdvanceTasksToReady { ready }
    }

    pub fn fail_convoy(
        cancelled_tasks: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailConvoy { cancelled_tasks, finished_at, message }
    }

    pub fn roll_up_phase(phase: ConvoyPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpPhase { phase, started_at, finished_at }
    }
}

pub mod provisioning_patches {
    use super::*;

    pub fn task_launching(task: String, started_at: DateTime<Utc>, placement: PlacementStatus) -> ConvoyStatusPatch {
        ConvoyStatusPatch::TaskLaunching { task, started_at, placement }
    }

    pub fn task_running(task: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::TaskRunning { task }
    }
}

pub mod external_patches {
    use super::*;

    pub fn mark_task_completed(task: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkTaskCompleted { task, finished_at, message }
    }

    pub fn mark_task_failed(task: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkTaskFailed { task, finished_at, message }
    }

    pub fn mark_task_cancelled(task: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkTaskCancelled { task, finished_at }
    }
}
