use std::{env, path::PathBuf};

use flotilla_resources::{
    ensure_crd, ensure_namespace, validate, Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, HttpBackend, InputMeta, InputValue,
    ResourceBackend, WatchEvent, WatchStart, WorkflowTemplate, WorkflowTemplateSpec,
};
use futures::StreamExt;
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

fn sample_workflow_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/review-and-fix.yaml")
}

fn load_workflow_spec() -> Result<WorkflowTemplateSpec, Box<dyn std::error::Error>> {
    let yaml = std::fs::read_to_string(sample_workflow_path())?;
    let document: WorkflowTemplateDocument = serde_yml::from_str(&yaml)?;
    Ok(document.spec)
}

fn updated_workflow_spec() -> WorkflowTemplateSpec {
    let mut spec = load_workflow_spec().expect("sample workflow should parse");
    if let flotilla_resources::ProcessSource::Tool { command } = &mut spec.tasks[0].processes[1].source {
        *command = "cargo check --all-targets".to_string();
    }
    spec
}

fn convoy_spec(workflow_ref: &str) -> ConvoySpec {
    ConvoySpec {
        workflow_ref: workflow_ref.to_string(),
        inputs: [
            ("feature".to_string(), InputValue::String("Retry logic for the poller".to_string())),
            ("branch".to_string(), InputValue::String("fix-bug-123".to_string())),
        ]
        .into_iter()
        .collect(),
        placement_policy: Some("laptop-docker".to_string()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kubeconfig = kubeconfig_path();
    let backend = HttpBackend::from_kubeconfig(&kubeconfig)?;
    let namespace = "flotilla";
    ensure_namespace(&backend, namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/workflow_template.crd.yaml")).await?;

    let backend = ResourceBackend::Http(backend);
    let workflow_resolver = backend.clone().using::<WorkflowTemplate>(namespace);
    let convoy_resolver = backend.using::<Convoy>(namespace);
    let workflow_spec = load_workflow_spec()?;
    validate(&workflow_spec).map_err(|errors| format!("sample workflow failed validation: {errors:?}"))?;
    let workflow_meta = InputMeta {
        name: format!("demo-workflow-template-{}", std::process::id()),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: Default::default(),
    };
    if let Err(err) = workflow_resolver.delete(&workflow_meta.name).await {
        if !matches!(err, flotilla_resources::ResourceError::NotFound { .. }) {
            return Err(err.into());
        }
    }
    let convoy_meta = InputMeta {
        name: format!("demo-convoy-{}", std::process::id()),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: Default::default(),
    };
    if let Err(err) = convoy_resolver.delete(&convoy_meta.name).await {
        if !matches!(err, flotilla_resources::ResourceError::NotFound { .. }) {
            return Err(err.into());
        }
    }

    println!("creating workflow template");
    let created_workflow = workflow_resolver.create(&workflow_meta, &workflow_spec).await?;
    println!("created workflow {} rv={}", created_workflow.metadata.name, created_workflow.metadata.resource_version);

    println!("updating workflow template");
    let updated_workflow =
        workflow_resolver.update(&workflow_meta, &created_workflow.metadata.resource_version, &updated_workflow_spec()).await?;
    println!("updated workflow rv={}", updated_workflow.metadata.resource_version);

    println!("creating convoy");
    let created_convoy = convoy_resolver.create(&convoy_meta, &convoy_spec(&workflow_meta.name)).await?;
    println!("created convoy {} rv={}", created_convoy.metadata.name, created_convoy.metadata.resource_version);

    let listed = convoy_resolver.list().await?;
    println!("listed {} convoys at rv={}", listed.items.len(), listed.resource_version);

    let mut watch = convoy_resolver.watch(WatchStart::FromVersion(listed.resource_version.clone())).await?;
    println!("updating convoy status");
    let updated_convoy = convoy_resolver
        .update_status(&created_convoy.metadata.name, &created_convoy.metadata.resource_version, &ConvoyStatus {
            phase: ConvoyPhase::Active,
            workflow_snapshot: None,
            tasks: Default::default(),
            message: None,
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            observed_workflows: None,
        })
        .await?;
    println!("updated convoy rv={}", updated_convoy.metadata.resource_version);

    if let Some(event) = watch.next().await {
        match event? {
            WatchEvent::Modified(object) => {
                println!("watch saw modified resource rv={}", object.metadata.resource_version);
            }
            WatchEvent::Added(_) | WatchEvent::Deleted(_) => println!("watch saw non-modified event"),
        }
    }

    println!("deleting convoy");
    convoy_resolver.delete(&created_convoy.metadata.name).await?;
    if let Some(event) = watch.next().await {
        match event? {
            WatchEvent::Deleted(object) => println!("watch saw deleted resource rv={}", object.metadata.resource_version),
            WatchEvent::Added(_) | WatchEvent::Modified(_) => println!("watch saw non-deleted event"),
        }
    }

    println!("deleting workflow template");
    workflow_resolver.delete(&created_workflow.metadata.name).await?;
    Ok(())
}
