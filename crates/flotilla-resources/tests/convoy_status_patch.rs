use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use flotilla_resources::{
    controller_patches, ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, CrewSource, CrewSpec, Selector, StatusPatch, VesselRequirement,
    WorkPhase, WorkState, WorkflowSnapshot,
};

fn ts(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

fn sample_snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        vessels: vec![
            VesselRequirement {
                name: "implement".to_string(),
                depends_on: Vec::new(),
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
                depends_on: vec!["implement".to_string()],
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

fn pending_task() -> WorkState {
    WorkState { phase: WorkPhase::Pending, ready_at: None, started_at: None, finished_at: None, message: None, placement: None }
}

#[test]
fn bootstrap_sets_snapshot_and_initial_task_map() {
    let mut status = ConvoyStatus::default();
    let mut tasks = BTreeMap::new();
    tasks.insert("implement".to_string(), pending_task());
    tasks.insert("review".to_string(), pending_task());

    let patch = controller_patches::bootstrap(
        sample_snapshot(),
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        tasks.clone(),
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
    assert_eq!(status.work, tasks);
}

#[test]
fn advance_work_to_ready_updates_only_selected_tasks() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([
            ("implement".to_string(), pending_task()),
            ("review".to_string(), WorkState {
                phase: WorkPhase::Complete,
                ready_at: Some(ts(5)),
                started_at: Some(ts(6)),
                finished_at: Some(ts(7)),
                message: Some("done".to_string()),
                placement: None,
            }),
        ]),
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
                ready_at: Some(ts(10)),
                started_at: Some(ts(11)),
                finished_at: Some(ts(12)),
                message: Some("boom".to_string()),
                placement: None,
            }),
            ("review".to_string(), WorkState {
                phase: WorkPhase::Running,
                ready_at: Some(ts(20)),
                started_at: Some(ts(21)),
                finished_at: None,
                message: None,
                placement: None,
            }),
        ]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch = controller_patches::fail_convoy(BTreeMap::from([("review".to_string(), ts(30))]), ts(30), Some("task failed".to_string()));
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Failed);
    assert_eq!(status.finished_at, Some(ts(30)));
    assert_eq!(status.message.as_deref(), Some("task failed"));
    assert_eq!(status.work["implement"].phase, WorkPhase::Failed);
    assert_eq!(status.work["review"].phase, WorkPhase::Cancelled);
    assert_eq!(status.work["review"].finished_at, Some(ts(30)));
}

#[test]
fn roll_up_phase_only_touches_convoy_level_fields() {
    let review = WorkState {
        phase: WorkPhase::Complete,
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
fn external_completion_marks_task_complete_without_touching_convoy_phase() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("review".to_string(), WorkState {
            phase: WorkPhase::Running,
            ready_at: Some(ts(10)),
            started_at: Some(ts(11)),
            finished_at: None,
            message: None,
            placement: None,
        })]),
        message: None,
        started_at: Some(ts(1)),
        finished_at: None,
        observed_workflow_ref: Some("review-and-fix".to_string()),
        observed_workflows: Some(BTreeMap::from([("review-and-fix".to_string(), "42".to_string())])),
    };

    let patch = ConvoyStatusPatch::MarkWorkCompleted { work: "review".to_string(), finished_at: ts(50), message: Some("done".to_string()) };
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.work["review"].phase, WorkPhase::Complete);
    assert_eq!(status.work["review"].finished_at, Some(ts(50)));
    assert_eq!(status.work["review"].message.as_deref(), Some("done"));
}

#[test]
fn convoy_lifecycle_timestamps_are_set_once_per_transition() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        work: BTreeMap::from([("implement".to_string(), pending_task())]),
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

    ConvoyStatusPatch::MarkWorkCompleted { work: "implement".to_string(), finished_at: ts(30), message: Some("done".to_string()) }
        .apply(&mut status);
    ConvoyStatusPatch::MarkWorkCompleted { work: "implement".to_string(), finished_at: ts(31), message: Some("still done".to_string()) }
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
