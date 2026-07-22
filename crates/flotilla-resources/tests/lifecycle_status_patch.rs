use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_resources::{
    ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, CrewWorkPhase, CrewWorkState, InnerCommandStatus, PlacementStatus, PresentationPhase,
    PresentationStatus, PresentationStatusPatch, Stance, StatusPatch, TerminalSessionPhase, TerminalSessionStatus,
    TerminalSessionStatusPatch, VesselPhase, VesselStatus, VesselStatusPatch, WorkCompletionAuthority, WorkPhase, WorkState,
    WorkflowSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleClass {
    Duplicate,
    Continuation,
    NewAttempt,
    Resettlement,
}

const NONE: &[LifecycleClass] = &[];
const DUPLICATE: &[LifecycleClass] = &[LifecycleClass::Duplicate];
const CONTINUATION: &[LifecycleClass] = &[LifecycleClass::Continuation];
const NEW_ATTEMPT: &[LifecycleClass] = &[LifecycleClass::NewAttempt];
const DUPLICATE_NEW_ATTEMPT: &[LifecycleClass] = &[LifecycleClass::Duplicate, LifecycleClass::NewAttempt];
const DUPLICATE_RESETTLEMENT: &[LifecycleClass] = &[LifecycleClass::Duplicate, LifecycleClass::Resettlement];
const DUPLICATE_CONTINUATION_RESETTLEMENT: &[LifecycleClass] =
    &[LifecycleClass::Duplicate, LifecycleClass::Continuation, LifecycleClass::Resettlement];

macro_rules! define_patch_kinds {
    ($($kind:ident => $classes:expr),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        enum PatchKind {
            $($kind),+
        }

        const ALL_PATCH_KINDS: &[PatchKind] = &[$(PatchKind::$kind),+];

        impl PatchKind {
            fn classes(self) -> &'static [LifecycleClass] {
                match self {
                    $(Self::$kind => $classes),+
                }
            }
        }
    };
}

define_patch_kinds! {
    ConvoyBootstrap => DUPLICATE,
    ConvoyBackfillCrewWork => NONE,
    ConvoyFailInit => DUPLICATE,
    ConvoyAdvanceWorkToReady => DUPLICATE,
    ConvoyFail => DUPLICATE_RESETTLEMENT,
    ConvoyRollUpPhase => DUPLICATE_CONTINUATION_RESETTLEMENT,
    ConvoyWorkLaunching => DUPLICATE,
    ConvoyWorkRunning => DUPLICATE,
    ConvoyForceWorkCompleted => DUPLICATE,
    ConvoyMarkWorkFailed => DUPLICATE,
    ConvoyMarkWorkCancelled => DUPLICATE,
    ConvoyMarkConvoyAbandoned => DUPLICATE,
    ConvoyMarkCrewCompleted => DUPLICATE_RESETTLEMENT,
    ConvoyMarkCrewFailed => DUPLICATE_RESETTLEMENT,
    ConvoyHandoffCrewWork => CONTINUATION,
    ConvoyRollUpWork => DUPLICATE_CONTINUATION_RESETTLEMENT,
    TerminalMarkStarting => NEW_ATTEMPT,
    TerminalMarkRunning => DUPLICATE,
    TerminalMarkMessageDelivered => NONE,
    TerminalMarkStopped => DUPLICATE,
    TerminalMarkFailed => DUPLICATE,
    VesselMarkProvisioning => DUPLICATE,
    VesselMarkReady => DUPLICATE,
    VesselMarkTearingDown => NONE,
    VesselMarkFailed => NONE,
    PresentationMarkActive => DUPLICATE_NEW_ATTEMPT,
    PresentationMarkTornDown => NONE,
    PresentationMarkFailed => NONE,
}

