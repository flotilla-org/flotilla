use std::collections::BTreeSet;

use flotilla_resources::{
    DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec, FulfilmentCostClass, FulfilmentGrant,
    FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec,
    InMemoryBackend, InputMeta, PlacementPolicySpec, ResourceBackend, SqliteBackend,
};

async fn contract(backend: ResourceBackend) {
    let kinds = backend.using::<FulfilmentKind>("flotilla");
    let spec = FulfilmentKindSpec::builder()
        .host_ref("feta".to_string())
        .pool("cleat".to_string())
        .grants(BTreeSet::from([FulfilmentGrant::Platform("linux".to_string()), FulfilmentGrant::Network("scoped".to_string())]))
        .realisation(FulfilmentRealisation::DockerPerVessel { image: "crew:v1".into() })
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
    contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_contract() {
    let dir = tempfile::tempdir().expect("temp directory");
    let backend = SqliteBackend::open(dir.path().join("resources.db")).expect("sqlite backend");
    contract(ResourceBackend::Sqlite(backend)).await;
}

#[test]
fn live_policy_set_migrates_to_kinds() {
    let docker = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            memory_policy: Default::default(),
            host_ref: "feta".to_string(),
            image: "crew:v1".into(),
            pull_policy: DockerImagePullPolicy::Always,
            agent_adapters: BTreeSet::new(),
            default_cwd: None,
            env: Default::default(),
            checkout: DockerCheckoutStrategy::WorktreeOnHostAndMount { mount_path: "/workspace".to_string() },
        })
        .build();
    let direct = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .host_direct(HostDirectPlacementPolicySpec { host_ref: "kiwi".to_string(), checkout: HostDirectPlacementPolicyCheckout::Worktree })
        .build();
    for name in ["docker-crew-image-feta", "docker-on-feta"] {
        let kind = FulfilmentKindSpec::from_policy(&docker, "macos").expect(name);
        assert_eq!(kind.realisation, FulfilmentRealisation::DockerPerVessel { image: "crew:v1".into() });
        assert!(kind.grants.contains(&FulfilmentGrant::Platform("linux".to_string())));
        assert!(kind.grants.contains(&FulfilmentGrant::Network("scoped".to_string())));
    }
    let kind = FulfilmentKindSpec::from_policy(&direct, "macos").expect("host-direct-kiwi");
    assert_eq!(kind.realisation, FulfilmentRealisation::HostDirect);
    assert!(kind.grants.contains(&FulfilmentGrant::HostAccountReach));
    assert!(kind.grants.contains(&FulfilmentGrant::Platform("macos".to_string())));
    assert!(kind.grants.contains(&FulfilmentGrant::GuiSession));
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
