mod common;

use common::{
    bootstrapped_tool_only_convoy_status, convoy_meta, task_provisioning_convoy_spec, timestamp, tool_only_workflow_template_object,
    valid_convoy_spec, workflow_template_meta,
};
use flotilla_resources::{
    apply_status_patch, controller::ControllerLoop, external_patches, reconcile, Convoy, ConvoyPhase, ConvoyReconciler, InMemoryBackend,
    InputMeta, Presentation, PresentationSpec, ResourceBackend, ResourceError, TaskWorkspace, TaskWorkspacePhase, TaskWorkspaceStatus,
    WorkflowTemplate, CONVOY_LABEL, TASK_LABEL,
};
use tokio::time::{timeout, Duration};

async fn reconcile_once(
    convoys: &flotilla_resources::TypedResolver<Convoy>,
    templates: &flotilla_resources::TypedResolver<WorkflowTemplate>,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<flotilla_resources::ConvoyStatusPatch> {
    let convoy = convoys.get(name).await.expect("convoy get should succeed");
    let template = if convoy.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_none() {
        match templates.get(&convoy.spec.workflow_ref).await {
            Ok(template) => Some(template),
            Err(ResourceError::NotFound { .. }) => None,
            Err(err) => panic!("template get should succeed: {err}"),
        }
    } else {
        None
    };

    let outcome = reconcile(&convoy, template.as_ref(), now);
    if let Some(patch) = outcome.patch.clone() {
        apply_status_patch(convoys, name, &patch).await.expect("apply patch should succeed");
        Some(patch)
    } else {
        None
    }
}

#[tokio::test]
async fn in_memory_controller_loop_drives_convoy_to_completion() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.using::<Convoy>("flotilla");

    let template = tool_only_workflow_template_object("review-and-fix");
    templates.create(&workflow_template_meta(&template.metadata.name), &template.spec).await.expect("template create should succeed");
    convoys.create(&convoy_meta("convoy-a"), &valid_convoy_spec()).await.expect("convoy create should succeed");

    let bootstrap = reconcile_once(&convoys, &templates, "convoy-a", timestamp(10)).await.expect("bootstrap patch");
    assert!(matches!(bootstrap, flotilla_resources::ConvoyStatusPatch::Bootstrap { .. }));

    let ready_implement = reconcile_once(&convoys, &templates, "convoy-a", timestamp(11)).await.expect("ready patch after bootstrap");
    assert!(matches!(ready_implement, flotilla_resources::ConvoyStatusPatch::AdvanceTasksToReady { .. }));

    apply_status_patch(
        &convoys,
        "convoy-a",
        &external_patches::mark_task_completed("implement".to_string(), timestamp(12), Some("implemented".to_string())),
    )
    .await
    .expect("implement completion should succeed");

    let ready_review = reconcile_once(&convoys, &templates, "convoy-a", timestamp(13)).await.expect("review should become ready");
    assert!(matches!(ready_review, flotilla_resources::ConvoyStatusPatch::AdvanceTasksToReady { .. }));

    apply_status_patch(
        &convoys,
        "convoy-a",
        &external_patches::mark_task_completed("review".to_string(), timestamp(14), Some("reviewed".to_string())),
    )
    .await
    .expect("review completion should succeed");

    let completed = reconcile_once(&convoys, &templates, "convoy-a", timestamp(15)).await.expect("completed roll-up patch");
    assert!(matches!(completed, flotilla_resources::ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, .. }));

    let final_convoy = convoys.get("convoy-a").await.expect("final convoy get should succeed");
    let final_status = final_convoy.status.expect("convoy status");
    assert_eq!(final_status.phase, ConvoyPhase::Completed);
    assert_eq!(final_status.tasks["implement"].phase, flotilla_resources::TaskPhase::Completed);
    assert_eq!(final_status.tasks["review"].phase, flotilla_resources::TaskPhase::Completed);
}

