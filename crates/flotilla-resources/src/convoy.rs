use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, workflow_template::VesselRequirement};

mod reconcile;

pub use reconcile::{reconcile, ConvoyEvent, ConvoyReconciler, ReconcileOutcome};

define_resource!(Convoy, "convoys", ConvoySpec, ConvoyStatus, ConvoyStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySpec {
    pub workflow_ref: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputValue>,
    #[serde(default)]
    pub placement_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<ConvoyRepositorySpec>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
    /// Grouping reference to a [`Project`](crate::Project) resource. Metadata only in v1 —
    /// the reconciler does not consult it. Future: substitute repository/ref from project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_checkout_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyRepositorySpec {
    pub url: String,
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
    /// Work aboard each declared vessel, keyed by vessel (requirement) name.
    /// Today written by explicit completion; becomes a roll-up over crew-level
    /// work state later (flotilla#681).
    #[serde(default)]
    pub work: BTreeMap<String, WorkState>,
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
    pub vessels: Vec<VesselRequirement>,
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
pub struct WorkState {
    pub phase: WorkPhase,
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
pub enum WorkPhase {
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
        work: BTreeMap<String, WorkState>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    },
    FailInit {
        phase: ConvoyPhase,
        message: String,
        finished_at: DateTime<Utc>,
    },
    AdvanceWorkToReady {
        ready: BTreeMap<String, DateTime<Utc>>,
    },
    FailConvoy {
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    RollUpPhase {
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
    },
    WorkLaunching {
        work: String,
        started_at: DateTime<Utc>,
        placement: PlacementStatus,
    },
    WorkRunning {
        work: String,
    },
    MarkWorkCompleted {
        work: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkWorkFailed {
        work: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    MarkWorkCancelled {
        work: String,
        finished_at: DateTime<Utc>,
    },
}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, status: &mut ConvoyStatus) {
        match self {
            Self::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, phase, started_at } => {
                status.workflow_snapshot = Some(workflow_snapshot.clone());
                status.observed_workflow_ref = Some(observed_workflow_ref.clone());
                status.observed_workflows = Some(observed_workflows.clone());
                status.work = work.clone();
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
            }
            Self::FailInit { phase, message, finished_at } => {
                status.phase = *phase;
                status.message = Some(message.clone());
                status.finished_at.get_or_insert(*finished_at);
            }
            Self::AdvanceWorkToReady { ready } => {
                for (work, ready_at) in ready {
                    if let Some(state) = status.work.get_mut(work) {
                        state.phase = WorkPhase::Ready;
                        state.ready_at.get_or_insert(*ready_at);
                    }
                }
            }
            Self::FailConvoy { cancelled_work, finished_at, message } => {
                status.phase = ConvoyPhase::Failed;
                status.finished_at.get_or_insert(*finished_at);
                status.message = message.clone();
                for (work, cancelled_at) in cancelled_work {
                    if let Some(state) = status.work.get_mut(work) {
                        state.phase = WorkPhase::Cancelled;
                        state.finished_at.get_or_insert(*cancelled_at);
                    }
                }
            }
            Self::RollUpPhase { phase, started_at, finished_at } => {
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
                if let Some(finished_at) = finished_at {
                    status.finished_at.get_or_insert(*finished_at);
                }
            }
            Self::WorkLaunching { work, started_at, placement } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Launching;
                    state.started_at.get_or_insert(*started_at);
                    state.placement = Some(placement.clone());
                }
            }
            Self::WorkRunning { work } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Running;
                }
            }
            Self::MarkWorkCompleted { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Completed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                }
            }
            Self::MarkWorkFailed { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
            }
            Self::MarkWorkCancelled { work, finished_at } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Cancelled;
                    state.finished_at.get_or_insert(*finished_at);
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
        work: BTreeMap<String, WorkState>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, phase, started_at }
    }

    pub fn fail_init(phase: ConvoyPhase, message: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailInit { phase, message, finished_at }
    }

    pub fn advance_work_to_ready(ready: BTreeMap<String, DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::AdvanceWorkToReady { ready }
    }

    pub fn fail_convoy(
        cancelled_work: BTreeMap<String, DateTime<Utc>>,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailConvoy { cancelled_work, finished_at, message }
    }

    pub fn roll_up_phase(phase: ConvoyPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpPhase { phase, started_at, finished_at }
    }
}

pub mod provisioning_patches {
    use super::*;

    pub fn work_launching(work: String, started_at: DateTime<Utc>, placement: PlacementStatus) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkLaunching { work, started_at, placement }
    }

    pub fn work_running(work: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkRunning { work }
    }
}

pub mod external_patches {
    use super::*;

    pub fn mark_work_completed(work: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkCompleted { work, finished_at, message }
    }

    pub fn mark_work_failed(work: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkFailed { work, finished_at, message }
    }

    pub fn mark_work_cancelled(work: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkCancelled { work, finished_at }
    }
}
