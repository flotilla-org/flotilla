use std::{env, path::PathBuf};

use flotilla_resources::{
    ensure_crd, ensure_namespace, ApiPaths, HttpBackend, InputMeta, Resource, ResourceBackend, WatchEvent, WatchStart,
};
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

#[tokio::test]
#[ignore = "requires minikube or another Kubernetes cluster"]
async fn k8s_crud_and_watch_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("FLOTILLA_RUN_K8S_TESTS").ok().as_deref() != Some("1") {
        return Ok(());
    }

    let backend = HttpBackend::from_kubeconfig(kubeconfig_path())?;
    let namespace = "flotilla";
    ensure_namespace(&backend, namespace).await?;
    ensure_crd(&backend, include_str!("../src/crds/convoy.crd.yaml")).await?;

    let resolver = ResourceBackend::Http(backend).using::<ConvoyResource>(namespace);
    let meta = InputMeta { name: format!("convoy-{}", std::process::id()), labels: Default::default(), annotations: Default::default() };

    let created = resolver.create(&meta, &ConvoySpec { template: "review".to_string() }).await?;
    let listed = resolver.list().await?;
    let mut watch = resolver.watch(WatchStart::FromVersion(listed.resource_version.clone())).await?;

    let updated = resolver
        .update_status(&created.metadata.name, &created.metadata.resource_version, &ConvoyStatus { phase: "Running".to_string() })
        .await?;

    match watch.next().await.expect("watch event")? {
        WatchEvent::Modified(object) => {
            assert_eq!(object.metadata.name, created.metadata.name);
            assert_eq!(object.status.expect("status").phase, "Running");
        }
        _ => panic!("expected modified event"),
    }

    resolver.delete(&created.metadata.name).await?;
    match watch.next().await.expect("watch event")? {
        WatchEvent::Deleted(object) => assert_eq!(object.metadata.name, created.metadata.name),
        _ => panic!("expected deleted event"),
    }

    assert_eq!(updated.status.expect("status").phase, "Running");
    Ok(())
}