fn convoy_patch_kind(patch: &ConvoyStatusPatch) -> PatchKind {
    match patch {
        ConvoyStatusPatch::Bootstrap { .. } => PatchKind::ConvoyBootstrap,
        ConvoyStatusPatch::BackfillCrewWork { .. } => PatchKind::ConvoyBackfillCrewWork,
        ConvoyStatusPatch::FailInit { .. } => PatchKind::ConvoyFailInit,
        ConvoyStatusPatch::AdvanceWorkToReady { .. } => PatchKind::ConvoyAdvanceWorkToReady,
        ConvoyStatusPatch::FailConvoy { .. } => PatchKind::ConvoyFail,
        ConvoyStatusPatch::RollUpPhase { .. } => PatchKind::ConvoyRollUpPhase,
        ConvoyStatusPatch::WorkLaunching { .. } => PatchKind::ConvoyWorkLaunching,
        ConvoyStatusPatch::WorkRunning { .. } => PatchKind::ConvoyWorkRunning,
        ConvoyStatusPatch::ForceWorkCompleted { .. } => PatchKind::ConvoyForceWorkCompleted,
        ConvoyStatusPatch::MarkWorkFailed { .. } => PatchKind::ConvoyMarkWorkFailed,
        ConvoyStatusPatch::MarkWorkCancelled { .. } => PatchKind::ConvoyMarkWorkCancelled,
        ConvoyStatusPatch::MarkConvoyAbandoned { .. } => PatchKind::ConvoyMarkConvoyAbandoned,
        ConvoyStatusPatch::MarkCrewCompleted { .. } => PatchKind::ConvoyMarkCrewCompleted,
        ConvoyStatusPatch::MarkCrewFailed { .. } => PatchKind::ConvoyMarkCrewFailed,
        ConvoyStatusPatch::HandoffCrewWork { .. } => PatchKind::ConvoyHandoffCrewWork,
        ConvoyStatusPatch::RollUpWork { .. } => PatchKind::ConvoyRollUpWork,
    }
}

fn terminal_session_patch_kind(patch: &TerminalSessionStatusPatch) -> PatchKind {
    match patch {
        TerminalSessionStatusPatch::MarkStarting => PatchKind::TerminalMarkStarting,
        TerminalSessionStatusPatch::MarkRunning { .. } => PatchKind::TerminalMarkRunning,
        TerminalSessionStatusPatch::MarkMessageDelivered { .. } => PatchKind::TerminalMarkMessageDelivered,
        TerminalSessionStatusPatch::MarkStopped { .. } => PatchKind::TerminalMarkStopped,
        TerminalSessionStatusPatch::MarkFailed { .. } => PatchKind::TerminalMarkFailed,
    }
}

fn vessel_patch_kind(patch: &VesselStatusPatch) -> PatchKind {
    match patch {
        VesselStatusPatch::MarkProvisioning { .. } => PatchKind::VesselMarkProvisioning,
        VesselStatusPatch::MarkReady { .. } => PatchKind::VesselMarkReady,
        VesselStatusPatch::MarkTearingDown => PatchKind::VesselMarkTearingDown,
        VesselStatusPatch::MarkFailed { .. } => PatchKind::VesselMarkFailed,
    }
}

fn presentation_patch_kind(patch: &PresentationStatusPatch) -> PatchKind {
    match patch {
        PresentationStatusPatch::MarkActive { .. } => PatchKind::PresentationMarkActive,
        PresentationStatusPatch::MarkTornDown { .. } => PatchKind::PresentationMarkTornDown,
        PresentationStatusPatch::MarkFailed { .. } => PatchKind::PresentationMarkFailed,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LifecycleTimestamps {
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

struct LifecycleCase {
    name: &'static str,
    kind: PatchKind,
    exercise: fn() -> (LifecycleTimestamps, LifecycleTimestamps),
}

fn assert_case_coverage(class: LifecycleClass, cases: &[LifecycleCase]) {
    let expected = ALL_PATCH_KINDS.iter().copied().filter(|kind| kind.classes().contains(&class)).collect::<BTreeSet<_>>();
    let actual = cases.iter().map(|case| case.kind).collect::<BTreeSet<_>>();
    assert_eq!(actual, expected, "every {class:?} patch kind must have a behavioral contract case");
}

fn apply_and_replay<S, P>(status: &mut S, patch: &P)
where
    S: Clone + Debug + PartialEq,
    P: StatusPatch<S>,
{
    patch.apply(status);
    let after_first_application = status.clone();
    patch.apply(status);
    assert_eq!(*status, after_first_application, "replaying the exact patch must be a semantic no-op");
}

fn ts(seconds: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

fn work_state(phase: WorkPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> WorkState {
    WorkState {
        phase,
        completion_authority: WorkCompletionAuthority::CrewRollup,
        ready_at: Some(ts(5)),
        started_at,
        finished_at,
        message: None,
        placement: None,
    }
}

fn crew_state(phase: CrewWorkPhase, started_at: Option<DateTime<Utc>>, finished_at: Option<DateTime<Utc>>) -> CrewWorkState {
    CrewWorkState { phase, started_at, finished_at, message: None }
}

fn active_convoy_status() -> ConvoyStatus {
    ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: None,
        work: BTreeMap::from([("implement".to_string(), work_state(WorkPhase::Running, Some(ts(10)), None))]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), crew_state(CrewWorkPhase::Working, Some(ts(10)), None))]),
        )]),
        message: None,
        started_at: Some(ts(10)),
        finished_at: None,
        observed_workflow_ref: None,
        observed_workflows: None,
    }
}

