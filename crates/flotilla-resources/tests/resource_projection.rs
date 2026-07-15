mod common;

use std::collections::BTreeMap;

use common::{convoy_object, convoy_spec, convoy_status, timestamp};
use flotilla_resources::{
    Checkout, CheckoutSpec, CheckoutWorktreeSpec, Convoy, ConvoyPhase, ConvoyRepositorySpec, InputMeta, K8sResourceObject,
    LifecycleAuthority, ObservedCheckoutSpec, RepositoryKey, ResourceError, ResourceObject, AUTHORITY_LABEL,
};

#[test]
fn resource_object_projects_to_k8s_object_shape() {
    let object = convoy_object("alpha", convoy_spec("review"), Some(convoy_status(ConvoyPhase::Active)));

    let projected = object.to_k8s_object();
    let json = serde_json::to_value(&projected).expect("projection should serialize");

    assert_eq!(json["apiVersion"], "flotilla.work/v1");
    assert_eq!(json["kind"], "Convoy");
    assert_eq!(json["metadata"]["name"], "alpha");
    assert_eq!(json["metadata"]["namespace"], "flotilla");
    assert_eq!(json["metadata"]["resourceVersion"], "7");
    assert_eq!(json["metadata"]["creationTimestamp"], "1970-01-01T00:00:01Z");
    assert_eq!(json["spec"]["workflow_ref"], "review");
    assert_eq!(json["status"]["phase"], "Active");
}

#[test]
fn k8s_object_projection_roundtrips_to_typed_resource_object() {
    let object = convoy_object("alpha", convoy_spec("review"), Some(convoy_status(ConvoyPhase::Active)));

    let roundtripped = ResourceObject::<Convoy>::from_k8s_object(object.to_k8s_object()).expect("projection should roundtrip");

    assert_eq!(roundtripped.metadata.name, "alpha");
    assert_eq!(roundtripped.metadata.namespace, "flotilla");
    assert_eq!(roundtripped.metadata.resource_version, "7");
    assert_eq!(roundtripped.metadata.creation_timestamp, timestamp(1));
    assert_eq!(roundtripped.spec.workflow_ref, "review");
    assert_eq!(roundtripped.status.expect("status").phase, ConvoyPhase::Active);
}

#[test]
fn convoy_repository_snapshot_roundtrips_every_repository_field() {
    let mut spec = convoy_spec("review");
    let repo_ref = RepositoryKey("repo-flotilla".to_string());
    spec.repositories = vec![ConvoyRepositorySpec {
        url: "https://github.com/flotilla-org/flotilla".to_string(),
        repo_ref: repo_ref.clone(),
        base_ref: "main".to_string(),
        workspace_slug: "flotilla".to_string(),
        subpaths: vec!["crates/core".to_string(), "crates/tui".to_string()],
    }];
    spec.adopted_checkout_refs = BTreeMap::from([(repo_ref, "checkout-existing".to_string())]);
    let object = convoy_object("alpha", spec.clone(), None);

    let roundtripped = ResourceObject::<Convoy>::from_k8s_object(object.to_k8s_object()).expect("projection should roundtrip");

    assert_eq!(roundtripped.spec, spec);
}

#[test]
fn k8s_object_projection_rejects_wrong_resource_identity() {
    let object = convoy_object("alpha", convoy_spec("review"), None);
    let mut projected = object.to_k8s_object();
    projected.kind = "WorkflowTemplate".to_string();

    let err = ResourceObject::<Convoy>::from_k8s_object(projected).expect_err("wrong kind should fail");

    match err {
        ResourceError::Other { message } => assert!(message.contains("unexpected kind")),
        other => panic!("expected decode error, got {other}"),
    }
}

#[test]
fn k8s_object_projection_deserializes_kubernetes_casing() {
    let json = serde_json::json!({
        "apiVersion": "flotilla.work/v1",
        "kind": "Convoy",
        "metadata": {
            "name": "alpha",
            "namespace": "flotilla",
            "resourceVersion": "9",
            "labels": { "app": "flotilla" },
            "annotations": { "note": "test" },
            "creationTimestamp": "2026-04-13T12:00:00Z"
        },
        "spec": {
            "workflow_ref": "review",
            "inputs": {},
            "placement_policy": "laptop-docker"
        },
        "status": { "phase": "Pending" }
    });

    let projected: K8sResourceObject<Convoy> = serde_json::from_value(json).expect("k8s object should deserialize");
    let object = ResourceObject::<Convoy>::from_k8s_object(projected).expect("k8s object should map to typed object");

    assert_eq!(object.metadata.resource_version, "9");
    assert_eq!(object.spec.workflow_ref, "review");
    assert_eq!(object.status.expect("status").phase, ConvoyPhase::Pending);
}

