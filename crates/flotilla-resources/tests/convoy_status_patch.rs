use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use flotilla_resources::{
    controller_patches, external_patches, provisioning_patches, ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, CrewSource, CrewSpec,
    CrewWorkPhase, CrewWorkState, Selector, StatusPatch, VesselRequirement, WorkCompletionAuthority, WorkPhase, WorkState,
    WorkflowSnapshot,
};

fn ts(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

fn sample_snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        vessels: vec![
            VesselRequirement {
                name: "implement".to_string(),
                stance: Default::default(),
                depends_on: Vec::new(),
                repository_refs: None,
                crew: vec![
                    CrewSpec {
                        role: "coder".to_string(),
                        source: CrewSource::Agent {
                            selector: Selector { capability: "code".to_string() },
                            prompt: Some("Implement {{inputs.feature}}".to_string()),
                        },
                        labels: BTreeMap::new(),
                    },
                    CrewSpec {
                        role: "build".to_string(),
                        source: CrewSource::Tool { command: "cargo test".to_string() },
                        labels: BTreeMap::new(),
                    },
                ],
            },
            VesselRequirement {
                name: "review".to_string(),
                stance: Default::default(),
                depends_on: vec!["implement".to_string()],
                repository_refs: None,
                crew: vec![CrewSpec {
                    role: "reviewer".to_string(),
                    source: CrewSource::Agent {
                        selector: Selector { capability: "code-review".to_string() },
                        prompt: Some("Review {{inputs.feature}}".to_string()),
                    },
                    labels: BTreeMap::new(),
                }],
            },
        ],
    }
}

fn pending_work() -> WorkState {
    WorkState {
        phase: WorkPhase::Pending,
        completion_authority: WorkCompletionAuthority::CrewRollup,
        ready_at: None,
        started_at: None,
        finished_at: None,
        message: None,
        placement: None,
    }
}

fn crew_work(phase: CrewWorkPhase) -> CrewWorkState {
    CrewWorkState::builder().phase(phase).started_at(ts(10)).build()
}

#[test]
fn crew_completion_updates_only_the_calling_agent() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), WorkState {
            phase: WorkPhase::Running,
            completion_authority: WorkCompletionAuthority::CrewRollup,
            ready_at: Some(ts(8)),
            started_at: Some(ts(9)),
            finished_at: None,
            message: None,
            placement: None,
        })]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::Working)),
            ]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    external_patches::mark_crew_completed("implement".to_string(), "coder".to_string(), ts(20), Some("ready for review".to_string()))
        .apply(&mut status);
    external_patches::mark_crew_completed("implement".to_string(), "coder".to_string(), ts(30), Some("still ready".to_string()))
        .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Done);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, Some(ts(20)));
    assert_eq!(status.crew_work["implement"]["coder"].message.as_deref(), Some("still ready"));
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Working);
    assert_eq!(status.work["implement"].phase, WorkPhase::Running);
}

#[test]
fn crew_failure_records_terminal_state_and_message() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::new(),
        crew_work: BTreeMap::from([("implement".to_string(), BTreeMap::from([("coder".to_string(), crew_work(CrewWorkPhase::Working))]))]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    external_patches::mark_crew_completed("implement".to_string(), "coder".to_string(), ts(15), Some("initially done".to_string()))
        .apply(&mut status);
    external_patches::mark_crew_failed("implement".to_string(), "coder".to_string(), ts(20), "blocked by missing credentials".to_string())
        .apply(&mut status);
    external_patches::mark_crew_failed("implement".to_string(), "coder".to_string(), ts(30), "still blocked".to_string())
        .apply(&mut status);

    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Failed);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, Some(ts(20)));
    assert_eq!(status.crew_work["implement"]["coder"].message.as_deref(), Some("still blocked"));
}

#[test]
fn handoff_to_done_crew_reopens_target_and_marks_sender_handed_back() {
    let mut coder = crew_work(CrewWorkPhase::Done);
    coder.finished_at = Some(ts(15));
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Completed,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), WorkState {
            phase: WorkPhase::Complete,
            completion_authority: WorkCompletionAuthority::HumanOverride,
            ready_at: Some(ts(8)),
            started_at: Some(ts(9)),
            finished_at: Some(ts(16)),
            message: Some("complete".to_string()),
            placement: None,
        })]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), coder), ("reviewer".to_string(), crew_work(CrewWorkPhase::Working))]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: Some(ts(16)),
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    external_patches::handoff_crew_work(
        "implement".to_string(),
        "reviewer".to_string(),
        "coder".to_string(),
        ts(20),
        "address review findings".to_string(),
    )
    .apply(&mut status);

    assert_eq!(status.work["implement"].completion_authority, WorkCompletionAuthority::CrewRollup);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].finished_at, None);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::HandedBack);
    assert_eq!(status.crew_work["implement"]["reviewer"].finished_at, Some(ts(20)));
    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);
}