fn settled_convoy_status() -> ConvoyStatus {
    let mut status = active_convoy_status();
    status.phase = ConvoyPhase::Completed;
    status.finished_at = Some(ts(20));
    status.work.insert("implement".to_string(), work_state(WorkPhase::Complete, Some(ts(10)), Some(ts(20))));
    status.crew_work.insert(
        "implement".to_string(),
        BTreeMap::from([("coder".to_string(), crew_state(CrewWorkPhase::Done, Some(ts(10)), Some(ts(20))))]),
    );
    status
}

fn convoy_timestamps(status: &ConvoyStatus) -> LifecycleTimestamps {
    LifecycleTimestamps { started_at: status.started_at, finished_at: status.finished_at }
}

fn work_timestamps(status: &ConvoyStatus) -> LifecycleTimestamps {
    let work = status.work.get("implement").expect("implement work");
    LifecycleTimestamps { started_at: work.started_at, finished_at: work.finished_at }
}

fn crew_timestamps(status: &ConvoyStatus) -> LifecycleTimestamps {
    let crew = status.crew_work.get("implement").and_then(|crew| crew.get("coder")).expect("coder work");
    LifecycleTimestamps { started_at: crew.started_at, finished_at: crew.finished_at }
}

#[test]
fn patch_variants_exhaustively_declare_their_lifecycle_classes() {
    // The exhaustive matches above make a newly-added patch variant fail to compile until its
    // duplicate, continuation, new-attempt, and re-settlement semantics are declared here.
    assert_eq!(
        convoy_patch_kind(&ConvoyStatusPatch::HandoffCrewWork {
            vessel: "implement".to_string(),
            sender_role: "reviewer".to_string(),
            target_role: "coder".to_string(),
            handed_off_at: ts(30),
            message: "address review".to_string(),
        }),
        PatchKind::ConvoyHandoffCrewWork
    );
    assert_eq!(terminal_session_patch_kind(&TerminalSessionStatusPatch::MarkStarting), PatchKind::TerminalMarkStarting);
    assert_eq!(
        vessel_patch_kind(&VesselStatusPatch::MarkProvisioning {
            observed_policy_ref: "docker".to_string(),
            observed_policy_version: "2".to_string(),
            started_at: ts(30),
        }),
        PatchKind::VesselMarkProvisioning
    );
    assert_eq!(
        presentation_patch_kind(&PresentationStatusPatch::MarkActive {
            presentation_manager: "tmux".to_string(),
            workspace_ref: "workspace-a".to_string(),
            spec_hash: "hash-a".to_string(),
            ready_at: ts(30),
        }),
        PatchKind::PresentationMarkActive
    );
}

