use flotilla_store_memory::InMemoryBackend;
use flotilla_store_sqlite::SqliteBackend;
use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    image_identity, resolve_image, EnvironmentStatus, FulfilmentGrant, FulfilmentImage, ImageCatalogue, ImageLayer, ImageLayerSource,
    ImageLayerSpec, ImageLayerTier, ImageRequest, InputMeta, VesselStatus,
};
use flotilla_store::{apply_resource_document, ResourceBackend};
use rstest::rstest;

/// An installation layer: the shipped stdlib layers are code-backed and always listed, so a test
/// layer named like one shadows it rather than colliding with it.
fn layer(name: &str, tier: ImageLayerTier, provides: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0.0".into())
        .tier(tier)
        .source(ImageLayerSource::Installation)
        .maybe_from((tier == ImageLayerTier::Base).then(|| format!("ubuntu@sha256:{}", "a".repeat(64))))
        .fragment(format!("FROM ${{BASE}}\nRUN echo {name}\n"))
        .provides(provides.iter().map(|capability| capability.to_string()).collect())
        .probe("true".into())
        .build()
}

#[test]
fn previous_generation_decodes_but_new_writes_use_current_shape() {
    for (legacy, expected) in [
        (serde_json::json!({"kind":"platform","value":"linux"}), "platform:linux"),
        (serde_json::json!({"kind":"toolchain","value":"rust"}), "toolchain:rust"),
        (serde_json::json!({"kind":"network","value":"scoped"}), "network:scoped"),
        (serde_json::json!({"kind":"gui_session"}), "display:gui-session"),
        (serde_json::json!({"kind":"gpu"}), "hardware:gpu"),
        (serde_json::json!({"kind":"host_devices"}), "hardware:host-devices"),
        (serde_json::json!({"kind":"host_account_reach"}), "account:host"),
        (serde_json::json!({"kind":"container_runtime"}), "runtime:container"),
    ] {
        let grant: FulfilmentGrant = serde_json::from_value(legacy).expect("old grant");
        assert_eq!(serde_json::to_value(grant).expect("new grant"), expected);
    }
    let environment: EnvironmentStatus =
        serde_json::from_value(serde_json::json!({"phase":"Ready","image_digest":"sha256:old"})).expect("old environment");
    let vessel: VesselStatus =
        serde_json::from_value(serde_json::json!({"phase":"Ready","image_digest":"sha256:old"})).expect("old vessel");
    for value in [serde_json::to_value(environment).expect("environment"), serde_json::to_value(vessel).expect("vessel")] {
        assert_eq!(value["local_image_id"], "sha256:old");
        assert!(value.get("image_digest").is_none());
    }
    let image: FulfilmentImage = serde_json::from_value(serde_json::json!("old:tag")).expect("old image");
    assert_eq!(serde_json::to_value(image).expect("structured image")["image_ref"], "old:tag");
}

// Behaviour (ADR 0062): ImageLayer is a validated federated Definition in
// every store. Invalid definitions are refused on apply, a retired ADR 0053
// record still applies (previous-generation manifests during the roll) but is
// inert for resolution, and identity survives the stored round trip.
#[rstest]
#[case::memory(ResourceBackend::new(InMemoryBackend::default()))]
#[case::sqlite(ResourceBackend::new(SqliteBackend::open_in_memory().expect("sqlite")))]
#[tokio::test]
async fn layer_definition_contract(#[case] backend: ResourceBackend) {
    let layers = backend.definitions::<ImageLayer>("flotilla");
    let base = layer("base-ubuntu", ImageLayerTier::Base, &["os:ubuntu"]);
    let meta = InputMeta::builder().name(base.resource_name()).build();
    layers.apply(&meta, &base.clone().into()).await.expect("apply definition");
    let stored = layers.get(&base.resource_name()).await.expect("get definition").spec;
    assert_eq!(stored.current(), Some(&base));
    assert_eq!(image_identity(std::slice::from_ref(stored.current().expect("current"))), image_identity(std::slice::from_ref(&base)));

    let mut invalid = base.clone();
    invalid.fragment = "FROM ubuntu:latest\n".into();
    assert!(layers.apply(&meta, &invalid.into()).await.is_err(), "the fragment FROM contract is validated on apply");
    assert_eq!(layers.get(&base.resource_name()).await.expect("unchanged definition").spec.current(), Some(&base));

    apply_resource_document(
        &backend,
        "flotilla",
        serde_json::json!({
            "apiVersion": "flotilla.work/v1", "kind": "ImageLayer", "metadata": {"name": "base"},
            "spec": {"stage": "base", "parent": {"image": format!("debian@sha256:{}", "b".repeat(64))},
                     "repository": "https://example.test/images", "revision": "1".repeat(40), "fragment": "Dockerfile.base",
                     "provides": ["os:debian-family"], "input_stability": "pinned"}
        }),
    )
    .await
    .expect("retired record applies");
    // The shipped stdlib layers are always listed beside the stored ones; only the stored ones are under test.
    let all: Vec<_> = layers
        .list()
        .await
        .expect("list")
        .into_iter()
        .filter(|layer| !flotilla_resources::builtins::is_builtin("ImageLayer", &layer.metadata.name))
        .collect();
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|layer| layer.spec.is_retired()));
    let catalogue = ImageCatalogue::from_specs(all.iter().map(|layer| &layer.spec));
    assert_eq!(catalogue.layers(), std::slice::from_ref(&base), "retired records never enter resolution");
}