#[test]
fn running_vessel_work_starts_pending_agents_without_reopening_done_agents() {
    let mut pending_coder = crew_work(CrewWorkPhase::Pending);
    pending_coder.started_at = None;
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), WorkState {
            phase: WorkPhase::Launching,
            completion_authority: WorkCompletionAuthority::CrewRollup,
            ready_at: Some(ts(8)),
            started_at: Some(ts(9)),
            finished_at: None,
            message: None,
            placement: None,
        })]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([("coder".to_string(), pending_coder), ("reviewer".to_string(), crew_work(CrewWorkPhase::Done))]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    provisioning_patches::work_running("implement".to_string(), ts(12)).apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Running);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["coder"].started_at, Some(ts(12)));
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::Done);
}

#[test]
fn bootstrap_sets_snapshot_and_initial_work_map() {
    let mut status = ConvoyStatus::default();
    let mut work = BTreeMap::new();
    work.insert("implement".to_string(), pending_work());
    work.insert("review".to_string(), pending_work());

    let patch = controller_patches::bootstrap(
        sample_snapshot(),
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        work.clone(),
        BTreeMap::new(),
        ConvoyPhase::Pending,
        None,
    );

    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Pending);
    assert_eq!(status.workflow_snapshot, Some(sample_snapshot()));
    assert_eq!(status.observed_workflow_ref.as_deref(), Some("review-and-fix"));
    assert_eq!(
        status.observed_workflows.as_ref().expect("observed workflows"),
        &BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])
    );
    assert_eq!(status.work, work);
}

#[test]
fn advance_work_to_ready_updates_only_selected_vessels() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            ("implement".to_string(), pending_work()),
            ("review".to_string(), WorkState {
                phase: WorkPhase::Complete,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(5)),
                started_at: Some(ts(6)),
                finished_at: Some(ts(7)),
                message: Some("done".to_string()),
                placement: None,
            }),
        ]),
        crew_work: BTreeMap::new(),
        message: Some("keep".to_string()),
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch = controller_patches::advance_work_to_ready(BTreeMap::from([("implement".to_string(), ts(10))]));
    patch.apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Ready);
    assert_eq!(status.work["implement"].ready_at, Some(ts(10)));
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
    assert_eq!(status.message.as_deref(), Some("keep"));
}

#[test]
fn fail_convoy_cancels_non_terminal_siblings_and_sets_convoy_failed() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            ("implement".to_string(), WorkState {
                phase: WorkPhase::Failed,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(10)),
                started_at: Some(ts(11)),
                finished_at: Some(ts(12)),
                message: Some("boom".to_string()),
                placement: None,
            }),
            ("review".to_string(), WorkState {
                phase: WorkPhase::Running,
                completion_authority: WorkCompletionAuthority::CrewRollup,
                ready_at: Some(ts(20)),
                started_at: Some(ts(21)),
                finished_at: None,
                message: None,
                placement: None,
            }),
        ]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch = controller_patches::fail_convoy(BTreeMap::from([("review".to_string(), ts(30))]), ts(30), Some("work failed".to_string()));
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Failed);
    assert_eq!(status.finished_at, Some(ts(30)));
    assert_eq!(status.message.as_deref(), Some("work failed"));
    assert_eq!(status.work["implement"].phase, WorkPhase::Failed);
    assert_eq!(status.work["review"].phase, WorkPhase::Cancelled);
    assert_eq!(status.work["review"].finished_at, Some(ts(30)));
}

#[test]
fn roll_up_phase_only_touches_convoy_level_fields() {
    let review = WorkState {
        phase: WorkPhase::Complete,
        completion_authority: WorkCompletionAuthority::CrewRollup,
        ready_at: Some(ts(10)),
        started_at: Some(ts(11)),
        finished_at: Some(ts(12)),
        message: Some("done".to_string()),
        placement: None,
    };
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("review".to_string(), review.clone())]),
        crew_work: BTreeMap::new(),
        message: Some("keep".to_string()),
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch = controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(ts(40)));
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Completed);
    assert_eq!(status.finished_at, Some(ts(40)));
    assert_eq!(status.message.as_deref(), Some("keep"));
    assert_eq!(status.work["review"], review);
}

