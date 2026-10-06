use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    compose_image, DockerImageSource, EnvironmentStatus, FrozenImageLayers, FulfilmentGrant, FulfilmentImage, ImageInputStability,
    ImageLayer, ImageLayerParent, ImageLayerSelection, ImageLayerSpec, ImageLayerStage, InMemoryBackend, InputMeta, PlacedImageIdentity,
    ResolvedImageInputs, ResourceBackend, SqliteBackend, VesselStatus,
};
use hegel::generators as gs;

fn layer(stage: ImageLayerStage, provides: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .stage(stage)
        .parent(match stage {
            ImageLayerStage::Base => ImageLayerParent::Image(format!("debian@sha256:{}", "a".repeat(64))),
            ImageLayerStage::Utilities => ImageLayerParent::Layer("base".into()),
            _ => ImageLayerParent::Rebasable,
        })
        .repository("https://example.test/images".into())
        .revision("1".repeat(40))
        .fragment("Dockerfile".into())
        .provides(provides.iter().map(|value| (*value).into()).collect())
        .build()
}

fn selection() -> ImageLayerSelection {
    ImageLayerSelection::builder().base("base".into()).utilities("utils".into()).harness("harness".into()).project("project".into()).build()
}

fn catalogue() -> BTreeMap<String, ImageLayerSpec> {
    BTreeMap::from([
        ("base".into(), layer(ImageLayerStage::Base, &["os:debian-family", "toolchain:rust@1.94.1"])),
        ("utils".into(), layer(ImageLayerStage::Utilities, &["utility:git"])),
        ("harness".into(), layer(ImageLayerStage::Harness, &["harness:codex@0.160.0"])),
        ("project".into(), layer(ImageLayerStage::Project, &["project:flotilla"])),
        ("a-display".into(), layer(ImageLayerStage::Capability, &["display:headless-x11"])),
        ("z-browser".into(), layer(ImageLayerStage::Capability, &["browser:chromium"])),
        ("irrelevant".into(), layer(ImageLayerStage::Capability, &["utility:unrelated"])),
    ])
}

// Behaviour: each need-set gets exactly its minimal cover in canonical order,
// regardless of catalogue insertion order, duplicate needs or host-local paths.
#[hegel::test]
fn canonical_minimal_cover(tc: hegel::TestCase) {
    // All combinations of two optional capabilities, duplicate/empty needs,
    // and permutations of input insertion order.
    let display = tc.draw(gs::booleans());
    let browser = tc.draw(gs::booleans());
    let reverse = tc.draw(gs::booleans());
    let mut entries = catalogue().into_iter().collect::<Vec<_>>();
    if reverse {
        entries.reverse();
    }
    let mut needs = BTreeSet::from(["harness:codex>=0.160".into(), "toolchain:rust>=1.94".into()]);
    if display {
        needs.insert("display:headless-x11".into());
        needs.insert("display:headless-x11".into());
    }
    if browser {
        needs.insert("browser:chromium".into());
    }
    let result = compose_image(&selection(), &needs, &entries.into_iter().collect(), &mut FrozenImageLayers::default()).expect("compose");
    let mut expected = vec!["base", "utils"];
    if display {
        expected.push("a-display");
    }
    if browser {
        expected.push("z-browser");
    }
    expected.extend(["harness", "project"]);
    assert_eq!(result.layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), expected);
    let empty = compose_image(&selection(), &BTreeSet::new(), &catalogue(), &mut FrozenImageLayers::default()).expect("empty needs");
    assert_eq!(empty.layers.len(), 4);
}