#[test]
fn duplicate_lifecycle_transitions_do_not_restamp_timestamps() {
    let cases = [
        LifecycleCase {
            name: "convoy bootstrap",
            kind: PatchKind::ConvoyBootstrap,
            exercise: || {
                let mut status = active_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::Bootstrap {
                    workflow_snapshot: WorkflowSnapshot { vessels: Vec::new() },
                    observed_workflow_ref: "workflow-a".to_string(),
                    observed_workflows: BTreeMap::new(),
                    work: status.work.clone(),
                    crew_work: status.crew_work.clone(),
                    phase: ConvoyPhase::Active,
                    started_at: Some(ts(30)),
                };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy initialization failure",
            kind: PatchKind::ConvoyFailInit,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Failed;
                let before = convoy_timestamps(&status);
                let patch =
                    ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, message: "still failed".to_string(), finished_at: ts(30) };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work ready",
            kind: PatchKind::ConvoyAdvanceWorkToReady,
            exercise: || {
                let mut status = active_convoy_status();
                let work = status.work.get_mut("implement").expect("implement work");
                work.phase = WorkPhase::Ready;
                work.ready_at = Some(ts(10));
                let before = LifecycleTimestamps { started_at: work.ready_at, finished_at: work.finished_at };
                let patch = ConvoyStatusPatch::AdvanceWorkToReady { ready: BTreeMap::from([("implement".to_string(), ts(30))]) };
                apply_and_replay(&mut status, &patch);
                let work = status.work.get("implement").expect("implement work");
                let after = LifecycleTimestamps { started_at: work.ready_at, finished_at: work.finished_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "convoy failure",
            kind: PatchKind::ConvoyFail,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Failed;
                status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Cancelled;
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::FailConvoy {
                    cancelled_work: BTreeMap::from([("implement".to_string(), ts(30))]),
                    finished_at: ts(30),
                    message: Some("still failed".to_string()),
                };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work cancellation during convoy failure",
            kind: PatchKind::ConvoyFail,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Failed;
                status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Cancelled;
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::FailConvoy {
                    cancelled_work: BTreeMap::from([("implement".to_string(), ts(30))]),
                    finished_at: ts(30),
                    message: Some("still failed".to_string()),
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy abandonment",
            kind: PatchKind::ConvoyMarkConvoyAbandoned,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Abandoned;
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkConvoyAbandoned {
                    finished_at: ts(30),
                    authority: WorkCompletionAuthority::HumanOverride,
                    reason: "already abandoned".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy activation",
            kind: PatchKind::ConvoyRollUpPhase,
            exercise: || {
                let mut status = active_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(30)), finished_at: None };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy settlement",
            kind: PatchKind::ConvoyRollUpPhase,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, started_at: None, finished_at: Some(ts(30)) };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work launch",
            kind: PatchKind::ConvoyWorkLaunching,
            exercise: || {
                let mut status = active_convoy_status();
                status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Launching;
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::WorkLaunching {
                    work: "implement".to_string(),
                    started_at: ts(30),
                    placement: PlacementStatus::default(),
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work running starts crew",
            kind: PatchKind::ConvoyWorkRunning,
            exercise: || {
                let mut status = active_convoy_status();
                let crew = status.crew_work.get_mut("implement").and_then(|crew| crew.get_mut("coder")).expect("coder work");
                crew.phase = CrewWorkPhase::Working;
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::WorkRunning { work: "implement".to_string(), started_at: ts(30) };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "forced work completion",
            kind: PatchKind::ConvoyForceWorkCompleted,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::ForceWorkCompleted {
                    work: "implement".to_string(),
                    finished_at: ts(30),
                    message: Some("still complete".to_string()),
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work failure",
            kind: PatchKind::ConvoyMarkWorkFailed,
            exercise: || {
                let mut status = settled_convoy_status();
                status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Failed;
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkWorkFailed {
                    work: "implement".to_string(),
                    finished_at: ts(30),
                    message: "still failed".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work cancellation",
            kind: PatchKind::ConvoyMarkWorkCancelled,
            exercise: || {
                let mut status = settled_convoy_status();
                status.work.get_mut("implement").expect("implement work").phase = WorkPhase::Cancelled;
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkWorkCancelled { work: "implement".to_string(), finished_at: ts(30) };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work settlement roll-up",
            kind: PatchKind::ConvoyRollUpWork,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpWork {
                    work: "implement".to_string(),
                    phase: WorkPhase::Complete,
                    transitioned_at: ts(30),
                    message: None,
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "crew completion",
            kind: PatchKind::ConvoyMarkCrewCompleted,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkCrewCompleted {
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    finished_at: ts(30),
                    message: Some("still complete".to_string()),
                };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "crew failure",
            kind: PatchKind::ConvoyMarkCrewFailed,
            exercise: || {
                let mut status = settled_convoy_status();
                let crew = status.crew_work.get_mut("implement").and_then(|crew| crew.get_mut("coder")).expect("coder work");
                crew.phase = CrewWorkPhase::Failed;
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkCrewFailed {
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    finished_at: ts(30),
                    message: "still failed".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "terminal running",
            kind: PatchKind::TerminalMarkRunning,
            exercise: || {
                let mut status = TerminalSessionStatus {
                    phase: TerminalSessionPhase::Running,
                    session_id: Some("session-a".to_string()),
                    pid: Some(42),
                    started_at: Some(ts(10)),
                    stopped_at: None,
                    inner_command_status: Some(InnerCommandStatus::Running),
                    inner_exit_code: None,
                    message: None,
                    crew: None,
                    launch_command: Some("bash".to_string()),
                    delivered_message_id: None,
                };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                let patch = TerminalSessionStatusPatch::MarkRunning {
                    session_id: "session-a".to_string(),
                    pid: Some(42),
                    started_at: ts(30),
                    crew: None,
                    launch_command: "bash".to_string(),
                    delivered_message_id: None,
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "terminal stopped",
            kind: PatchKind::TerminalMarkStopped,
            exercise: || {
                let mut status = TerminalSessionStatus {
                    phase: TerminalSessionPhase::Stopped,
                    started_at: Some(ts(10)),
                    stopped_at: Some(ts(20)),
                    ..TerminalSessionStatus::default()
                };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                let patch = TerminalSessionStatusPatch::MarkStopped {
                    stopped_at: ts(30),
                    inner_command_status: Some(InnerCommandStatus::Exited),
                    inner_exit_code: Some(0),
                    message: None,
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "terminal failure",
            kind: PatchKind::TerminalMarkFailed,
            exercise: || {
                let mut status = TerminalSessionStatus {
                    phase: TerminalSessionPhase::Failed,
                    started_at: Some(ts(10)),
                    stopped_at: Some(ts(20)),
                    ..TerminalSessionStatus::default()
                };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                let patch = TerminalSessionStatusPatch::MarkFailed { message: "still failed".to_string(), stopped_at: Some(ts(30)) };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "vessel provisioning",
            kind: PatchKind::VesselMarkProvisioning,
            exercise: || {
                let mut status = VesselStatus { phase: VesselPhase::Provisioning, started_at: Some(ts(10)), ..VesselStatus::default() };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.ready_at };
                let patch = VesselStatusPatch::MarkProvisioning {
                    observed_policy_ref: "docker".to_string(),
                    observed_policy_version: "2".to_string(),
                    started_at: ts(30),
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.ready_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "vessel ready",
            kind: PatchKind::VesselMarkReady,
            exercise: || {
                let mut status =
                    VesselStatus { phase: VesselPhase::Ready, started_at: Some(ts(10)), ready_at: Some(ts(20)), ..VesselStatus::default() };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.ready_at };
                let patch = VesselStatusPatch::MarkReady {
                    environment_ref: Some("env-a".to_string()),
                    checkout_refs: Default::default(),
                    terminal_session_refs: Vec::new(),
                    requested_stance: Stance::WorkspaceWrite,
                    effective_stance: Stance::Contained,
                    ready_at: ts(30),
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.ready_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "presentation active",
            kind: PatchKind::PresentationMarkActive,
            exercise: || {
                let mut status =
                    PresentationStatus { phase: PresentationPhase::Active, ready_at: Some(ts(10)), ..PresentationStatus::default() };
                let before = LifecycleTimestamps { started_at: status.ready_at, finished_at: None };
                let patch = PresentationStatusPatch::MarkActive {
                    presentation_manager: "tmux".to_string(),
                    workspace_ref: "workspace-a".to_string(),
                    spec_hash: "hash-a".to_string(),
                    ready_at: ts(30),
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.ready_at, finished_at: None };
                (before, after)
            },
        },
    ];

    assert_case_coverage(LifecycleClass::Duplicate, &cases);
    for case in &cases {
        let (before, after) = (case.exercise)();
        assert_eq!(after, before, "{} duplicate transition must preserve its timestamps", case.name);
    }
}

#[test]
fn continuation_transitions_keep_started_at_and_clear_finished_at() {
    let cases = [
        LifecycleCase {
            name: "convoy reopens",
            kind: PatchKind::ConvoyRollUpPhase,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(30)), finished_at: None };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work reopens",
            kind: PatchKind::ConvoyRollUpWork,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpWork {
                    work: "implement".to_string(),
                    phase: WorkPhase::Running,
                    transitioned_at: ts(30),
                    message: None,
                };
                apply_and_replay(&mut status, &patch);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "crew hand-back",
            kind: PatchKind::ConvoyHandoffCrewWork,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::HandoffCrewWork {
                    vessel: "implement".to_string(),
                    sender_role: "reviewer".to_string(),
                    target_role: "coder".to_string(),
                    handed_off_at: ts(30),
                    message: "address review".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
    ];

    assert_case_coverage(LifecycleClass::Continuation, &cases);
    for case in &cases {
        let (before, after) = (case.exercise)();
        assert_eq!(after.started_at, before.started_at, "{} continuation must keep started_at", case.name);
        assert_eq!(after.finished_at, None, "{} continuation must clear finished_at", case.name);
    }
}

#[test]
fn new_attempt_transitions_replace_attempt_timestamps() {
    let cases = [
        LifecycleCase {
            name: "terminal session restart",
            kind: PatchKind::TerminalMarkStarting,
            exercise: || {
                let mut status = TerminalSessionStatus {
                    phase: TerminalSessionPhase::Stopped,
                    started_at: Some(ts(10)),
                    stopped_at: Some(ts(20)),
                    ..TerminalSessionStatus::default()
                };
                let before = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                apply_and_replay(&mut status, &TerminalSessionStatusPatch::MarkStarting);
                let patch = TerminalSessionStatusPatch::MarkRunning {
                    session_id: "session-b".to_string(),
                    pid: Some(43),
                    started_at: ts(30),
                    crew: None,
                    launch_command: "bash".to_string(),
                    delivered_message_id: None,
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.started_at, finished_at: status.stopped_at };
                (before, after)
            },
        },
        LifecycleCase {
            name: "presentation realization",
            kind: PatchKind::PresentationMarkActive,
            exercise: || {
                let mut status =
                    PresentationStatus { phase: PresentationPhase::TornDown, ready_at: Some(ts(10)), ..PresentationStatus::default() };
                let before = LifecycleTimestamps { started_at: status.ready_at, finished_at: None };
                let patch = PresentationStatusPatch::MarkActive {
                    presentation_manager: "tmux".to_string(),
                    workspace_ref: "workspace-b".to_string(),
                    spec_hash: "hash-b".to_string(),
                    ready_at: ts(30),
                };
                apply_and_replay(&mut status, &patch);
                let after = LifecycleTimestamps { started_at: status.ready_at, finished_at: None };
                (before, after)
            },
        },
    ];

    assert_case_coverage(LifecycleClass::NewAttempt, &cases);
    for case in &cases {
        let (before, after) = (case.exercise)();
        assert_ne!(after.started_at, before.started_at, "{} new attempt must replace its start timestamp", case.name);
        assert_eq!(after.finished_at, None, "{} new attempt must clear its finish timestamp", case.name);
    }
}

#[test]
fn settling_again_after_a_continuation_records_the_new_outcome_time() {
    let cases = [
        LifecycleCase {
            name: "convoy roll-up changes settled outcome",
            kind: PatchKind::ConvoyRollUpPhase,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Failed;
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, started_at: None, finished_at: Some(ts(30)) };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy fail-fast changes settled outcome",
            kind: PatchKind::ConvoyFail,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::FailConvoy {
                    cancelled_work: BTreeMap::new(),
                    finished_at: ts(30),
                    message: Some("work failure detected".to_string()),
                };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "work completes after reopening",
            kind: PatchKind::ConvoyRollUpWork,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = work_timestamps(&status);
                let reopen = ConvoyStatusPatch::RollUpWork {
                    work: "implement".to_string(),
                    phase: WorkPhase::Running,
                    transitioned_at: ts(25),
                    message: None,
                };
                apply_and_replay(&mut status, &reopen);
                let resettle = ConvoyStatusPatch::RollUpWork {
                    work: "implement".to_string(),
                    phase: WorkPhase::Complete,
                    transitioned_at: ts(30),
                    message: None,
                };
                apply_and_replay(&mut status, &resettle);
                (before, work_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "crew completes after hand-back",
            kind: PatchKind::ConvoyMarkCrewCompleted,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = crew_timestamps(&status);
                let hand_back = ConvoyStatusPatch::HandoffCrewWork {
                    vessel: "implement".to_string(),
                    sender_role: "reviewer".to_string(),
                    target_role: "coder".to_string(),
                    handed_off_at: ts(25),
                    message: "address review".to_string(),
                };
                apply_and_replay(&mut status, &hand_back);
                let resettle = ConvoyStatusPatch::MarkCrewCompleted {
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    finished_at: ts(30),
                    message: Some("addressed".to_string()),
                };
                apply_and_replay(&mut status, &resettle);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "crew changes settled outcome",
            kind: PatchKind::ConvoyMarkCrewFailed,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::MarkCrewFailed {
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    finished_at: ts(30),
                    message: "regression found".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
    ];

    assert_case_coverage(LifecycleClass::Resettlement, &cases);
    for case in &cases {
        let (before, after) = (case.exercise)();
        assert_eq!(after.started_at, before.started_at, "{} must keep the original start timestamp", case.name);
        assert_ne!(after.finished_at, before.finished_at, "{} must replace the settled outcome timestamp", case.name);
        assert_eq!(after.finished_at, Some(ts(30)), "{} must record the new settled outcome time", case.name);
    }
}
