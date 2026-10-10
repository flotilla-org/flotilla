use std::collections::BTreeMap;

use crate::*;

#[tokio::test]
async fn registered_defaults_apply_as_a_validated_definition() {
    // Intended: the manifest reconciler's dynamic application accepts the new
    // kind and rejects malformed refs before persisting desired state.
    let backend = crate::ResourceBackend::InMemory(crate::InMemoryBackend::default());
    let resolver = backend.definitions::<CrewDefaults>("flotilla");
    let meta = crate::InputMeta::builder().name("fleet".to_string()).build();
    assert!(resolver
        .apply(
            &meta,
            &CrewDefaultsSpec {
                project_ref: None,
                default_workflow_ref: None,
                roles: BTreeMap::new(),
                skills: BTreeMap::from([("*".into(), vec!["a/b@testing".into()])])
            }
        )
        .await
        .is_ok());
    assert!(resolver
        .apply(
            &meta,
            &CrewDefaultsSpec {
                project_ref: None,
                default_workflow_ref: None,
                roles: BTreeMap::new(),
                skills: BTreeMap::from([("*".into(), vec!["../bad".into()])])
            }
        )
        .await
        .is_err());
    let document = serde_json::json!({"apiVersion":"flotilla.work/v1", "kind":"CrewDefaults", "metadata":{"name":"fleet"}, "spec":{"skills":{"*": ["a/b@testing"]}}});
    crate::apply_manifest_resource_document(&backend, "flotilla", document).await.expect("manifest reconciler applies new kind");
    assert_eq!(resolver.get("fleet").await.expect("stored definition").spec.skills["*"], ["a/b@testing"]);
}