#[test]
fn lifecycle_authority_is_stored_as_reserved_label_on_input_metadata() {
    let mut meta = InputMeta::builder().name("alpha".to_string()).build();

    meta.set_lifecycle_authority(LifecycleAuthority::Observed);

    assert_eq!(meta.labels.get(AUTHORITY_LABEL).map(String::as_str), Some("observed"));
    assert_eq!(meta.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Observed));
}

#[test]
fn lifecycle_authority_roundtrips_through_k8s_projection_labels() {
    let mut object = convoy_object("alpha", convoy_spec("review"), None);
    object.metadata.set_lifecycle_authority(LifecycleAuthority::Managed);

    let roundtripped = ResourceObject::<Convoy>::from_k8s_object(object.to_k8s_object()).expect("projection should roundtrip");

    assert_eq!(roundtripped.metadata.labels.get(AUTHORITY_LABEL).map(String::as_str), Some("managed"));
    assert_eq!(roundtripped.metadata.lifecycle_authority().expect("authority label should parse"), Some(LifecycleAuthority::Managed));
}

#[test]
fn checkout_spec_worktree_variant_roundtrips_through_k8s_projection() {
    let object = ResourceObject::<Checkout> {
        metadata: common::object_meta("checkout-a", "flotilla", "3"),
        spec: CheckoutSpec::Worktree(CheckoutWorktreeSpec {
            repo_ref: flotilla_resources::RepositoryKey("project-flotilla".to_string()),
            env_ref: "env-a".to_string(),
            r#ref: "feature-a".to_string(),
            base_ref: None,
            target_path: "/worktrees/feature-a".to_string(),
            clone_ref: "clone-a".to_string(),
        }),
        status: None,
    };

    let json = serde_json::to_value(object.to_k8s_object()).expect("checkout projection should serialize");
    assert_eq!(json["spec"]["kind"], "worktree");
    assert_eq!(json["spec"]["env_ref"], "env-a");
    assert_eq!(json["spec"]["target_path"], "/worktrees/feature-a");
    assert_eq!(json["spec"]["clone_ref"], "clone-a");

    let roundtripped = ResourceObject::<Checkout>::from_k8s_object(serde_json::from_value(json).expect("deserialize checkout"))
        .expect("checkout projection should roundtrip");
    assert_eq!(roundtripped.spec, object.spec);
}

#[test]
fn checkout_spec_observed_variant_carries_only_observed_facts() {
    let object = ResourceObject::<Checkout> {
        metadata: common::object_meta("checkout-a", "flotilla", "3"),
        spec: CheckoutSpec::Observed(ObservedCheckoutSpec {
            r#ref: "main".to_string(),
            path: "/Users/dev/flotilla".to_string(),
            repo_ref: flotilla_resources::RepositoryKey("project-flotilla".to_string()),
            host_ref: "host-01".to_string(),
            is_main: true,
        }),
        status: None,
    };

    let json = serde_json::to_value(object.to_k8s_object()).expect("checkout projection should serialize");
    assert_eq!(json["spec"]["kind"], "observed");
    assert_eq!(json["spec"]["ref"], "main");
    assert_eq!(json["spec"]["path"], "/Users/dev/flotilla");
    assert_eq!(json["spec"]["repo_ref"], "project-flotilla");
    assert_eq!(json["spec"]["is_main"], true);
    assert!(json["spec"].get("env_ref").is_none(), "observed checkout should not carry env_ref");
    assert!(json["spec"].get("target_path").is_none(), "observed checkout should not carry target_path");

    let roundtripped = ResourceObject::<Checkout>::from_k8s_object(serde_json::from_value(json).expect("deserialize checkout"))
        .expect("checkout projection should roundtrip");
    assert_eq!(roundtripped.spec, object.spec);
}
