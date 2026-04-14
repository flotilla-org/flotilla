use std::{env, path::PathBuf};

use flotilla_resources::{
    apply_status_patch, ensure_crd, ensure_namespace, external_patches, reconcile, validate, Convoy, ConvoyPhase, ConvoySpec, HttpBackend,
    InputMeta, InputValue, ResourceBackend, ResourceError, WorkflowTemplate, WorkflowTemplateSpec,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WorkflowTemplateDocument {
    spec: WorkflowTemplateSpec,
}

fn kubeconfig_path() -> PathBuf {
    if let Ok(path) = env::var("KUBECONFIG") {
        return PathBuf::from(path);
    }
    let home = env::var("HOME").expect("HOME must be set when KUBECONFIG is unset");
    PathBuf::from(home).join(".kube/config")
}

fn workflow_template_spec() -> WorkflowTemplateSpec {
    let yaml = include_str!("../examples/review-and-fix.yaml");
    let document: WorkflowTemplateDocument = serde_yml::from_str(yaml).expect("parse workflow template fixture");
    document.spec
}

fn convoy_spec(workflow_ref: &str) -> ConvoySpec {
    ConvoySpec {
        workflow_ref: workflow_ref.to_string(),
        inputs: [
            ("feature".to_string(), InputValue::String("Retry logic".to_string())),
            ("branch".to_string(), InputValue::String("fix-retry".to_string())),
        ]
        .into_iter()
        .collect(),
        placement_policy: Some("laptop-docker".to_string()),
    }
}

async fn reconcile_once(
    convoys: &flotilla_resources::TypedResolver<Convoy>,
    templates: &flotilla_resources::TypedResolver<WorkflowTemplate>,
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<flotilla_resources::ConvoyStatusPatch>, ResourceError> {
    let convoy = convoys.get(name).await?;
    let template = if convoy.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_none() {
        Some(templates.get(&convoy.spec.workflow_ref).await?)
    } else {
        None
    };
    let outcome = reconcile(&convoy, template.as_ref(), now);
    if let Some(patch) = outcome.patch.clone() {
        apply_status_patch(convoys, name, &patch).await?;
        Ok(Some(patch))
    } else {
        Ok(None)
    }
}

#[tokio::test]
#[ignore = "requires minikube or another Kubernetes cluster"]
async fn convoy_controller_roundtrip_and_cel_validation() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("FLOTILLA_RUN_K8S_TESTS").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let backend = HttpBackend::from_kubeconfig(kubeconfig_path())?;
    let namespace = "flotilla";
    ensure_namespace(&backend, namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/workflow_template.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;

    let backend = ResourceBackend::Http(backend);
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    let convoys = backend.using::<Convoy>(namespace);

    let workflow_meta = InputMeta {
        name: format!("workflow-template-{}", std::process::id()),
        labels: Default::default(),
        annotations: Default::default(),
    };
    let workflow_spec = workflow_template_spec();
    validate(&workflow_spec).map_err(|errors| format!("fixture workflow failed validation: {errors:?}"))?;
    let _workflow = templates.create(&workflow_meta, &workflow_spec).await?;

    let convoy_meta =
        InputMeta { name: format!("convoy-{}", std::process::id()), labels: Default::default(), annotations: Default::default() };
    let created = convoys.create(&convoy_meta, &convoy_spec(&workflow_meta.name)).await?;

    let mut changed_workflow = convoy_spec(&workflow_meta.name);
    changed_workflow.workflow_ref = format!("{}-other", workflow_meta.name);
    let workflow_err = convoys
        .update(&convoy_meta, &created.metadata.resource_version, &changed_workflow)
        .await
        .expect_err("workflow_ref update should be rejected");
    assert!(matches!(workflow_err, ResourceError::Invalid { .. }));

    let mut changed_inputs = convoy_spec(&workflow_meta.name);
    changed_inputs.inputs.insert("feature".to_string(), InputValue::String("Changed".to_string()));
    let current = convoys.get(&created.metadata.name).await?;
    let inputs_err = convoys
        .update(&convoy_meta, &current.metadata.resource_version, &changed_inputs)
        .await
        .expect_err("inputs update should be rejected");
    assert!(matches!(inputs_err, ResourceError::Invalid { .. }));

    reconcile_once(&convoys, &templates, &created.metadata.name, chrono::Utc::now()).await?;
    reconcile_once(&convoys, &templates, &created.metadata.name, chrono::Utc::now()).await?;

    apply_status_patch(
        &convoys,
        &created.metadata.name,
        &external_patches::mark_task_completed("implement".to_string(), chrono::Utc::now(), Some("implemented".to_string())),
    )
    .await?;
    let ready_review = reconcile_once(&convoys, &templates, &created.metadata.name, chrono::Utc::now()).await?;
    assert!(matches!(ready_review, Some(flotilla_resources::ConvoyStatusPatch::AdvanceTasksToReady { .. })));

    apply_status_patch(
        &convoys,
        &created.metadata.name,
        &external_patches::mark_task_completed("review".to_string(), chrono::Utc::now(), Some("reviewed".to_string())),
    )
    .await?;
    let completed = reconcile_once(&convoys, &templates, &created.metadata.name, chrono::Utc::now()).await?;
    assert!(matches!(completed, Some(flotilla_resources::ConvoyStatusPatch::RollUpPhase { phase: ConvoyPhase::Completed, .. })));

    let final_convoy = convoys.get(&created.metadata.name).await?;
    assert_eq!(final_convoy.status.expect("status").phase, ConvoyPhase::Completed);
    Ok(())
}
