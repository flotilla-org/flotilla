use flotilla_resources::ResumeIntent;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use chrono::{DateTime, TimeZone, Utc};
use flotilla_resources::{
    ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, CrewWorkPhase, CrewWorkState, PlacementStatus, StatusPatch, TurnDeliveryEpisode,
    TurnDeliveryOutcome, TurnDeliveryRung, WorkCompletionAuthority, WorkPhase, WorkState, WorkflowSnapshot,
};

#[test]
fn force_does_not_relabel_a_ledger_backed_claim_as_overridden() {
    let mut status = settled_convoy_status();
    status.crew_work.get_mut("implement").expect("crew vessel").get_mut("coder").expect("crew role").decision_ledger_ref =
        Some("https://example.test/pull/1#decision-ledger".to_string());

    ConvoyStatusPatch::MarkCrewCompleted {
        vessel: "implement".to_string(),
        role: "coder".to_string(),
        finished_at: ts(30),
        message: Some("duplicate completion".to_string()),
        disposition: None,
        decision_ledger_ref: None,
        decision_ledger_digest: None,
        completed_while_crew_active: false,
        forced_by: Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".to_string(), name: "operator".to_string() }),
    }
    .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].completion_override, None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleClass {
    Duplicate,
    Continuation,
    Resettlement,
}

const NONE: &[LifecycleClass] = &[];
const DUPLICATE: &[LifecycleClass] = &[LifecycleClass::Duplicate];
const CONTINUATION: &[LifecycleClass] = &[LifecycleClass::Continuation];
const DUPLICATE_RESETTLEMENT: &[LifecycleClass] = &[LifecycleClass::Duplicate, LifecycleClass::Resettlement];
const DUPLICATE_CONTINUATION_RESETTLEMENT: &[LifecycleClass] =
    &[LifecycleClass::Duplicate, LifecycleClass::Continuation, LifecycleClass::Resettlement];