// Behaviour: a layer can provide multiple needs; admission minimises layer count
// rather than greedily selecting a layer per need, while honouring requires.
#[test]
fn minimum_cover_and_ordered_requires() {
    let mut catalogue = catalogue();
    let mut combined = layer(ImageLayerStage::Capability, &["display:headless-x11", "browser:chromium"]);
    combined.requires.insert("os:debian-family".into());
    catalogue.insert("combined".into(), combined);
    let needs = BTreeSet::from(["display:headless-x11".into(), "browser:chromium".into()]);
    let composition = compose_image(&selection(), &needs, &catalogue, &mut FrozenImageLayers::default()).expect("combined cover");
    assert_eq!(
        composition
            .layers
            .iter()
            .filter(|layer| layer.spec.stage == ImageLayerStage::Capability)
            .map(|layer| layer.name.as_str())
            .collect::<Vec<_>>(),
        ["combined"]
    );
    catalogue.get_mut("combined").expect("combined").requires.insert("browser:chromium".into());
    let composition = compose_image(&selection(), &needs, &catalogue, &mut FrozenImageLayers::default()).expect("individual cover");
    assert_eq!(composition.layers.iter().filter(|layer| layer.spec.stage == ImageLayerStage::Capability).count(), 2);
    catalogue.get_mut("a-display").expect("display").requires.insert("browser:chromium".into());
    let mut frozen = FrozenImageLayers::default();
    let error = compose_image(&selection(), &needs, &catalogue, &mut frozen).expect_err("later layer cannot satisfy earlier requires");
    assert!(error.contains("display:headless-x11"));
    assert!(frozen.layers.is_empty());
}

// Behaviour: unsatisfiable versions and missing capabilities name the need and
// leave freezing unchanged. Fixed parent mistakes refuse instead of restacking.
#[test]
fn refusal_is_named_and_atomic() {
    for need in ["harness:codex>=0.161", "display:missing", "toolchain:rust@1.93.0"] {
        let mut frozen = FrozenImageLayers::default();
        let error = compose_image(&selection(), &BTreeSet::from([need.into()]), &catalogue(), &mut frozen).expect_err("refuse");
        assert!(error.contains(need));
        assert_eq!(frozen, FrozenImageLayers::default());
    }
    let mut catalogue = catalogue();
    catalogue.get_mut("utils").expect("utilities").parent = ImageLayerParent::Layer("other-base".into());
    assert!(compose_image(&selection(), &BTreeSet::new(), &catalogue, &mut FrozenImageLayers::default()).is_err());
}

// Behaviour: ordinary convoy inputs never change, including after any sequence
// of catalogue updates and dynamic capability additions; failed additions are atomic.
#[hegel::test]
fn staged_freezing_survives_catalogue_updates(tc: hegel::TestCase) {
    // Interleavings of harness/base edits, capabilities added at different steps,
    // and attempts to add unsatisfiable needs.
    let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let operations = (0..steps).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(3))).collect::<Vec<_>>();
    let mut catalogue = catalogue();
    let mut frozen = FrozenImageLayers::default();
    let first = compose_image(&selection(), &BTreeSet::new(), &catalogue, &mut frozen).expect("admission");
    let initial = frozen.clone();
    for (step, op) in operations.into_iter().enumerate() {
        let mut needs = BTreeSet::new();
        match op {
            0 => {
                catalogue.get_mut("base").expect("base").revision = format!("{step:040x}");
                catalogue.get_mut("harness").expect("harness").revision = format!("{step:040x}");
            }
            1 => {
                needs.insert("display:headless-x11".into());
            }
            2 => {
                needs.insert("browser:chromium".into());
            }
            _ => {
                needs.insert("display:missing".into());
            }
        }
        let before = frozen.clone();
        let result = compose_image(&selection(), &needs, &catalogue, &mut frozen);
        if op == 3 {
            assert!(result.is_err());
            assert_eq!(frozen, before);
        } else {
            let result = result.expect("dynamic composition");
            assert_eq!(result.layers[0], first.layers[0]);
            for (name, spec) in &initial.layers {
                assert_eq!(frozen.layers.get(name), Some(spec));
            }
            for (name, spec) in &before.layers {
                assert_eq!(frozen.layers.get(name), Some(spec));
            }
        }
    }
    // Even a new cascade cannot replace an admitted base/harness selection.
    let changed = ImageLayerSelection::builder().base("missing-new-base".into()).build();
    assert_eq!(compose_image(&changed, &BTreeSet::new(), &catalogue, &mut frozen).expect("frozen cascade").selection, selection());
}