#[tokio::test]
async fn missing_template_transitions_convoy_to_failed() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.using::<Convoy>("flotilla");

    convoys.create(&convoy_meta("convoy-missing-template"), &valid_convoy_spec()).await.expect("convoy create should succeed");

    let patch = reconcile_once(&convoys, &templates, "convoy-missing-template", timestamp(10)).await.expect("fail-init patch");
    assert!(matches!(patch, flotilla_resources::ConvoyStatusPatch::FailInit { phase: ConvoyPhase::Failed, .. }));

    let convoy = convoys.get("convoy-missing-template").await.expect("convoy get should succeed");
    let status = convoy.status.expect("convoy status");
    assert_eq!(status.phase, ConvoyPhase::Failed);
    assert!(status.message.as_deref().is_some_and(|message| message.contains("not found")));
}

#[tokio::test]
async fn controller_loop_drives_convoy_progression_without_manual_reconcile_calls() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");

    let template = tool_only_workflow_template_object("review-and-fix");
    templates.create(&workflow_template_meta(&template.metadata.name), &template.spec).await.expect("template create should succeed");
    convoys.create(&convoy_meta("convoy-loop"), &valid_convoy_spec()).await.expect("convoy create should succeed");

    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: Vec::new(),
            reconciler: ConvoyReconciler::new(templates.clone()).with_task_workspaces(backend.clone().using::<TaskWorkspace>("flotilla")),
            resync_interval: Duration::from_secs(60),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let convoy = convoys.get("convoy-loop").await.expect("convoy get should succeed");
            let Some(status) = convoy.status else {
                tokio::task::yield_now().await;
                continue;
            };
            if status.tasks.get("implement").is_some_and(|task| task.phase == flotilla_resources::TaskPhase::Ready) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should bootstrap and advance implement");

    apply_status_patch(
        &convoys,
        "convoy-loop",
        &external_patches::mark_task_completed("implement".to_string(), timestamp(12), Some("implemented".to_string())),
    )
    .await
    .expect("implement completion should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let convoy = convoys.get("convoy-loop").await.expect("convoy get should succeed");
            let status = convoy.status.expect("convoy status");
            if status.tasks.get("review").is_some_and(|task| task.phase == flotilla_resources::TaskPhase::Ready) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should advance review after implement completion");

    apply_status_patch(
        &convoys,
        "convoy-loop",
        &external_patches::mark_task_completed("review".to_string(), timestamp(14), Some("reviewed".to_string())),
    )
    .await
    .expect("review completion should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let convoy = convoys.get("convoy-loop").await.expect("convoy get should succeed");
            let status = convoy.status.expect("convoy status");
            if status.phase == ConvoyPhase::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should roll convoy up to completed");

    loop_task.abort();
}

#[tokio::test]
async fn controller_loop_advances_task_via_task_workspace_secondary_watch() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let templates = backend.clone().using::<WorkflowTemplate>("flotilla");
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let workspaces = backend.clone().using::<TaskWorkspace>("flotilla");

    let template = tool_only_workflow_template_object("review-and-fix");
    templates.create(&workflow_template_meta(&template.metadata.name), &template.spec).await.expect("template create should succeed");
    convoys.create(&convoy_meta("convoy-stage4a"), &task_provisioning_convoy_spec()).await.expect("convoy create should succeed");

    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: ConvoyReconciler::secondary_watches(),
            reconciler: ConvoyReconciler::new(templates.clone()).with_task_workspaces(workspaces.clone()),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            if workspaces.get("convoy-stage4a-implement").await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should create a task workspace for the ready task");

    let workspace = workspaces.get("convoy-stage4a-implement").await.expect("workspace get should succeed");
    workspaces
        .update_status("convoy-stage4a-implement", &workspace.metadata.resource_version, &TaskWorkspaceStatus {
            phase: TaskWorkspacePhase::Ready,
            message: None,
            observed_policy_ref: Some("laptop-docker".to_string()),
            observed_policy_version: Some("17".to_string()),
            environment_ref: Some("env-implement".to_string()),
            checkout_ref: Some("checkout-implement".to_string()),
            terminal_session_refs: vec!["terminal-implement-coder".to_string()],
            started_at: Some(timestamp(18)),
            ready_at: Some(timestamp(19)),
        })
        .await
        .expect("workspace status update should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            let convoy = convoys.get("convoy-stage4a").await.expect("convoy get should succeed");
            let Some(status) = convoy.status else {
                tokio::task::yield_now().await;
                continue;
            };
            if status.tasks.get("implement").is_some_and(|task| task.phase == flotilla_resources::TaskPhase::Running) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should advance the task to running after the workspace becomes ready");

    loop_task.abort();
}

#[tokio::test]
async fn controller_loop_finalizer_deletes_presentations_and_task_workspaces() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let convoys = backend.clone().using::<Convoy>("flotilla");
    let workspaces = backend.clone().using::<TaskWorkspace>("flotilla");
    let presentations = backend.clone().using::<Presentation>("flotilla");

    let created = convoys
        .create(&convoy_meta("convoy-delete"), &task_provisioning_convoy_spec())
        .await
        .expect("convoy create should succeed");
    let mut status = bootstrapped_tool_only_convoy_status();
    status.phase = ConvoyPhase::Active;
    status.started_at = Some(timestamp(18));
    status.tasks.get_mut("implement").expect("implement").phase = flotilla_resources::TaskPhase::Running;
    status.tasks.get_mut("implement").expect("implement").started_at = Some(timestamp(18));
    convoys
        .update_status("convoy-delete", &created.metadata.resource_version, &status)
        .await
        .expect("convoy status update should succeed");

    workspaces
        .create(
            &InputMeta::builder()
                .name("convoy-delete-implement".to_string())
                .labels([
                    (CONVOY_LABEL.to_string(), "convoy-delete".to_string()),
                    (TASK_LABEL.to_string(), "implement".to_string()),
                ]
                .into_iter()
                .collect())
                .build(),
            &flotilla_resources::TaskWorkspaceSpec {
                convoy_ref: "convoy-delete".to_string(),
                task: "implement".to_string(),
                placement_policy_ref: "laptop-docker".to_string(),
            },
        )
        .await
        .expect("task workspace create should succeed");
    presentations
        .create(
            &InputMeta::builder()
                .name("convoy-delete-presentation".to_string())
                .labels([(CONVOY_LABEL.to_string(), "convoy-delete".to_string())].into_iter().collect())
                .build(),
            &PresentationSpec {
                convoy_ref: "convoy-delete".to_string(),
                presentation_policy_ref: "default".to_string(),
                name: "convoy-delete".to_string(),
                process_selector: [(CONVOY_LABEL.to_string(), "convoy-delete".to_string())].into_iter().collect(),
            },
        )
        .await
        .expect("presentation create should succeed");

    let loop_task = tokio::spawn(
        ControllerLoop {
            primary: convoys.clone(),
            secondaries: ConvoyReconciler::secondary_watches(),
            reconciler: ConvoyReconciler::new(backend.clone().using::<WorkflowTemplate>("flotilla"))
                .with_task_workspaces(workspaces.clone())
                .with_presentations(presentations.clone()),
            resync_interval: Duration::from_millis(50),
            backend: backend.clone(),
        }
        .run(),
    );

    timeout(Duration::from_secs(1), async {
        loop {
            let convoy = convoys.get("convoy-delete").await.expect("convoy get should succeed");
            if convoy.metadata.finalizers == vec!["flotilla.work/convoy-teardown".to_string()] {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controller loop should attach convoy finalizer");

    convoys.delete("convoy-delete").await.expect("convoy delete should succeed");

    timeout(Duration::from_secs(1), async {
        loop {
            if matches!(convoys.get("convoy-delete").await, Err(ResourceError::NotFound { .. }))
                && matches!(workspaces.get("convoy-delete-implement").await, Err(ResourceError::NotFound { .. }))
                && matches!(presentations.get("convoy-delete-presentation").await, Err(ResourceError::NotFound { .. }))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("convoy finalizer should delete presentation and task workspaces");

    loop_task.abort();
}
