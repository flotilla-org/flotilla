use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, workflow_template::VesselRequirement};

mod reconcile;

pub use reconcile::{reconcile, ConvoyEvent, ConvoyReconciler, ReconcileOutcome};

define_resource!(Convoy, "convoys", ConvoySpec, ConvoyStatus, ConvoyStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ConvoySpec {
    pub workflow_ref: String,
    #[builder(default)]
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
    /// Agent-backed work rolls up from `crew_work`; tool-only work is completed
    /// explicitly by an operator.
    #[serde(default)]
    pub work: BTreeMap<String, WorkState>,
    /// Workflow state for each declared agent crew member, keyed first by
    /// vessel name and then by its unique role. Tool processes are excluded.
    #[serde(default)]
    pub crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct WorkState {
    pub phase: WorkPhase,
    /// A human explicitly completed this vessel's work at the roll-up level.
    /// Crew-owned state remains unchanged while this override is active.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "WorkCompletionAuthority::is_crew_rollup")]
    pub completion_authority: WorkCompletionAuthority,
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
    Complete,
    Failed,
    Cancelled,
}

impl WorkPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkCompletionAuthority {
    #[default]
    CrewRollup,
    HumanOverride,
}

impl WorkCompletionAuthority {
    fn is_crew_rollup(&self) -> bool {
        *self == Self::CrewRollup
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CrewWorkState {
    pub phase: CrewWorkPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CrewWorkPhase {
    #[default]
    Pending,
    Working,
    Done,
    HandedBack,
    Failed,
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
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    },
    BackfillCrewWork {
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
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
        started_at: DateTime<Utc>,
    },
    ForceWorkCompleted {
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
    MarkCrewCompleted {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: Option<String>,
    },
    MarkCrewFailed {
        vessel: String,
        role: String,
        finished_at: DateTime<Utc>,
        message: String,
    },
    HandoffCrewWork {
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    },
    RollUpWork {
        work: String,
        phase: WorkPhase,
        transitioned_at: DateTime<Utc>,
        message: Option<String>,
    },
}

impl StatusPatch<ConvoyStatus> for ConvoyStatusPatch {
    fn apply(&self, status: &mut ConvoyStatus) {
        match self {
            Self::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at } => {
                status.workflow_snapshot = Some(workflow_snapshot.clone());
                status.observed_workflow_ref = Some(observed_workflow_ref.clone());
                status.observed_workflows = Some(observed_workflows.clone());
                status.work = work.clone();
                status.crew_work = crew_work.clone();
                status.phase = *phase;
                if let Some(started_at) = started_at {
                    status.started_at.get_or_insert(*started_at);
                }
            }
            Self::BackfillCrewWork { crew_work, completion_overrides } => {
                for (vessel, missing_crew) in crew_work {
                    let crew = status.crew_work.entry(vessel.clone()).or_default();
                    for (role, state) in missing_crew {
                        crew.entry(role.clone()).or_insert_with(|| state.clone());
                    }
                }
                for work in completion_overrides {
                    if let Some(state) = status.work.get_mut(work) {
                        state.completion_authority = WorkCompletionAuthority::HumanOverride;
                    }
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
                // Derived Active roll-up is a continuation of the convoy voyage, not a new attempt.
                status.phase = *phase;
                if *phase == ConvoyPhase::Active {
                    status.finished_at = None;
                }
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
            Self::WorkRunning { work, started_at } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Running;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(crew) = status.crew_work.get_mut(work) {
                    for state in crew.values_mut().filter(|state| state.phase == CrewWorkPhase::Pending) {
                        state.phase = CrewWorkPhase::Working;
                        state.started_at.get_or_insert(*started_at);
                    }
                }
            }
            Self::ForceWorkCompleted { work, finished_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    state.phase = WorkPhase::Complete;
                    state.completion_authority = WorkCompletionAuthority::HumanOverride;
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
            Self::MarkCrewCompleted { vessel, role, finished_at, message } => {
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Done {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Done;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = message.clone();
                }
            }
            Self::MarkCrewFailed { vessel, role, finished_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                if let Some(state) = status.crew_work.get_mut(vessel).and_then(|crew| crew.get_mut(role)) {
                    // Duplicate settlement is sticky; changing the settled outcome records its own time.
                    if state.phase != CrewWorkPhase::Failed {
                        state.finished_at = None;
                    }
                    state.phase = CrewWorkPhase::Failed;
                    state.finished_at.get_or_insert(*finished_at);
                    state.message = Some(message.clone());
                }
            }
            Self::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message } => {
                if let Some(work) = status.work.get_mut(vessel) {
                    work.completion_authority = WorkCompletionAuthority::CrewRollup;
                }
                let Some(crew) = status.crew_work.get_mut(vessel) else {
                    return;
                };
                let target_was_done = crew.get(target_role).is_some_and(|state| state.phase == CrewWorkPhase::Done);
                if let Some(target) = crew.get_mut(target_role) {
                    if matches!(target.phase, CrewWorkPhase::Pending | CrewWorkPhase::Done | CrewWorkPhase::HandedBack) {
                        // Hand-back continues the same crew process, preserving its original start.
                        target.phase = CrewWorkPhase::Working;
                        target.started_at.get_or_insert(*handed_off_at);
                        target.finished_at = None;
                        target.message = Some(message.clone());
                    }
                }
                if target_was_done && sender_role != target_role {
                    if let Some(sender) = crew.get_mut(sender_role) {
                        sender.phase = CrewWorkPhase::HandedBack;
                        sender.finished_at = Some(*handed_off_at);
                        sender.message = Some(message.clone());
                    }
                }
            }
            Self::RollUpWork { work, phase, transitioned_at, message } => {
                if let Some(state) = status.work.get_mut(work) {
                    // Work roll-up reports the same process across continuation and re-settlement.
                    let previous_phase = state.phase;
                    state.phase = *phase;
                    state.completion_authority = WorkCompletionAuthority::CrewRollup;
                    state.message = message.clone();
                    match phase {
                        WorkPhase::Complete | WorkPhase::Failed | WorkPhase::Cancelled => {
                            if previous_phase != *phase {
                                state.finished_at = None;
                            }
                            state.finished_at.get_or_insert(*transitioned_at);
                        }
                        WorkPhase::Pending | WorkPhase::Ready | WorkPhase::Launching | WorkPhase::Running => {
                            state.finished_at = None;
                        }
                    }
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
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        phase: ConvoyPhase,
        started_at: Option<DateTime<Utc>>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::Bootstrap { workflow_snapshot, observed_workflow_ref, observed_workflows, work, crew_work, phase, started_at }
    }

    pub fn fail_init(phase: ConvoyPhase, message: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::FailInit { phase, message, finished_at }
    }

    pub fn backfill_crew_work(
        crew_work: BTreeMap<String, BTreeMap<String, CrewWorkState>>,
        completion_overrides: BTreeSet<String>,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::BackfillCrewWork { crew_work, completion_overrides }
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

    pub fn roll_up_work(work: String, phase: WorkPhase, transitioned_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::RollUpWork { work, phase, transitioned_at, message }
    }
}

pub mod provisioning_patches {
    use super::*;

    pub fn work_launching(work: String, started_at: DateTime<Utc>, placement: PlacementStatus) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkLaunching { work, started_at, placement }
    }

    pub fn work_running(work: String, started_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::WorkRunning { work, started_at }
    }
}

pub mod external_patches {
    use super::*;

    pub fn force_work_completed(work: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::ForceWorkCompleted { work, finished_at, message }
    }

    pub fn mark_work_failed(work: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkFailed { work, finished_at, message }
    }

    pub fn mark_work_cancelled(work: String, finished_at: DateTime<Utc>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkWorkCancelled { work, finished_at }
    }

    pub fn mark_crew_completed(vessel: String, role: String, finished_at: DateTime<Utc>, message: Option<String>) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewCompleted { vessel, role, finished_at, message }
    }

    pub fn mark_crew_failed(vessel: String, role: String, finished_at: DateTime<Utc>, message: String) -> ConvoyStatusPatch {
        ConvoyStatusPatch::MarkCrewFailed { vessel, role, finished_at, message }
    }

    pub fn handoff_crew_work(
        vessel: String,
        sender_role: String,
        target_role: String,
        handed_off_at: DateTime<Utc>,
        message: String,
    ) -> ConvoyStatusPatch {
        ConvoyStatusPatch::HandoffCrewWork { vessel, sender_role, target_role, handed_off_at, message }
    }
}