// Behaviour: recipe keys depend on content, architecture, normalised pins/args
// and parent identity; they exclude host identity, paths and revision metadata.
#[hegel::test]
fn merkle_recipe_is_host_independent_and_sensitive(tc: hegel::TestCase) {
    // Both supported architectures, arbitrary independent hosts/repository paths,
    // and changing each input category, crossing zero/one argument boundaries.
    let architecture = if tc.draw(gs::booleans()) { "amd64" } else { "arm64" };
    let args_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
    let mut inputs = ResolvedImageInputs::builder()
        .parent_digest(format!("sha256:{}", "a".repeat(64)))
        .content_hashes(vec![format!("sha256:{}", "b".repeat(64)), format!("sha256:{}", "d".repeat(64))])
        .architecture(architecture.into())
        .stability(ImageInputStability::Pinned)
        .args((0..args_count).map(|n| (format!("ARG{n}"), n.to_string())).collect())
        .build();
    let key = inputs.recipe_key().expect("pinned key");
    let mut other_host = inputs.clone();
    other_host.args = inputs.args.iter().rev().map(|(key, value)| (key.clone(), value.clone())).collect();
    other_host.content_hashes.reverse();
    assert_eq!(key, other_host.recipe_key().expect("other host"));
    let mut changed = inputs.clone();
    match tc.draw(gs::integers::<usize>().min_value(0).max_value(3)) {
        0 => changed.content_hashes[0] = format!("sha256:{}", "e".repeat(64)),
        1 => changed.architecture = if architecture == "amd64" { "arm64" } else { "amd64" }.into(),
        2 => {
            changed.args.insert("EXTRA".into(), "changed".into());
        }
        _ => {
            changed.pins.insert("codex".into(), "0.161.0".into());
        }
    }
    assert_ne!(key, changed.recipe_key().expect("changed own input"));
    inputs.parent_digest = format!("sha256:{}", "c".repeat(64));
    let changed_parent = inputs.recipe_key().expect("new parent");
    assert_ne!(key, changed_parent);
    inputs.parent_digest = key.clone();
    let child = inputs.recipe_key().expect("child");
    inputs.parent_digest = changed_parent;
    assert_ne!(child, inputs.recipe_key().expect("changed chain"));
    inputs.stability = ImageInputStability::Unpinned;
    assert!(inputs.recipe_key().is_err());
}

// Behaviour: concrete image identity binds at placement and cannot change on a
// relaunch; it is separate from frozen revisions and preserves local-only builds.
#[test]
fn placement_identity_binds_once() {
    let mut composition = compose_image(&selection(), &BTreeSet::new(), &catalogue(), &mut FrozenImageLayers::default()).expect("compose");
    assert!(composition.identity.is_none());
    assert!(composition.bind(PlacedImageIdentity { local_image_id: String::new(), registry_digest: None }).is_err());
    let identity = PlacedImageIdentity { local_image_id: "sha256:local".into(), registry_digest: None };
    composition.bind(identity.clone()).expect("bind");
    composition.bind(identity).expect("idempotent");
    assert!(composition.bind(PlacedImageIdentity { local_image_id: "sha256:other".into(), registry_digest: None }).is_err());
    assert_eq!(
        serde_json::from_value::<DockerImageSource>(
            serde_json::to_value(DockerImageSource::Composition { composition: Box::new(composition.clone()) }).expect("encode")
        )
        .expect("decode"),
        DockerImageSource::Composition { composition: Box::new(composition) }
    );
}

// Behaviour: previous-generation grants, local IDs and host image strings decode;
// new writes use only namespaced grants and honest structured identity names.
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

