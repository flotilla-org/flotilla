use std::{env, path::PathBuf, time::Duration};

use flotilla_resources::{
    apply_status_patch, ensure_crd, ensure_namespace, reconcile, Convoy, HttpBackend, ResourceBackend, ResourceError, WatchEvent,
    WatchStart, WorkflowTemplate,
};
use futures::StreamExt;
use tracing::{error, info, warn};

fn kubeconfig_path() -> PathBuf {
    if let Ok(path) = env::var("KUBECONFIG") {
        return PathBuf::from(path);
    }
    let home = env::var("HOME").expect("HOME must be set when KUBECONFIG is unset");
    PathBuf::from(home).join(".kube/config")
}

async fn reconcile_and_apply(
    convoys: &flotilla_resources::TypedResolver<Convoy>,
    templates: &flotilla_resources::TypedResolver<WorkflowTemplate>,
    name: &str,
) -> Result<(), flotilla_resources::ResourceError> {
    let convoy = convoys.get(name).await?;
    let template = if convoy.status.as_ref().and_then(|status| status.observed_workflow_ref.as_ref()).is_none() {
        match templates.get(&convoy.spec.workflow_ref).await {
            Ok(template) => Some(template),
            Err(ResourceError::NotFound { .. }) => None,
            Err(err) => return Err(err),
        }
    } else {
        None
    };
    let outcome = reconcile(&convoy, template.as_ref(), chrono::Utc::now());
    if let Some(patch) = outcome.patch {
        info!(convoy = %name, ?patch, "applying convoy patch");
        apply_status_patch(convoys, name, &patch).await?;
    }
    for event in outcome.events {
        info!(convoy = %name, ?event, "convoy reconcile event");
    }
    Ok(())
}

async fn resync_all(
    convoys: &flotilla_resources::TypedResolver<Convoy>,
    templates: &flotilla_resources::TypedResolver<WorkflowTemplate>,
) -> Result<(), flotilla_resources::ResourceError> {
    let listed = convoys.list().await?;
    for convoy in listed.items {
        reconcile_and_apply(convoys, templates, &convoy.metadata.name).await?;
    }
    Ok(())
}

fn parse_namespace() -> String {
    let mut args = env::args().skip(1);
    let mut namespace = "flotilla".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--namespace" => {
                namespace = args.next().expect("--namespace requires a value");
            }
            other => panic!("unexpected argument: {other}"),
        }
    }
    namespace
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_target(false).init();

    let namespace = parse_namespace();
    let backend = HttpBackend::from_kubeconfig(kubeconfig_path())?;
    ensure_namespace(&backend, &namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/workflow_template.crd.yaml")).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;

    let backend = ResourceBackend::Http(backend);
    let convoys = backend.clone().using::<Convoy>(&namespace);
    let templates = backend.using::<WorkflowTemplate>(&namespace);

    let listed = convoys.list().await?;
    for convoy in &listed.items {
        reconcile_and_apply(&convoys, &templates, &convoy.metadata.name).await?;
    }

    let mut watch = convoys.watch(WatchStart::FromVersion(listed.resource_version)).await?;
    let mut resync = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            maybe_event = watch.next() => {
                match maybe_event {
                    Some(Ok(WatchEvent::Added(convoy) | WatchEvent::Modified(convoy))) => {
                        if let Err(err) = reconcile_and_apply(&convoys, &templates, &convoy.metadata.name).await {
                            error!(convoy = %convoy.metadata.name, %err, "convoy reconcile failed");
                        }
                    }
                    Some(Ok(WatchEvent::Deleted(convoy))) => {
                        info!(convoy = %convoy.metadata.name, "convoy deleted");
                    }
                    Some(Err(err)) => {
                        warn!(%err, "convoy watch error; waiting for next resync");
                    }
                    None => {
                        warn!("convoy watch stream ended");
                        break;
                    }
                }
            }
            _ = resync.tick() => {
                if let Err(err) = resync_all(&convoys, &templates).await {
                    warn!(%err, "convoy resync failed");
                }
            }
        }
    }

    Ok(())
}
