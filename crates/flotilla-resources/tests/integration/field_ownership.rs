use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    apply_resource_document, ApiPaths, DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec,
    FieldOwnedResource, FieldOwnership, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InMemoryBackend, InputMeta,
    NoStatusPatch, OwnershipEnforcement, PlacementPolicy, PlacementPolicySpec, ReplicationClass, Resource, ResourceBackend, ResourceError,
    WriterIdentity, WriterRole,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

fn host_direct(pool: &str, priority: i32, host: &str) -> PlacementPolicySpec {
    PlacementPolicySpec::builder()
        .pool(pool.to_string())
        .priority(priority)
        .host_direct(HostDirectPlacementPolicySpec { host_ref: host.to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build()
}

fn docker(pool: &str, priority: i32, host: &str, image: &str) -> PlacementPolicySpec {
    PlacementPolicySpec::builder()
        .pool(pool.to_string())
        .priority(priority)
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            legacy_image_baseline_ref: None,
            memory_policy: Default::default(),
            host_ref: host.to_string(),
            image: image.to_string().into(),
            pull_policy: DockerImagePullPolicy::IfNotPresent,
            agent_adapters: BTreeSet::from(["codex".to_string()]),
            default_cwd: Some("/workspace".to_string()),
            env: BTreeMap::new(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
        .build()
}

#[test]
fn placement_policy_declares_every_spec_leaf_and_no_status_fields() {
    let declared = <PlacementPolicy as FieldOwnedResource>::FIELD_OWNERSHIP
        .iter()
        .map(|ownership| (ownership.field, ownership.owner))
        .collect::<Vec<_>>();
    assert_eq!(
        declared,
        vec![
            ("spec.pool", WriterRole::ReconcileLoop),
            ("spec.priority", WriterRole::Operator),
            ("spec.host_direct", WriterRole::ReconcileLoop),
            ("spec.docker_per_vessel", WriterRole::ReconcileLoop),
            ("spec.docker_per_vessel.host_ref", WriterRole::ReconcileLoop),
            ("spec.docker_per_vessel.image", WriterRole::Operator),
            ("spec.docker_per_vessel.pull_policy", WriterRole::Operator),
            ("spec.docker_per_vessel.memory_policy", WriterRole::Operator),
            ("spec.docker_per_vessel.agent_adapters", WriterRole::Operator),
            ("spec.docker_per_vessel.default_cwd", WriterRole::Operator),
            ("spec.docker_per_vessel.env", WriterRole::Operator),
            ("spec.docker_per_vessel.checkout", WriterRole::ReconcileLoop),
        ]
    );
    assert!(declared.iter().all(|(field, _)| !field.starts_with("status.")), "PlacementPolicy has no status fields");
}

#[tokio::test]
async fn observe_mode_preserves_operator_fields_and_surfaces_loop_violation() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let created = policies
        .create(&InputMeta::builder().name("host-direct-local".to_string()).build(), &host_direct("old", 42, "old-host"))
        .await
        .expect("create policy");

    let updated = policies
        .write_spec(
            &WriterIdentity::reconcile_loop(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &host_direct("new", 0, "new-host"),
        )
        .await
        .expect("observe-mode owned update");

    assert_eq!(updated.spec.priority, 42, "the stored operator field must be preserved");
    assert_eq!(updated.spec.pool, "new");
    assert_eq!(updated.spec.host_direct.expect("host-direct").host_ref, "new-host");

    let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics");
    assert_eq!(diagnostics.field_ownership_violations.len(), 1);
    let violation = &diagnostics.field_ownership_violations[0];
    assert_eq!(violation.writer.role, WriterRole::ReconcileLoop);
    assert_eq!(violation.field, "spec.priority");
    assert_eq!(violation.attempted_value, serde_json::json!(0));
    assert!(violation.rule.contains("Operator"));
}

#[tokio::test]
async fn operator_apply_preserves_loop_fields_while_updating_priority() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let created = policies
        .create(&InputMeta::builder().name("host-direct-local".to_string()).build(), &host_direct("owned-pool", 1, "owned-host"))
        .await
        .expect("create policy");

    let updated = policies
        .write_spec(
            &WriterIdentity::operator(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &host_direct("attempted-pool", 99, "attempted-host"),
        )
        .await
        .expect("operator write");

    assert_eq!(updated.spec.priority, 99);
    assert_eq!(updated.spec.pool, "owned-pool");
    assert_eq!(updated.spec.host_direct.expect("host-direct").host_ref, "owned-host");
}

// A full operator document may echo loop-owned host_ref while editing memory_policy.
// Echoing a protected field does not transfer its static owner; changing it is refused.
#[tokio::test]
async fn resource_apply_updates_memory_policy_with_unchanged_host_ref() {
    for backend in [
        ResourceBackend::InMemory(InMemoryBackend::default()),
        ResourceBackend::Sqlite(flotilla_resources::SqliteBackend::open_in_memory().expect("sqlite")),
    ] {
        let policies = backend.using::<PlacementPolicy>("flotilla");
        let original = docker("docker", 4, "feta", "image:old");
        policies.create(&InputMeta::builder().name("policy".to_string()).build(), &original).await.expect("create policy");
        let mut requested = original.clone();
        requested.docker_per_vessel.as_mut().expect("docker").memory_policy.host_memory_percent = 80;
        let document = |spec: &PlacementPolicySpec| {
            json!({
                "apiVersion": "flotilla.work/v1", "kind": "PlacementPolicy",
                "metadata": { "name": "policy" }, "spec": spec
            })
        };
        apply_resource_document(&backend, "flotilla", document(&requested)).await.expect("unchanged loop-owned fields must be accepted");
        assert_eq!(policies.get("policy").await.expect("stored policy").spec, requested);
        assert!(backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics").field_ownership_violations.is_empty());
        let mut forbidden = requested.clone();
        forbidden.docker_per_vessel.as_mut().expect("docker").host_ref = "other-host".into();
        let error = apply_resource_document(&backend, "flotilla", document(&forbidden))
            .await
            .expect_err("echoing host_ref must not transfer its owner");
        assert!(matches!(error, ResourceError::FieldOwnership { ref violations }
            if violations.len() == 1 && violations[0].field == "spec.docker_per_vessel.host_ref"
                && violations[0].rule.contains("ReconcileLoop")));
        assert_eq!(policies.get("policy").await.expect("stored policy after refusal").spec, requested);
    }
}

#[tokio::test]
async fn resource_apply_reports_rejected_field_and_owner() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    policies
        .create(&InputMeta::builder().name("policy".to_string()).build(), &docker("docker", 4, "udder", "image:old"))
        .await
        .expect("create policy");

    let error = apply_resource_document(
        &backend,
        "flotilla",
        json!({
            "apiVersion": "flotilla.work/v1",
            "kind": "PlacementPolicy",
            "metadata": { "name": "policy" },
            "spec": docker("docker", 4, "other-host", "image:new")
        }),
    )
    .await
    .expect_err("operator apply must report rejected ownership change");

    let message = error.to_string();
    assert!(message.contains("spec.docker_per_vessel.host_ref"), "{message}");
    assert!(message.contains("ReconcileLoop"), "{message}");
    let stored = policies.get("policy").await.expect("stored policy").spec.docker_per_vessel.expect("docker");
    assert_eq!(stored.host_ref, "udder");
    assert_eq!(stored.image, "image:old".into(), "a rejected apply must not partially change operator fields");
    assert_eq!(backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics").field_ownership_violations.len(), 1);
}

#[tokio::test]
async fn operator_cannot_clear_loop_owned_docker_strategy_and_violation_remains_visible() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let created = policies
        .create(&InputMeta::builder().name("docker-local".to_string()).build(), &docker("docker", 4, "local", "operator/image:latest"))
        .await
        .expect("create docker policy");

    let updated = policies
        .write_spec(
            &WriterIdentity::operator(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &host_direct("docker", 9, "local"),
        )
        .await
        .expect("observe-mode structural violation must not become Invalid");

    assert_eq!(updated.spec.priority, 9);
    assert!(updated.spec.host_direct.is_none());
    assert_eq!(updated.spec.docker_per_vessel.expect("preserved docker strategy").image, "operator/image:latest".into());
    let diagnostics = backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics");
    assert!(
        diagnostics.field_ownership_violations.iter().any(|violation| violation.field == "spec.docker_per_vessel"),
        "the structural violation must be recorded"
    );
}

#[tokio::test]
async fn loop_can_switch_from_docker_to_host_direct_without_descendant_ownership_conflicts() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let created = policies
        .create(&InputMeta::builder().name("switching".to_string()).build(), &docker("docker", 44, "local", "operator/image:latest"))
        .await
        .expect("create docker policy");

    let updated = policies
        .write_spec(
            &WriterIdentity::reconcile_loop(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &host_direct("direct", 0, "local"),
        )
        .await
        .expect("loop-owned strategy switch");

    assert_eq!(updated.spec.priority, 44);
    assert!(updated.spec.docker_per_vessel.is_none());
    assert_eq!(updated.spec.host_direct.expect("host-direct strategy").host_ref, "local");
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnforcedSpec {
    operator_value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnforcedResource;

impl Resource for EnforcedResource {
    type Spec = EnforcedSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "enforcedresources", kind: "EnforcedResource" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::None;
}

impl FieldOwnedResource for EnforcedResource {
    const FIELD_OWNERSHIP: &'static [FieldOwnership] = &[FieldOwnership::new("spec.operator_value", WriterRole::Operator)];
    const OWNERSHIP_ENFORCEMENT: OwnershipEnforcement = OwnershipEnforcement::Enforce;
}

#[tokio::test]
async fn enforce_mode_records_and_refuses_with_typed_error() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let resources = backend.using::<EnforcedResource>("flotilla");
    let created = resources
        .create(&InputMeta::builder().name("only".to_string()).build(), &EnforcedSpec { operator_value: "stored".to_string() })
        .await
        .expect("create");

    let error = resources
        .write_spec(
            &WriterIdentity::reconcile_loop(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &EnforcedSpec { operator_value: "attempted".to_string() },
        )
        .await
        .expect_err("enforcement must refuse");

    assert!(matches!(error, ResourceError::FieldOwnership { ref violations } if violations.len() == 1));
    assert!(error.is_stale_view(), "callers must classify ownership refusal as a stale-view requeue");
    assert_eq!(resources.get("only").await.expect("stored resource").spec.operator_value, "stored");
    assert_eq!(backend.diagnostics().await.expect("diagnostics").expect("embedded diagnostics").field_ownership_violations.len(), 1);
}