macro_rules! define_patch_kinds {
    ($($kind:ident => $classes:expr),+ $(,)?) => {
        // Each kind is named for its resource and patch; only Convoy has lifecycle patches left.
        #[allow(clippy::enum_variant_names)]
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
    ConvoyPromise => NONE,
    ConvoyRestoreTurnActivation => DUPLICATE,
    ConvoySetStall => NONE,
    ConvoyEndStall => NONE,
    ConvoySetNudgeObligations => NONE,
    ConvoySetTeardownWait => NONE,
    ConvoySetPlacementDecision => NONE,
    ConvoyBootstrap => DUPLICATE,
    ConvoyBackfillCrewWork => NONE,
    ConvoyFailInit => DUPLICATE,
    ConvoyAdvanceWorkToReady => DUPLICATE,
    ConvoyFail => DUPLICATE,
    ConvoyRollUpPhase => &[LifecycleClass::Duplicate, LifecycleClass::Continuation],
    ConvoySettle => DUPLICATE,
    ConvoySetSettlementAttention => NONE,
    ConvoyRecordLandingEvent => NONE,
    ConvoyObserveTurnHookHealth => NONE,
    ConvoyWorkLaunching => DUPLICATE,
    ConvoyWorkRunning => DUPLICATE,
    ConvoyWorkInterrupted => CONTINUATION,
    ConvoyForceWorkCompleted => DUPLICATE,
    ConvoyMarkWorkFailed => DUPLICATE,
    ConvoyMarkWorkCancelled => DUPLICATE,
    ConvoyMarkConvoyAbandoned => DUPLICATE,
    ConvoyMarkCrewCompleted => DUPLICATE_RESETTLEMENT,
    ConvoyDeriveCrewDone => NONE,
    ConvoyMarkCrewFailed => DUPLICATE_RESETTLEMENT,
    ConvoyMarkCrewStalled => NONE,
    ConvoyHandoffCrewWork => CONTINUATION,
    ConvoyResumeCrewWork => CONTINUATION,
    ConvoyQueueMessageFollowUp => DUPLICATE,
    ConvoyBeginMessageFollowUp => CONTINUATION,
    ConvoyRecordTurnDelivery => CONTINUATION,
    ConvoyHoldTurnDelivery => NONE,
    ConvoyRefuseTurnDelivery => NONE,
    ConvoyFailTurnDelivery => NONE,
    ConvoyObserveQueuedTurnDeliveries => NONE,
    ConvoyRollUpWork => DUPLICATE_CONTINUATION_RESETTLEMENT,
}

fn convoy_patch_kind(patch: &ConvoyStatusPatch) -> PatchKind {
    match patch {
        ConvoyStatusPatch::RecordLandingEvent { transition: Some(transition) } => convoy_patch_kind(transition),
        ConvoyStatusPatch::RecordLandingEvent { transition: None } => PatchKind::ConvoyRecordLandingEvent,
        ConvoyStatusPatch::HoldTurnDelivery { .. } => PatchKind::ConvoyHoldTurnDelivery,
        ConvoyStatusPatch::RestoreTurnActivation { .. } => PatchKind::ConvoyRestoreTurnActivation,
        ConvoyStatusPatch::RecordNaming { .. }
        | ConvoyStatusPatch::DiscoverSubjects { .. }
        | ConvoyStatusPatch::RecordBranchSubjectScan { .. }
        | ConvoyStatusPatch::RecordBranchSubjectScanFailure { .. }
        | ConvoyStatusPatch::UnlinkSubject { .. } => {
            panic!("admission and subject patches are outside this lifecycle contract")
        }
        ConvoyStatusPatch::SetStall { .. } => PatchKind::ConvoySetStall,
        ConvoyStatusPatch::EndStall { .. } => PatchKind::ConvoyEndStall,
        ConvoyStatusPatch::SetNudgeObligations { .. } => PatchKind::ConvoySetNudgeObligations,
        ConvoyStatusPatch::SetTeardownWait { .. } => PatchKind::ConvoySetTeardownWait,
        ConvoyStatusPatch::SetPlacementDecision { .. } => PatchKind::ConvoySetPlacementDecision,
        ConvoyStatusPatch::Bootstrap { .. } => PatchKind::ConvoyBootstrap,
        ConvoyStatusPatch::BackfillCrewWork { .. } => PatchKind::ConvoyBackfillCrewWork,
        ConvoyStatusPatch::FailInit { .. } => PatchKind::ConvoyFailInit,
        ConvoyStatusPatch::AdvanceWorkToReady { .. } => PatchKind::ConvoyAdvanceWorkToReady,
        ConvoyStatusPatch::FailConvoy { .. } => PatchKind::ConvoyFail,
        ConvoyStatusPatch::RollUpPhase { .. } => PatchKind::ConvoyRollUpPhase,
        ConvoyStatusPatch::Settle { .. } => PatchKind::ConvoySettle,
        ConvoyStatusPatch::SetSettlementAttention { .. } => PatchKind::ConvoySetSettlementAttention,
        ConvoyStatusPatch::ObserveTurnHookHealth { .. } => PatchKind::ConvoyObserveTurnHookHealth,
        ConvoyStatusPatch::WorkLaunching { .. } => PatchKind::ConvoyWorkLaunching,
        ConvoyStatusPatch::WorkRunning { .. } => PatchKind::ConvoyWorkRunning,
        ConvoyStatusPatch::WorkInterrupted { .. } => PatchKind::ConvoyWorkInterrupted,
        ConvoyStatusPatch::ForceWorkCompleted { .. } => PatchKind::ConvoyForceWorkCompleted,
        ConvoyStatusPatch::MarkWorkFailed { .. } => PatchKind::ConvoyMarkWorkFailed,
        ConvoyStatusPatch::MarkWorkCancelled { .. } => PatchKind::ConvoyMarkWorkCancelled,
        ConvoyStatusPatch::MarkConvoyAbandoned { .. } => PatchKind::ConvoyMarkConvoyAbandoned,
        ConvoyStatusPatch::Promise { .. } | ConvoyStatusPatch::ObservePromise { .. } | ConvoyStatusPatch::HandPromisesToEngine { .. } => {
            PatchKind::ConvoyPromise
        }
        ConvoyStatusPatch::MarkCrewCompleted { .. } => PatchKind::ConvoyMarkCrewCompleted,
        ConvoyStatusPatch::DeriveCrewDone { .. } => PatchKind::ConvoyDeriveCrewDone,
        ConvoyStatusPatch::MarkCrewFailed { .. } => PatchKind::ConvoyMarkCrewFailed,
        ConvoyStatusPatch::MarkCrewStalled { .. } => PatchKind::ConvoyMarkCrewStalled,
        ConvoyStatusPatch::HandoffCrewWork { .. } => PatchKind::ConvoyHandoffCrewWork,
        ConvoyStatusPatch::ResumeCrewWork { .. } => PatchKind::ConvoyResumeCrewWork,
        ConvoyStatusPatch::QueueMessageFollowUp { .. } => PatchKind::ConvoyQueueMessageFollowUp,
        ConvoyStatusPatch::BeginMessageFollowUp { .. } => PatchKind::ConvoyBeginMessageFollowUp,
        ConvoyStatusPatch::RecordTurnDelivery { .. } => PatchKind::ConvoyRecordTurnDelivery,
        ConvoyStatusPatch::RefuseTurnDelivery { .. } => PatchKind::ConvoyRefuseTurnDelivery,
        ConvoyStatusPatch::FailTurnDelivery { .. } => PatchKind::ConvoyFailTurnDelivery,
        ConvoyStatusPatch::ObserveQueuedTurnDeliveries { .. } => PatchKind::ConvoyObserveQueuedTurnDeliveries,
        ConvoyStatusPatch::RollUpWork { .. } => PatchKind::ConvoyRollUpWork,
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
    CrewWorkState {
        task_assigned: false,
        completion_turn: None,
        pending_follow_up: None,
        completion_authority: Default::default(),
        phase,
        resumed_at: None,
        resume_brief_id: None,
        started_at,
        finished_at,
        message: None,
        stall_reason: None,
        stall_proposed_disposition: None,
        disposition: None,
        decision_ledger_ref: None,
        decision_ledger_digest: None,
        superseded_claims: Vec::new(),
        completion_override: None,
        completed_while_crew_active: false,
        claim_evidence: None,
        completion_refusal: None,
    }
}

fn active_convoy_status() -> ConvoyStatus {
    ConvoyStatus {
        naming: None,
        promises: Default::default(),
        landing_entry: None,
        landing_settlement: None,
        ensure_admission: None,
        unlinked_subjects: Vec::new(),
        subjects: Vec::new(),
        branch_subject_scan_at: None,
        branch_subject_scan_error: None,
        stalled: None,
        stalls: Default::default(),
        nudge_obligations: Vec::new(),
        provisioning: None,
        placement_decision: None,
        phase: ConvoyPhase::Active,
        workflow_snapshot: None,
        work: BTreeMap::from([("implement".to_string(), work_state(WorkPhase::Running, Some(ts(10)), None))]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), crew_state(CrewWorkPhase::Working, Some(ts(10)), None))]),
        )]),
        engine_launch: None,
        message: None,
        started_at: Some(ts(10)),
        finished_at: None,
        disposition: None,
        observed_workflow_ref: None,
        observed_workflows: None,
        target_mismatches: Vec::new(),
        turn_deliveries: BTreeMap::new(),
        attention: None,
    }
}

