mod common;

use common::{
    bootstrapped_convoy_status, convoy_object, pending_task_state, timestamp, valid_convoy_spec, valid_workflow_template_object,
};
use flotilla_resources::{
    controller_patches, reconcile, ConvoyEvent, ConvoyPhase, ConvoyStatusPatch, InputValue, TaskPhase, ValidationError,
};

#[test]
fn bootstrap_from_valid_template_returns_bootstrap_patch() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    let expected_tasks = [
        ("implement".to_string(), pending_task_state()),
        ("review".to_string(), pending_task_state()),
    ]
    .into_iter()
    .collect();
    let expected_patch = controller_patches::bootstrap(
        common::bootstrapped_convoy_status().workflow_snapshot.expect("snapshot"),
        "review-and-fix".to_string(),
        [("review-and-fix".to_string(), "42".to_string())].into_iter().collect(),
        expected_tasks,
        ConvoyPhase::Pending,
        None,
    );

    assert_eq!(outcome.patch, Some(expected_patch));
    assert!(outcome.events.is_empty());
}

#[test]
fn missing_template_fails_init() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);

    let outcome = reconcile(&convoy, None, timestamp(10));

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })
    ));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::TemplateNotFound { name }] if name == "review-and-fix"
    ));
}

#[test]
fn invalid_template_fails_init_with_validation_error_event() {
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), None);
    let mut template = valid_workflow_template_object("review-and-fix");
    template.spec.tasks[1].depends_on = vec!["missing".to_string()];

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })
    ));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::TemplateInvalid { name, errors }]
            if name == "review-and-fix"
                && matches!(errors.as_slice(), [ValidationError::UnknownDependency { task, missing }] if task == "review" && missing == "missing")
    ));
}

#[test]
fn missing_required_input_fails_init() {
    let mut spec = valid_convoy_spec();
    spec.inputs.remove("branch");
    let convoy = convoy_object("convoy-a", spec, None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })
    ));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::MissingInput { name }] if name == "branch"
    ));
}

#[test]
fn extra_input_is_allowed() {
    let mut spec = valid_convoy_spec();
    spec.inputs.insert("extra".to_string(), InputValue::String("ignored".to_string()));
    let convoy = convoy_object("convoy-a", spec, None);
    let template = valid_workflow_template_object("review-and-fix");

    let outcome = reconcile(&convoy, Some(&template), timestamp(10));

    assert!(matches!(outcome.patch, Some(ConvoyStatusPatch::Bootstrap { .. })));
    assert!(outcome.events.is_empty());
}

#[test]
fn fan_out_advances_all_newly_ready_tasks() {
    let spec = valid_convoy_spec();
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        tasks: vec![
            flotilla_resources::SnapshotTask { name: "a".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "b".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "c".to_string(), depends_on: Vec::new(), processes: Vec::new() },
        ],
    });
    status.tasks = [
        ("a".to_string(), pending_task_state()),
        ("b".to_string(), pending_task_state()),
        ("c".to_string(), pending_task_state()),
    ]
    .into_iter()
    .collect();

    let convoy = convoy_object("convoy-a", spec, Some(status));
    let outcome = reconcile(&convoy, None, timestamp(20));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_tasks_to_ready(
            [
                ("a".to_string(), timestamp(20)),
                ("b".to_string(), timestamp(20)),
                ("c".to_string(), timestamp(20)),
            ]
            .into_iter()
            .collect()
        ))
    );
}

#[test]
fn fan_in_waits_until_all_dependencies_complete() {
    let mut status = bootstrapped_convoy_status();
    status.workflow_snapshot = Some(flotilla_resources::WorkflowSnapshot {
        tasks: vec![
            flotilla_resources::SnapshotTask { name: "implement".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask { name: "verify".to_string(), depends_on: Vec::new(), processes: Vec::new() },
            flotilla_resources::SnapshotTask {
                name: "review".to_string(),
                depends_on: vec!["implement".to_string(), "verify".to_string()],
                processes: Vec::new(),
            },
        ],
    });
    status.tasks.insert("verify".to_string(), pending_task_state());
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Completed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(8));
    status.tasks.get_mut("verify").expect("verify").phase = TaskPhase::Running;
    status.tasks.get_mut("verify").expect("verify").started_at = Some(timestamp(9));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Pending;
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status.clone()));

    let first = reconcile(&convoy, None, timestamp(20));
    assert_eq!(
        first.patch,
        Some(controller_patches::roll_up_phase(ConvoyPhase::Active, Some(timestamp(20)), None))
    );

    status.tasks.get_mut("verify").expect("verify").phase = TaskPhase::Completed;
    status.tasks.get_mut("verify").expect("verify").finished_at = Some(timestamp(10));
    status.phase = ConvoyPhase::Active;

    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));
    let second = reconcile(&convoy, None, timestamp(21));

    assert_eq!(
        second.patch,
        Some(controller_patches::advance_tasks_to_ready(
            [("review".to_string(), timestamp(21))].into_iter().collect()
        ))
    );
}

#[test]
fn failed_task_triggers_fail_fast() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Failed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    status.tasks.get_mut("review").expect("review").phase = TaskPhase::Running;
    status.tasks.get_mut("review").expect("review").started_at = Some(timestamp(11));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(30));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::fail_convoy(
            [("review".to_string(), timestamp(30))].into_iter().collect(),
            timestamp(30),
            Some("task failure detected".to_string())
        ))
    );
}

#[test]
fn all_completed_rolls_up_to_completed() {
    let mut status = bootstrapped_convoy_status();
    status.phase = ConvoyPhase::Active;
    for task in status.tasks.values_mut() {
        task.phase = TaskPhase::Completed;
        task.finished_at = Some(timestamp(12));
    }
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(40));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::roll_up_phase(ConvoyPhase::Completed, None, Some(timestamp(40))))
    );
}

#[test]
fn workflow_ref_change_after_init_fails_defensively() {
    let mut spec = valid_convoy_spec();
    spec.workflow_ref = "new-template".to_string();
    let convoy = convoy_object("convoy-a", spec, Some(bootstrapped_convoy_status()));

    let outcome = reconcile(&convoy, None, timestamp(50));

    assert!(matches!(
        outcome.patch,
        Some(ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. })
    ));
    assert!(matches!(
        outcome.events.as_slice(),
        [ConvoyEvent::WorkflowRefChanged { from, to }] if from == "review-and-fix" && to == "new-template"
    ));
}

#[test]
fn snapshot_state_allows_advancement_without_template() {
    let mut status = bootstrapped_convoy_status();
    status.tasks.get_mut("implement").expect("implement").phase = TaskPhase::Completed;
    status.tasks.get_mut("implement").expect("implement").finished_at = Some(timestamp(12));
    let convoy = convoy_object("convoy-a", valid_convoy_spec(), Some(status));

    let outcome = reconcile(&convoy, None, timestamp(60));

    assert_eq!(
        outcome.patch,
        Some(controller_patches::advance_tasks_to_ready(
            [("review".to_string(), timestamp(60))].into_iter().collect()
        ))
    );
}