// Behaviour: admission resolves against the replica-inclusive catalogue, so a
// layer authored on another root is selectable here with the same identity.
#[tokio::test]
async fn replicated_layers_resolve_with_the_authored_identity() {
    let local = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
    let remote = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("root-b"));
    let authored = [
        layer("base-ubuntu-24.04", ImageLayerTier::Base, &["os:ubuntu"]),
        layer("crew-utilities", ImageLayerTier::AgentRuntime, &["crew:utilities"]),
        layer("display-x11", ImageLayerTier::Shared, &["display:x11"]),
    ];
    for spec in &authored {
        remote
            .definitions::<ImageLayer>("flotilla")
            .apply(&InputMeta::builder().name(spec.resource_name()).build(), &spec.clone().into())
            .await
            .expect("remote layer");
    }
    local
        .replica_writer::<ImageLayer>(NodeId::new("root-b"), "flotilla")
        .replace(&remote.using::<ImageLayer>("flotilla").list().await.expect("list"), Utc::now())
        .await
        .expect("replicate");
    let merged = local.definitions::<ImageLayer>("flotilla").list().await.expect("merged definitions");
    let catalogue = ImageCatalogue::from_specs(merged.iter().map(|layer| &layer.spec));
    let request = ImageRequest::builder().project("flotilla".into()).needs(BTreeSet::from(["display:x11".to_string()])).build();
    let resolved = resolve_image(&catalogue, &request).expect("resolve from replicas");
    assert_eq!(
        resolved.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(),
        ["base-ubuntu-24.04", "display-x11", "crew-utilities"]
    );
    let expected = [authored[0].clone(), authored[2].clone(), authored[1].clone()];
    assert_eq!(resolved.identity, image_identity(&expected));
}

// Behaviour: namespaced host privilege needs retain their host-sensitive typed
// meaning; serializing legacy spellings writes the current namespaced vocabulary.
#[test]
fn namespaced_host_needs_preserve_privilege_checks() {
    use flotilla_resources::CapabilityNeed;
    for (old, current, expected) in [
        ("gui_session", "display:gui-session", CapabilityNeed::GuiSession),
        ("gpu", "hardware:gpu", CapabilityNeed::Gpu),
        ("host_devices", "hardware:host-devices", CapabilityNeed::HostDevices),
        ("host_account_reach", "account:host", CapabilityNeed::HostAccountReach),
        ("container_runtime", "runtime:container", CapabilityNeed::ContainerRuntime),
    ] {
        assert_eq!(old.parse::<CapabilityNeed>().expect("old need"), expected);
        assert_eq!(current.parse::<CapabilityNeed>().expect("current need"), expected);
        assert_eq!(serde_json::to_value(expected).expect("new need"), current);
    }
    assert!(!"hardware:device".parse::<CapabilityNeed>().expect("open hardware need").is_image_need());
}

// Behaviour: exact and unversioned open harness needs use probed versions for
// host-direct placements, consistently with the implicit minimum-version need.
#[test]
fn open_harness_needs_use_host_probes() {
    use flotilla_resources::{CapabilityNeed, FulfilmentFacts, HarnessFacts};
    let facts = FulfilmentFacts {
        harnesses: BTreeMap::from([("codex".into(), HarnessFacts { version: "0.160.0".into(), ..Default::default() })]),
        ..Default::default()
    };
    for need in ["harness:codex", "harness:codex@0.160.0"] {
        let need = need.parse::<CapabilityNeed>().expect("open need");
        assert!(need.covered_by(&BTreeSet::new(), Some(&facts)));
        assert!(!need.covered_by(&BTreeSet::new(), None));
    }
    assert!(!"harness:codex@0.159.0".parse::<CapabilityNeed>().expect("exact need").covered_by(&BTreeSet::new(), Some(&facts)));
}