fn settled_convoy_status() -> ConvoyStatus {
    let mut status = active_convoy_status();
    status.phase = ConvoyPhase::Landed;
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
}

#[test]
fn duplicate_lifecycle_transitions_do_not_restamp_timestamps() {
    let cases = [
        LifecycleCase {
            name: "speculative turn activation rollback",
            kind: PatchKind::ConvoyRestoreTurnActivation,
            exercise: || {
                let mut previous = settled_convoy_status();
                previous.phase = ConvoyPhase::Landing;
                let before = crew_timestamps(&previous);
                let mut status = previous.clone();
                flotilla_resources::external_patches::resume_crew_work(
                    "implement".into(),
                    "coder".into(),
                    ts(30),
                    "continue".into(),
                    Some("intent".into()),
                    ResumeIntent::Reopen,
                )
                .apply(&mut status);
                let patch = flotilla_resources::external_patches::restore_turn_activation(
                    "implement".into(),
                    "coder".into(),
                    status.clone(),
                    previous,
                );
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy bootstrap",
            kind: PatchKind::ConvoyBootstrap,
            exercise: || {
                let mut status = active_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::Bootstrap {
                    workflow_snapshot: WorkflowSnapshot {
                        cascade: None,
                        stall_nudges: Default::default(),
                        supervision: None,
                        exit: None,
                        turn_delivery: Default::default(),
                        vessels: Vec::new(),
                    },
                    observed_workflow_ref: "workflow-a".to_string(),
                    observed_workflows: BTreeMap::new(),
                    work: status.work.clone(),
                    crew_work: status.crew_work.clone(),
                    dispatch_promises: Vec::new(),
                    standing: false,
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
                    expected_phase: ConvoyPhase::Abandoned,
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
                let patch = ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Landed, started_at: None, finished_at: Some(ts(30)) };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "convoy settlement with target facts",
            kind: PatchKind::ConvoySettle,
            exercise: || {
                let mut status = settled_convoy_status();
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::Settle {
                    evidence: None,
                    disposition: "merged".to_string(),
                    target_mismatches: Vec::new(),
                    finished_at: ts(30),
                };
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
                let patch = ConvoyStatusPatch::WorkRunning {
                    work: "implement".to_string(),
                    started_at: ts(30),
                    launched_roles: BTreeSet::from(["coder".to_string()]),
                };
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
                    disposition: None,
                    decision_ledger_ref: None,
                    decision_ledger_digest: None,
                    completed_while_crew_active: false,
                    forced_by: None,
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
            name: "Message continuation queued",
            kind: PatchKind::ConvoyQueueMessageFollowUp,
            exercise: || {
                let mut status = active_convoy_status();
                let before = convoy_timestamps(&status);
                apply_and_replay(
                    &mut status,
                    &ConvoyStatusPatch::QueueMessageFollowUp {
                        vessel: "implement".into(),
                        role: "coder".into(),
                        message: Some(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "follow-up")),
                    },
                );
                (before, convoy_timestamps(&status))
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
            name: "convoy continues from landing",
            kind: PatchKind::ConvoyRollUpPhase,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
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
                status.phase = ConvoyPhase::Landing;
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
            name: "work interrupted",
            kind: PatchKind::ConvoyWorkInterrupted,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
                let before = work_timestamps(&status);
                let patch = ConvoyStatusPatch::WorkInterrupted {
                    work: "implement".to_string(),
                    roles: BTreeSet::from(["coder".to_string()]),
                    message: "crew session disappeared".to_string(),
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
                status.phase = ConvoyPhase::Landing;
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
        LifecycleCase {
            name: "crew resume",
            kind: PatchKind::ConvoyResumeCrewWork,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
                let before = crew_timestamps(&status);
                let patch = ConvoyStatusPatch::ResumeCrewWork {
                    intent: ResumeIntent::Reopen,
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    resumed_at: ts(30),
                    prompt: "rebase onto main".to_string(),
                    brief_id: None,
                };
                apply_and_replay(&mut status, &patch);
                (before, crew_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "Message continuation reopens convoy once",
            kind: PatchKind::ConvoyBeginMessageFollowUp,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
                let reference = flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "follow-up");
                ConvoyStatusPatch::QueueMessageFollowUp {
                    vessel: "implement".into(),
                    role: "coder".into(),
                    message: Some(reference.clone()),
                }
                .apply(&mut status);
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::BeginMessageFollowUp {
                    vessel: "implement".into(),
                    role: "coder".into(),
                    message: reference,
                    content: "new turn".into(),
                    claim: flotilla_resources::SupersededCrewClaim {
                        claimed_at: ts(30),
                        message: Some("first turn complete".into()),
                        disposition: None,
                        decision_ledger_ref: None,
                        decision_ledger_digest: None,
                        completion_override: None,
                        completed_while_crew_active: false,
                    },
                };
                apply_and_replay(&mut status, &patch);
                assert_eq!(status.crew_work["implement"]["coder"].superseded_claims.len(), 1);
                assert!(status.crew_work["implement"]["coder"].pending_follow_up.is_none());
                (before, convoy_timestamps(&status))
            },
        },
        LifecycleCase {
            name: "turn delivery reopens convoy",
            kind: PatchKind::ConvoyRecordTurnDelivery,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
                let before = convoy_timestamps(&status);
                let patch = ConvoyStatusPatch::RecordTurnDelivery {
                    source: "review".to_string(),
                    episode: TurnDeliveryEpisode {
                        subject_revision: "abc123".to_string(),
                        evidence_at: ts(30),
                        judged_claim_at: ts(20),
                        outcome: TurnDeliveryOutcome::Delivered { rung: TurnDeliveryRung::WarmSession, delivered_at: ts(30) },
                    },
                    vessel: "implement".to_string(),
                    role: "coder".to_string(),
                    prompt: "address review".to_string(),
                };
                apply_and_replay(&mut status, &patch);
                (before, convoy_timestamps(&status))
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
fn settling_again_after_a_continuation_records_the_new_outcome_time() {
    let cases = [
        LifecycleCase {
            name: "work completes after reopening",
            kind: PatchKind::ConvoyRollUpWork,
            exercise: || {
                let mut status = settled_convoy_status();
                status.phase = ConvoyPhase::Landing;
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
                status.phase = ConvoyPhase::Landing;
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
                    disposition: None,
                    decision_ledger_ref: None,
                    decision_ledger_digest: None,
                    completed_while_crew_active: false,
                    forced_by: None,
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
                status.phase = ConvoyPhase::Landing;
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

#[test]
fn turn_delivery_episodes_stored_before_2135_decode_their_head_sha_as_subject_revision() {
    let episode = TurnDeliveryEpisode::builder()
        .subject_revision("00cef2657a7c900ba85f851088333035b106faba".to_string())
        .evidence_at(ts(20))
        .judged_claim_at(ts(10))
        .outcome(TurnDeliveryOutcome::Delivered { rung: TurnDeliveryRung::WarmSession, delivered_at: ts(30) })
        .build();
    let mut stored = serde_json::to_value(&episode).expect("serialize episode");
    let revision = stored.as_object_mut().expect("episode object").remove("subject_revision").expect("subject_revision written");
    stored["head_sha"] = revision;

    let decoded: TurnDeliveryEpisode = serde_json::from_value(stored).expect("pre-#2135 episode decodes");
    assert_eq!(decoded, episode);
    assert!(serde_json::to_value(&decoded).expect("serialize").get("head_sha").is_none(), "the old name is never written back");
}

// Review #2681: admitted artifact identity survives duplicate completion and force,
// is archived at a Message follow-up boundary, and cannot leak into a new working turn.
#[hegel::test]
fn ledger_admission_digest_survives_duplicates_and_resets_for_new_turns(tc: hegel::TestCase) {
    use hegel::generators as gs;
    // All continuation writers: resume, hand-off target, Message follow-up, and subject turn.
    let continuation = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let mut status = active_convoy_status();
    let completion = ConvoyStatusPatch::MarkCrewCompleted {
        vessel: "implement".into(),
        role: "coder".into(),
        finished_at: ts(30),
        message: None,
        disposition: None,
        decision_ledger_ref: None,
        decision_ledger_digest: Some("admitted".into()),
        completed_while_crew_active: false,
        forced_by: None,
    };
    completion.apply(&mut status);
    let duplicate = ConvoyStatusPatch::MarkCrewCompleted {
        vessel: "implement".into(),
        role: "coder".into(),
        finished_at: ts(31),
        message: None,
        disposition: None,
        decision_ledger_ref: None,
        decision_ledger_digest: None,
        completed_while_crew_active: false,
        forced_by: Some(flotilla_protocol::PrincipalRef { namespace: "flotilla".into(), name: "operator".into() }),
    };
    duplicate.apply(&mut status);
    assert_eq!(status.crew_work["implement"]["coder"].decision_ledger_digest.as_deref(), Some("admitted"));
    assert!(status.crew_work["implement"]["coder"].completion_override.is_none());
    if continuation == 2 {
        ConvoyStatusPatch::QueueMessageFollowUp {
            vessel: "implement".into(),
            role: "coder".into(),
            message: Some(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "follow-up")),
        }
        .apply(&mut status);
    }
    let patch = match continuation {
        0 => ConvoyStatusPatch::ResumeCrewWork {
            intent: ResumeIntent::Reopen,
            vessel: "implement".into(),
            role: "coder".into(),
            resumed_at: ts(40),
            prompt: "new turn".into(),
            brief_id: None,
        },
        1 => ConvoyStatusPatch::HandoffCrewWork {
            vessel: "implement".into(),
            sender_role: "reviewer".into(),
            target_role: "coder".into(),
            handed_off_at: ts(40),
            message: "new turn".into(),
        },
        2 => ConvoyStatusPatch::BeginMessageFollowUp {
            vessel: "implement".into(),
            role: "coder".into(),
            message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "follow-up"),
            content: "address review".into(),
            claim: flotilla_resources::SupersededCrewClaim {
                claimed_at: ts(40),
                message: None,
                disposition: None,
                decision_ledger_ref: None,
                decision_ledger_digest: Some("admitted".into()),
                completion_override: None,
                completed_while_crew_active: false,
            },
        },
        _ => ConvoyStatusPatch::RecordTurnDelivery {
            source: "review".into(),
            episode: TurnDeliveryEpisode {
                subject_revision: "new-head".into(),
                evidence_at: ts(40),
                judged_claim_at: ts(20),
                outcome: TurnDeliveryOutcome::Delivered { rung: TurnDeliveryRung::WarmSession, delivered_at: ts(40) },
            },
            vessel: "implement".into(),
            role: "coder".into(),
            prompt: "new turn".into(),
        },
    };
    patch.apply(&mut status);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert!(status.crew_work["implement"]["coder"].decision_ledger_digest.is_none());
    if continuation == 2 {
        assert!(status.crew_work["implement"]["coder"].superseded_claims[0].completion_override.is_none());
        assert_eq!(status.crew_work["implement"]["coder"].superseded_claims[0].decision_ledger_digest.as_deref(), Some("admitted"));
    }
}

// Suppression records an episode reference without reopening completed work or
// changing lifecycle timestamps. The receiver still owns the open expectation.
#[test]
fn suppressed_message_admission_does_not_open_another_workflow_turn() {
    let mut status = settled_convoy_status();
    status.phase = ConvoyPhase::Landing;
    let before = status.clone();
    let episode = TurnDeliveryEpisode {
        subject_revision: "head".into(),
        evidence_at: ts(30),
        judged_claim_at: ts(20),
        outcome: TurnDeliveryOutcome::MessageAccepted {
            new_turn: false,
            message: flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Message", "flotilla", "canonical"),
            rung: TurnDeliveryRung::WarmSession,
            accepted_at: ts(30),
        },
    };
    let patch = ConvoyStatusPatch::RecordTurnDelivery {
        source: "review".into(),
        episode,
        vessel: "implement".into(),
        role: "coder".into(),
        prompt: "suppressed turn".into(),
    };
    apply_and_replay(&mut status, &patch);
    assert_eq!(status.work, before.work);
    assert_eq!(status.crew_work, before.crew_work);
    assert_eq!(status.phase, before.phase);
    assert_eq!(convoy_timestamps(&status), convoy_timestamps(&before));
    assert_eq!(status.turn_deliveries["review"].episodes.len(), 1);
}
