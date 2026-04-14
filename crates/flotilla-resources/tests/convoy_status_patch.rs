use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use flotilla_resources::{
    controller_patches, ConvoyPhase, ConvoyStatus, ConvoyStatusPatch, ProcessDefinition, ProcessSource, Selector, SnapshotTask,
    StatusPatch, TaskPhase, TaskState, WorkflowSnapshot,
};

fn ts(seconds: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(seconds, 0).single().expect("valid timestamp")
}

fn sample_snapshot() -> WorkflowSnapshot {
    WorkflowSnapshot {
        tasks: vec![
            SnapshotTask {
                name: "implement".to_string(),
                depends_on: Vec::new(),
                processes: vec![
                    ProcessDefinition {
                        role: "coder".to_string(),
                        source: ProcessSource::Agent {
                            selector: Selector { capability: "code".to_string() },
                            prompt: Some("Implement {{inputs.feature}}".to_string()),
                        },
                    },
                    ProcessDefinition { role: "build".to_string(), source: ProcessSource::Tool { command: "cargo test".to_string() } },
                ],
            },
            SnapshotTask {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string()],
                processes: vec![ProcessDefinition {
                    role: "reviewer".to_string(),
                    source: ProcessSource::Agent {
                        selector: Selector { capability: "code-review".to_string() },
                        prompt: Some("Review {{inputs.feature}}".to_string()),
                    },
                }],
            },
        ],
    }
}

fn pending_task() -> TaskState {
    TaskState { phase: TaskPhase::Pending, ready_at: None, started_at: None, finished_at: None, message: None, placement: None }
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
    assert_eq!(status.tasks, tasks);
}

#[test]
fn advance_tasks_to_ready_updates_only_selected_tasks() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        tasks: BTreeMap::from([
            ("implement".to_string(), pending_task()),
            ("review".to_string(), TaskState {
                phase: TaskPhase::Completed,
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

    let patch = controller_patches::advance_tasks_to_ready(BTreeMap::from([("implement".to_string(), ts(10))]));
    patch.apply(&mut status);

    assert_eq!(status.tasks["implement"].phase, TaskPhase::Ready);
    assert_eq!(status.tasks["implement"].ready_at, Some(ts(10)));
    assert_eq!(status.tasks["review"].phase, TaskPhase::Completed);
    assert_eq!(status.message.as_deref(), Some("keep"));
}

#[test]
fn fail_convoy_cancels_non_terminal_siblings_and_sets_convoy_failed() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        tasks: BTreeMap::from([
            ("implement".to_string(), TaskState {
                phase: TaskPhase::Failed,
                ready_at: Some(ts(10)),
                started_at: Some(ts(11)),
                finished_at: Some(ts(12)),
                message: Some("boom".to_string()),
                placement: None,
            }),
            ("review".to_string(), TaskState {
                phase: TaskPhase::Running,
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
    assert_eq!(status.tasks["implement"].phase, TaskPhase::Failed);
    assert_eq!(status.tasks["review"].phase, TaskPhase::Cancelled);
    assert_eq!(status.tasks["review"].finished_at, Some(ts(30)));
}

#[test]
fn roll_up_phase_only_touches_convoy_level_fields() {
    let review = TaskState {
        phase: TaskPhase::Completed,
        ready_at: Some(ts(10)),
        started_at: Some(ts(11)),
        finished_at: Some(ts(12)),
        message: Some("done".to_string()),
        placement: None,
    };
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Pending,
        workflow_snapshot: Some(sample_snapshot()),
        tasks: BTreeMap::from([("review".to_string(), review.clone())]),
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
    assert_eq!(status.tasks["review"], review);
}

#[test]
fn external_completion_marks_task_complete_without_touching_convoy_phase() {
    let mut status = ConvoyStatus {
        phase: ConvoyPhase::Active,
        workflow_snapshot: Some(sample_snapshot()),
        tasks: BTreeMap::from([("review".to_string(), TaskState {
            phase: TaskPhase::Running,
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

    let patch = ConvoyStatusPatch::MarkTaskCompleted { task: "review".to_string(), finished_at: ts(50), message: Some("done".to_string()) };
    patch.apply(&mut status);

    assert_eq!(status.phase, ConvoyPhase::Active);
    assert_eq!(status.tasks["review"].phase, TaskPhase::Completed);
    assert_eq!(status.tasks["review"].finished_at, Some(ts(50)));
    assert_eq!(status.tasks["review"].message.as_deref(), Some("done"));
}
