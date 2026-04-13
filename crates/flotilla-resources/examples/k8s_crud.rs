use std::{env, path::PathBuf};

use flotilla_resources::{ensure_crd, ensure_namespace, ApiPaths, HttpBackend, InputMeta, Resource, WatchEvent, WatchStart};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

struct ConvoyResource;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConvoySpec {
    template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConvoyStatus {
    phase: String,
}

impl Resource for ConvoyResource {
    type Spec = ConvoySpec;
    type Status = ConvoyStatus;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.io", version: "v1", plural: "convoys", kind: "Convoy" };
}

fn kubeconfig_path() -> PathBuf {
    if let Ok(path) = env::var("KUBECONFIG") {
        return PathBuf::from(path);
    }
    let home = env::var("HOME").expect("HOME must be set when KUBECONFIG is unset");
    PathBuf::from(home).join(".kube/config")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let kubeconfig = kubeconfig_path();
    let backend = HttpBackend::from_kubeconfig(&kubeconfig)?;
    let namespace = "flotilla";
    ensure_namespace(&backend, namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;

    let resolver = flotilla_resources::ResourceBackend::Http(backend).using::<ConvoyResource>(namespace);
    let meta = InputMeta {
        name: "demo-convoy".to_string(),
        labels: [("app".to_string(), "flotilla".to_string())].into_iter().collect(),
        annotations: Default::default(),
    };

    println!("creating resource");
    let created = resolver.create(&meta, &ConvoySpec { template: "review-and-fix".to_string() }).await?;
    println!("created {} rv={}", created.metadata.name, created.metadata.resource_version);

    let listed = resolver.list().await?;
    println!("listed {} resources at rv={}", listed.items.len(), listed.resource_version);

    let mut watch = resolver.watch(WatchStart::FromVersion(listed.resource_version.clone())).await?;
    println!("updating status");
    let updated = resolver
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus { phase: "Running".to_string() })
        .await?;
    println!("status rv={}", updated.metadata.resource_version);

    if let Some(event) = watch.next().await {
        match event? {
            WatchEvent::Modified(object) => {
                let phase = object.status.map(|status| status.phase).unwrap_or_else(|| "unknown".to_string());
                println!("watch saw modified phase={phase}");
            }
            WatchEvent::Added(_) | WatchEvent::Deleted(_) => println!("watch saw non-modified event"),
        }
    }

    println!("deleting resource");
    resolver.delete(&created.metadata.name).await?;
    Ok(())
}
