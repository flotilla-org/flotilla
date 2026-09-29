use chrono::Utc;
use flotilla_resources::{
    DocumentKey, DocumentPhase, DocumentState, InMemoryBackend, InputMeta, ManifestRoot, ManifestRootSpec, ManifestRootStatus, Resolution,
    ResolutionAction, ResourceBackend, ResourceError, SqliteBackend,
};

async fn contract(backend: ResourceBackend) {
    let roots = backend.using::<ManifestRoot>("flotilla");
    let key =
        DocumentKey { path: "sub/policy.yaml".into(), kind: "PlacementPolicy".into(), namespace: "flotilla".into(), name: "demo".into() };
    let spec = ManifestRootSpec::builder().host("host-a".into()).path("/tmp/manifests".into()).source("source".into()).build();
    let root = roots.create(&InputMeta::builder().name("host-a".into()).build(), &spec).await.expect("create root");
    let mut requested = root.spec.clone();
    requested.resolutions.insert(key.clone(), Resolution {
        action: ResolutionAction::Sync,
        token: "token-1".into(),
        requested_by: "operator".into(),
    });
    let updated = roots.update(&InputMeta::from(&root.metadata), &root.metadata.resource_version, &requested).await.expect("request sync");
    let mut status = ManifestRootStatus::default();
    status.documents.insert(key.clone(), DocumentState {
        phase: DocumentPhase::Refused,
        reason: Some("spec.pool: invalid".into()),
        live_hash: Some("live".into()),
        desired_hash: Some("desired".into()),
        baseline_hash: Some("baseline".into()),
        observed_at: Utc::now(),
        resolved_token: None,
        resolution_outcome: None,
    });
    let status_updated = roots.update_status("host-a", &updated.metadata.resource_version, &status).await.expect("publish status");
    assert_eq!(status_updated.spec, requested);
    assert_eq!(status_updated.status.as_ref().expect("status").documents[&key].reason.as_deref(), Some("spec.pool: invalid"));
    assert!(matches!(
        roots.update_status("host-a", &updated.metadata.resource_version, &status).await,
        Err(ResourceError::Conflict { .. })
    ));
    let encoded = serde_json::to_value(status_updated.to_k8s_object()).expect("encode");
    let decoded: flotilla_resources::K8sResourceObject<ManifestRoot> = serde_json::from_value(encoded).expect("decode map key");
    assert!(decoded.status.expect("status").documents.contains_key(&key));
}

#[tokio::test]
async fn in_memory_contract() {
    contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_contract() {
    contract(ResourceBackend::Sqlite(SqliteBackend::open_in_memory().expect("sqlite"))).await;
}
