mod common;

use common::{convoy_meta, timestamp, valid_convoy_spec, valid_workflow_template_object, workflow_template_meta};
use flotilla_resources::{
    apply_status_patch, external_patches, reconcile, Convoy, ConvoyPhase, InMemoryBackend, ResourceBackend, ResourceError, WorkflowTemplate,
};

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

    let template = valid_workflow_template_object("review-and-fix");
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
