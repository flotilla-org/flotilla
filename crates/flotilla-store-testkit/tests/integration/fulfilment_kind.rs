use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use std::collections::BTreeSet;

use flotilla_resources::{
    DockerCheckoutStrategy, DockerImagePullPolicy, DockerPerVesselPlacementPolicySpec, FulfilmentCostClass, FulfilmentGrant,
    FulfilmentKind, FulfilmentKindSpec, FulfilmentRealisation, HostDirectPlacementPolicyCheckout, HostDirectPlacementPolicySpec, InputMeta,
    PlacementPolicySpec,
};
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

// Behaviour (#3824 layer 4, ADR 0058): a placement realises with its host's provider record of its
// realisation, once one is declared; a host without one keeps the settings its policy carries.
async fn provider_settings_contract(backend: ResourceBackend) {
    use flotilla_resources::environment_provider::{EnvironmentProvider, EnvironmentProviderSpec, ProviderSettings};
    let policy: PlacementPolicySpec = serde_json::from_value(serde_json::json!({"pool": "cleat", "docker_per_vessel": {
        "host_ref": "feta", "memory_policy": {"host_memory_percent": 50, "expected_concurrent_crews": 4, "swap_bytes": 0},
        "default_cwd": "/workspace", "checkout": {"worktree_on_host_and_mount": {"mount_path": "/workspace"}}}}))
    .expect("policy");
    let carried = flotilla_store::provider_settings(&backend, "flotilla", &policy).await.expect("no record yet");
    assert_eq!(carried, EnvironmentProviderSpec::from_policy(&policy).expect("policy settings"));

    let mut declared = carried.clone();
    if let ProviderSettings::DockerPerVessel { memory_policy, checkout, .. } = &mut declared.settings {
        memory_policy.host_memory_percent = 30;
        *checkout = DockerCheckoutStrategy::FreshCloneInContainer { clone_path: "/w".into() };
    }
    let providers = backend.using::<EnvironmentProvider>("flotilla");
    providers.create(&InputMeta::builder().name("feta.docker_per_vessel".into()).build(), &declared).await.expect("declare");
    assert_eq!(flotilla_store::provider_settings(&backend, "flotilla", &policy).await.expect("record"), declared);

    // A host-direct placement on the same host reads its own realisation's settings, not Docker's.
    let host_direct: PlacementPolicySpec =
        serde_json::from_value(serde_json::json!({"pool": "local", "host_direct": {"host_ref": "feta", "checkout": "worktree"}}))
            .expect("policy");
    let settings = flotilla_store::provider_settings(&backend, "flotilla", &host_direct).await.expect("host-direct");
    assert_eq!(settings.settings, ProviderSettings::HostDirect { host_checkout: HostDirectPlacementPolicyCheckout::Worktree });

    // A record whose spec is for another host than its name says is refused, as the import refuses it.
    let mut misnamed = EnvironmentProviderSpec::from_policy(&host_direct).expect("settings");
    misnamed.host_ref = "kiwi".into();
    providers.create(&InputMeta::builder().name("feta.host_direct".into()).build(), &misnamed).await.expect("misnamed record");
    let error = flotilla_store::provider_settings(&backend, "flotilla", &host_direct).await.expect_err("misnamed");
    assert!(error.contains("are for `kiwi.host_direct`, not their name"), "{error}");
}

#[tokio::test]
async fn in_memory_contract() {
    contract(ResourceBackend::new(InMemoryBackend::default())).await;
    provider_settings_contract(ResourceBackend::new(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn sqlite_contract() {
    let dir = tempfile::tempdir().expect("temp directory");
    let backend = SqliteBackend::open(dir.path().join("resources.db")).expect("sqlite backend");
    contract(ResourceBackend::new(backend.clone())).await;
    provider_settings_contract(ResourceBackend::new(backend)).await;
}

#[test]
fn live_policy_set_migrates_to_kinds() {
    let docker = PlacementPolicySpec::builder()
        .pool("cleat".to_string())
        .docker_per_vessel(DockerPerVesselPlacementPolicySpec {
            legacy_image_baseline_ref: None,
            memory_policy: Default::default(),
            host_ref: "feta".to_string(),
            image: Some("crew:v1".into()),
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
        assert_eq!(kind.realisation, FulfilmentRealisation::DockerPerVessel { image: Some("crew:v1".into()) });
        assert!(kind.grants.contains(&FulfilmentGrant::platform("linux".to_string())));
        assert!(kind.grants.contains(&FulfilmentGrant::network("scoped".to_string())));
    }
    let kind = FulfilmentKindSpec::from_policy(&direct, "macos").expect("host-direct-kiwi");
    assert_eq!(kind.realisation, FulfilmentRealisation::HostDirect);
    assert!(kind.grants.contains(&FulfilmentGrant::host_account_reach()));
    assert!(kind.grants.contains(&FulfilmentGrant::platform("macos".to_string())));
    assert!(kind.grants.contains(&FulfilmentGrant::gui_session()));
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
