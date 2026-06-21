mod common;

use common::{convoy_object, convoy_spec, convoy_status, timestamp};
use flotilla_resources::{Convoy, ConvoyPhase, K8sResourceObject, ResourceError, ResourceObject};

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