// Behaviour: ImageLayer is a validated federated Definition through both stores,
// rather than a home-bound runtime record or a composer-only data structure.
async fn definition_contract(backend: ResourceBackend) {
    let layers = backend.definitions::<ImageLayer>("flotilla");
    let meta = InputMeta::builder().name("base".into()).build();
    let spec = layer(ImageLayerStage::Base, &["os:debian-family"]);
    layers.apply(&meta, &spec).await.expect("apply definition");
    assert_eq!(layers.get("base").await.expect("get definition").spec, spec);
    let mut invalid = spec.clone();
    invalid.revision = "main".into();
    assert!(layers.apply(&meta, &invalid).await.is_err());
    assert_eq!(layers.get("base").await.expect("unchanged definition").spec, spec);
}

#[tokio::test]
async fn layer_definition_in_memory() {
    definition_contract(ResourceBackend::InMemory(InMemoryBackend::default())).await;
}

#[tokio::test]
async fn layer_definition_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    definition_contract(ResourceBackend::Sqlite(SqliteBackend::open(dir.path().join("resources.db")).expect("sqlite"))).await;
}

// Behaviour: a changed layer invalidates that stage and every stage above it,
// retaining keys below it; mixed architectures and missing stage inputs refuse.
#[hegel::test]
fn composition_recipe_chain_preserves_only_unaffected_lower_layers(tc: hegel::TestCase) {
    let composition = compose_image(&selection(), &BTreeSet::new(), &catalogue(), &mut FrozenImageLayers::default()).expect("compose");
    // Every stage can change, including the root and the top project boundary.
    let changed = tc.draw(gs::integers::<usize>().min_value(0).max_value(composition.layers.len() - 1));
    let mut inputs = composition
        .layers
        .iter()
        .map(|_| {
            ResolvedImageInputs::builder()
                .parent_digest(format!("sha256:{}", "a".repeat(64)))
                .content_hashes(vec![format!("sha256:{}", "b".repeat(64))])
                .architecture("amd64".into())
                .stability(ImageInputStability::Pinned)
                .build()
        })
        .collect::<Vec<_>>();
    let keys = composition.recipe_keys(&inputs).expect("chain");
    inputs[changed].content_hashes[0] = format!("sha256:{}", "c".repeat(64));
    let updated = composition.recipe_keys(&inputs).expect("updated chain");
    assert_eq!(&keys[..changed], &updated[..changed]);
    for index in changed..keys.len() {
        assert_ne!(keys[index], updated[index]);
    }
    inputs[1].architecture = "arm64".into();
    assert!(composition.recipe_keys(&inputs).is_err());
    inputs.pop();
    assert!(composition.recipe_keys(&inputs).is_err());
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

// Behaviour: registry-less installations can use a pinned local Docker image ID
// as a layer root; tags and malformed digests never claim pinned root identity.
#[test]
fn root_accepts_local_ids_and_registry_digests() {
    let mut base = layer(ImageLayerStage::Base, &["os:debian-family"]);
    for parent in [format!("sha256:{}", "a".repeat(64)), format!("arbitrary.example/image@sha256:{}", "b".repeat(64))] {
        base.parent = ImageLayerParent::Image(parent);
        assert!(base.validate().is_ok());
    }
    for parent in ["image:latest", "image@sha256:short", "sha256:"] {
        base.parent = ImageLayerParent::Image(parent.into());
        assert!(base.validate().is_err());
    }
}

// Behaviour: catalogue size alone cannot cause combinatorial admission work.
// Keep transitive prerequisites, prune unrelated layers and impossible order.
#[test]
fn large_catalogue_prunes_irrelevant_and_impossible_layers() {
    let mut catalogue = catalogue();
    for index in 0..100 {
        catalogue.insert(format!("unused-{index:03}"), layer(ImageLayerStage::Capability, &[&format!("extra:item-{index}")]));
    }
    let mut prerequisite = layer(ImageLayerStage::Capability, &["setup:display"]);
    prerequisite.requires.insert("os:debian-family".into());
    catalogue.insert("a-setup".into(), prerequisite);
    catalogue.get_mut("a-display").unwrap().requires.insert("setup:display".into());
    // Rename setup to precede the display in canonical order.
    let setup = catalogue.remove("a-setup").unwrap();
    catalogue.insert("a-before-display".into(), setup);
    let needs = BTreeSet::from(["display:headless-x11".into()]);
    let composed = compose_image(&selection(), &needs, &catalogue, &mut FrozenImageLayers::default()).expect("pruned cover");
    assert_eq!(
        composed
            .layers
            .iter()
            .filter(|layer| layer.spec.stage == ImageLayerStage::Capability)
            .map(|layer| layer.name.as_str())
            .collect::<Vec<_>>(),
        ["a-before-display", "a-display"]
    );
    catalogue.get_mut("a-before-display").unwrap().requires.insert("absent:prerequisite".into());
    let mut frozen = FrozenImageLayers::default();
    assert!(compose_image(&selection(), &needs, &catalogue, &mut frozen).unwrap_err().contains("display:headless-x11"));
    assert!(frozen.layers.is_empty());
}

#[test]
fn difficult_cover_refuses_at_search_limit_without_freezing() {
    let mut catalogue = catalogue();
    for index in 0..24 {
        catalogue.insert(format!("candidate-{index:02}"), layer(ImageLayerStage::Capability, &["goal:all", &format!("part:{index}")]));
        catalogue.get_mut("harness").unwrap().requires.insert(format!("part:{index}"));
    }
    let mut frozen = FrozenImageLayers::default();
    let error = compose_image(&selection(), &BTreeSet::from(["goal:all".into()]), &catalogue, &mut frozen).unwrap_err();
    assert!(error.contains("search limit"), "{error}");
    assert!(error.contains("goal:all"));
    assert!(frozen.layers.is_empty());
}

#[test]
fn digest_case_and_adoption_fields_are_strict() {
    let mut root = layer(ImageLayerStage::Base, &[]);
    root.parent = ImageLayerParent::Image(format!("sha256:{}", "A".repeat(64)));
    assert!(root.validate().is_err());
    let inputs = ResolvedImageInputs::builder()
        .parent_digest(format!("sha256:{}", "A".repeat(64)))
        .content_hashes(vec![format!("sha256:{}", "b".repeat(64))])
        .architecture("amd64".into())
        .stability(ImageInputStability::Pinned)
        .build();
    assert!(inputs.recipe_key().is_err());
    let mut inputs = inputs;
    inputs.parent_digest = inputs.parent_digest.to_lowercase();
    assert!(inputs.recipe_key().is_ok());
    inputs.content_hashes[0] = inputs.content_hashes[0].to_uppercase();
    assert!(inputs.recipe_key().is_err());
    assert!(serde_json::from_value::<flotilla_resources::ImageInputAdoption>(
        serde_json::json!({"policy":"auto","constraint":"1.*","typo":true})
    )
    .is_err());
}

#[test]
fn relevant_layer_limit_has_an_atomic_boundary() {
    for count in [64, 65] {
        let mut catalogue = catalogue();
        for index in 0..count {
            catalogue.insert(format!("cover-{index:02}"), layer(ImageLayerStage::Capability, &["goal:one"]));
        }
        let mut frozen = FrozenImageLayers::default();
        let result = compose_image(&selection(), &BTreeSet::from(["goal:one".into()]), &catalogue, &mut frozen);
        if count == 64 {
            let composition = result.expect("64 relevant layers remain supported");
            assert_eq!(composition.layers.iter().filter(|layer| layer.spec.stage == ImageLayerStage::Capability).count(), 1);
        } else {
            let error = result.expect_err("65 relevant layers exceed the supported limit");
            assert!(error.contains("65 relevant capability layers"), "{error}");
            assert!(error.contains("goal:one"));
            assert!(frozen.layers.is_empty());
        }
    }
}
