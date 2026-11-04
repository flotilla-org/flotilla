use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use std::collections::BTreeSet;

use flotilla_resources::{FulfilmentCostClass, FulfilmentGrant, FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, InputMeta};
use flotilla_store::ResourceBackend;

async fn contract(backend: ResourceBackend) {
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let spec = FulfilmentKindSpec::builder()
        .host_ref("feta".to_string())
        .pool("cleat".to_string())
        .grants(BTreeSet::from([FulfilmentGrant::platform("linux".to_string()), FulfilmentGrant::network("scoped".to_string())]))
        .realisation(FulfilmentRealisation::DockerPerVessel { image: Some("crew:v1".into()) })
        .build();
    let created = kinds.create(&InputMeta::builder().name("docker-crew-image-feta".to_string()).build(), &spec).await.expect("create kind");
    assert_eq!(created.spec, spec);
    let fetched = kinds.get("docker-crew-image-feta").await.expect("get kind");
    assert_eq!(fetched.spec, spec);
    assert_eq!(kinds.list().await.expect("list kinds").items.len(), 1);
    kinds.delete("docker-crew-image-feta").await.expect("delete kind");
}

#[tokio::test]
async fn in_memory_contract() {
    contract(ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_contract() {
    let dir = tempfile::tempdir().expect("temp directory");
    let backend = SqliteBackend::open(dir.path().join("resources.db")).expect("sqlite backend");
    contract(ResourceBackend::new(backend)).await;
}

#[test]
fn cost_class_names_match_their_serialized_form() {
    // Adding a variant breaks this match; add it to `FulfilmentCostClass::ALL` too.
    let position = |class: FulfilmentCostClass| match class {
        FulfilmentCostClass::OwnedIdle => 0,
        FulfilmentCostClass::SubscriptionIncluded => 1,
        FulfilmentCostClass::Metered => 2,
    };
    for (index, class) in FulfilmentCostClass::ALL.into_iter().enumerate() {
        assert_eq!(position(class), index, "{class} is listed out of place");
        assert_eq!(serde_json::to_value(class).expect("serialize cost class"), serde_json::json!(class.as_str()));
        assert_eq!(serde_json::from_value::<FulfilmentCostClass>(serde_json::json!(class.as_str())).expect("decode cost class"), class);
        assert_eq!(class.to_string(), class.as_str());
    }
}
