use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    ApiPaths, DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec, FieldOwnedResource, FieldOwnership,
    HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InMemoryBackend, InputMeta, NoStatusPatch, OwnershipEnforcement,
    PlacementPolicy, PlacementPolicySpec, ReplicationClass, Resource, ResourceBackend, ResourceError, WriterIdentity, WriterRole,
};
use serde::{Deserialize, Serialize};

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
            host_ref: host.to_string(),
            image: image.to_string(),
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
    assert_eq!(declared, vec![
        ("spec.pool", WriterRole::ReconcileLoop),
        ("spec.priority", WriterRole::Operator),
        ("spec.host_direct", WriterRole::ReconcileLoop),
        ("spec.docker_per_vessel", WriterRole::ReconcileLoop),
        ("spec.docker_per_vessel.host_ref", WriterRole::ReconcileLoop),
        ("spec.docker_per_vessel.image", WriterRole::Operator),
        ("spec.docker_per_vessel.pull_policy", WriterRole::Operator),
        ("spec.docker_per_vessel.agent_adapters", WriterRole::Operator),
        ("spec.docker_per_vessel.default_cwd", WriterRole::Operator),
        ("spec.docker_per_vessel.env", WriterRole::Operator),
        ("spec.docker_per_vessel.checkout", WriterRole::ReconcileLoop),
    ]);
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
    assert_eq!(updated.spec.docker_per_vessel.expect("preserved docker strategy").image, "operator/image:latest");
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

#[tokio::test]
async fn operator_priority_survives_unbounded_local_and_remote_registration_cycles() {
    let backend = ResourceBackend::InMemory(InMemoryBackend::default());
    let policies = backend.using::<PlacementPolicy>("flotilla");
    let created = policies
        .create(&InputMeta::builder().name("host-direct-shared".to_string()).build(), &host_direct("local", 0, "local"))
        .await
        .expect("create policy");
    let mut current = policies
        .write_spec(
            &WriterIdentity::operator(),
            &InputMeta::from(&created.metadata),
            &created.metadata.resource_version,
            &host_direct("local", 73, "local"),
        )
        .await
        .expect("operator applies priority");

    for cycle in 0..256 {
        let (pool, host) = if cycle % 2 == 0 { ("local", "local") } else { ("remote", "remote") };
        current = policies
            .write_spec(
                &WriterIdentity::reconcile_loop(),
                &InputMeta::from(&current.metadata),
                &current.metadata.resource_version,
                &host_direct(pool, 0, host),
            )
            .await
            .expect("registration cycle");
        assert_eq!(current.spec.priority, 73, "priority changed during registration cycle {cycle}");
    }
}