#[test]
fn external_completion_marks_work_complete_without_touching_convoy_phase() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("review".to_string(), WorkState {
            phase: WorkPhase::Running,
            completion_authority: WorkCompletionAuthority::CrewRollup,
            ready_at: Some(ts(10)),
            started_at: Some(ts(11)),
            finished_at: None,
            message: None,
            placement: None,
        })]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch =
        ConvoyStatusPatch::ForceWorkCompleted { work: "review".to_string(), finished_at: ts(50), message: Some("done".to_string()) };
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
    assert_eq!(status.work["review"].completion_authority, WorkCompletionAuthority::HumanOverride);
    assert_eq!(status.work["review"].finished_at, Some(ts(50)));
    assert_eq!(status.work["review"].message.as_deref(), Some("done"));
}

#[test]
fn forced_work_completion_preserves_agent_owned_state() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), WorkState {
            phase: WorkPhase::Running,
            completion_authority: WorkCompletionAuthority::CrewRollup,
            ready_at: None,
            started_at: None,
            finished_at: None,
            message: None,
            placement: None,
        })]),
        crew_work: BTreeMap::from([(
            "implement".to_string(),
            BTreeMap::from([
                ("coder".to_string(), crew_work(CrewWorkPhase::Working)),
                ("reviewer".to_string(), crew_work(CrewWorkPhase::HandedBack)),
            ]),
        )]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    external_patches::force_work_completed("implement".to_string(), ts(50), Some("human override".to_string())).apply(&mut status);

    assert_eq!(status.work["implement"].phase, WorkPhase::Complete);
    assert_eq!(status.work["implement"].completion_authority, WorkCompletionAuthority::HumanOverride);
    assert_eq!(status.crew_work["implement"]["coder"].phase, CrewWorkPhase::Working);
    assert_eq!(status.crew_work["implement"]["reviewer"].phase, CrewWorkPhase::HandedBack);
    assert_eq!(status.crew_work["implement"]["reviewer"].finished_at, None);
}

#[test]
fn convoy_lifecycle_timestamps_are_set_once_per_transition() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), pending_work())]),
        crew_work: BTreeMap::new(),
        message: None,
        started_at: None,
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::new()),
    };

    ConvoyStatusPatch::AdvanceWorkToReady { ready: BTreeMap::from([("implement".to_string(), ts(10))]) }.apply(&mut status);
    ConvoyStatusPatch::AdvanceWorkToReady { ready: BTreeMap::from([("implement".to_string(), ts(11))]) }.apply(&mut status);
    assert_eq!(status.work["implement"].ready_at, Some(ts(10)));

    ConvoyStatusPatch::WorkLaunching {
        work: "implement".to_string(),
        started_at: ts(20),
        placement: flotilla_resources::PlacementStatus::default(),
    }
    .apply(&mut status);
    ConvoyStatusPatch::WorkLaunching {
        work: "implement".to_string(),
        started_at: ts(21),
        placement: flotilla_resources::PlacementStatus::default(),
    }
    .apply(&mut status);
    assert_eq!(status.work["implement"].started_at, Some(ts(20)));

    ConvoyStatusPatch::ForceWorkCompleted { work: "implement".to_string(), finished_at: ts(30), message: Some("done".to_string()) }
        .apply(&mut status);
    ConvoyStatusPatch::ForceWorkCompleted { work: "implement".to_string(), finished_at: ts(31), message: Some("still done".to_string()) }
        .apply(&mut status);
    assert_eq!(status.work["implement"].finished_at, Some(ts(30)));
    assert_eq!(status.work["implement"].message.as_deref(), Some("still done"));

    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(40)), finished_at: None }.apply(&mut status);
    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Active, started_at: Some(ts(41)), finished_at: None }.apply(&mut status);
    assert_eq!(status.started_at, Some(ts(40)));

    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, started_at: None, finished_at: Some(ts(50)) }.apply(&mut status);
    ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, started_at: None, finished_at: Some(ts(51)) }.apply(&mut status);
    assert_eq!(status.finished_at, Some(ts(50)));
}
