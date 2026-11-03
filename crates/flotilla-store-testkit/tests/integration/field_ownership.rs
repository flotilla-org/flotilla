use flotilla_resources::{
    ApiPaths, FieldOwnedResource, FieldOwnership, InputMeta, NoStatusPatch, OwnershipEnforcement, ReplicationClass, Resource,
    ResourceError, WriterIdentity, WriterRole,
};
use flotilla_store::ResourceBackend;
use flotilla_store_memory::InMemoryBackend;
use serde::{Deserialize, Serialize};

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
    let backend = ResourceBackend::new(InMemoryBackend::default());
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
