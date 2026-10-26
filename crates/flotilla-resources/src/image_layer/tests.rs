use std::collections::{BTreeMap, BTreeSet};

use hegel::generators as gs;

use super::*;

pub(crate) const BASE_FROM: &str = "docker.io/library/ubuntu@sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// A valid stdlib definition of `tier`.
pub(crate) fn layer(name: &str, version: &str, tier: ImageLayerTier, provides: &[&str], requires: &[&str]) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version(version.into())
        .tier(tier)
        .source(if tier == ImageLayerTier::Project { ImageLayerSource::Project } else { ImageLayerSource::Stdlib })
        .maybe_project((tier == ImageLayerTier::Project).then(|| "flotilla".to_string()))
        .maybe_from((tier == ImageLayerTier::Base).then(|| BASE_FROM.to_string()))
        .fragment(format!("FROM ${{BASE}}\nRUN echo {name}\n"))
        .provides(provides.iter().map(|value| value.to_string()).collect())
        .requires(requires.iter().map(|value| value.to_string()).collect())
        .probe("true".into())
        .build()
}

pub(crate) fn base() -> ImageLayerSpec {
    layer("base-ubuntu-24.04", "2026.10.13", ImageLayerTier::Base, &["os:ubuntu@24.04"], &[])
}

fn runtime() -> Vec<ImageLayerSpec> {
    vec![
        layer("crew-utilities", "1.0.0", ImageLayerTier::AgentRuntime, &["crew:utilities"], &[]),
        layer("harness-codex", "0.160.0", ImageLayerTier::AgentRuntime, &["harness:codex", "harness:codex@0.160.0"], &[]),
        layer("harness-claude-code", "2.1.287", ImageLayerTier::AgentRuntime, &["harness:claude-code"], &[]),
    ]
}

fn catalogue() -> Vec<ImageLayerSpec> {
    let mut layers = vec![
        base(),
        layer("rust", "1.99.0", ImageLayerTier::Shared, &["toolchain:rust", "toolchain:rust@1.99.0"], &[]),
        layer("node", "24.18.0", ImageLayerTier::Shared, &["toolchain:node"], &[]),
        layer("zig", "0.15.2", ImageLayerTier::Shared, &["toolchain:zig", "toolchain:zig@0.15"], &[]),
        layer("zig", "0.16.0", ImageLayerTier::Shared, &["toolchain:zig", "toolchain:zig@0.16"], &[]),
        layer("display-x11", "1.0.0", ImageLayerTier::Shared, &["display:x11"], &[]),
        layer("formal-modelling", "1.0.0", ImageLayerTier::Shared, &["tool:formal-modelling"], &["toolchain:node"]),
    ];
    layers.extend(runtime());
    layers
}

fn names(resolved: &ResolvedImage) -> Vec<String> {
    resolved.layers.iter().map(|layer| format!("{}@{}", layer.name, layer.version)).collect()
}

fn request() -> ImageRequest {
    ImageRequest::builder().project("flotilla".into()).build()
}

// ---- per-definition validation ----

// Every per-definition rule refuses its violation and names the layer; the
// fixture itself is valid.
#[test]
fn validation_refuses_each_rule_violation() {
    for layer in catalogue() {
        layer.validate().expect("fixture is valid");
    }
    let shared = || layer("rust", "1.99.0", ImageLayerTier::Shared, &["toolchain:rust"], &[]);
    let pin = |artifacts: &[(&str, &str, &str)]| ImageLayerPin {
        version: "1.28.2".into(),
        artifacts: artifacts
            .iter()
            .map(|(architecture, url, sha256)| {
                (architecture.to_string(), PinArtifact { url: url.to_string(), sha256: sha256.to_string(), unpack: PinUnpack::File })
            })
            .collect(),
    };
    let hex = "a".repeat(64);
    let cases: Vec<(&str, ImageLayerSpec)> = vec![
        ("empty name", ImageLayerSpec { name: String::new(), ..shared() }),
        ("upper-case name", ImageLayerSpec { name: "Rust".into(), ..shared() }),
        ("version with a dash", ImageLayerSpec { version: "1.99-beta".into(), ..shared() }),
        ("empty version segment", ImageLayerSpec { version: "1..0".into(), ..shared() }),
        ("empty fragment", ImageLayerSpec { fragment: "  ".into(), ..shared() }),
        ("empty probe", ImageLayerSpec { probe: String::new(), ..shared() }),
        ("project source without project", ImageLayerSpec { source: ImageLayerSource::Project, tier: ImageLayerTier::Project, ..shared() }),
        ("project named by stdlib", ImageLayerSpec { project: Some("flotilla".into()), ..shared() }),
        (
            "project source in shared tier",
            ImageLayerSpec { source: ImageLayerSource::Project, project: Some("flotilla".into()), ..shared() },
        ),
        ("project tier from installation", ImageLayerSpec { tier: ImageLayerTier::Project, ..shared() }),
        ("base without from", ImageLayerSpec { from: None, ..base() }),
        ("base with a tag", ImageLayerSpec { from: Some("ubuntu:24.04".into()), ..base() }),
        ("base requiring", ImageLayerSpec { requires: BTreeSet::from(["toolchain:rust".into()]), ..base() }),
        ("shared with from", ImageLayerSpec { from: Some(BASE_FROM.into()), ..shared() }),
        ("unnamespaced provide", ImageLayerSpec { provides: BTreeSet::from(["rust".into()]), ..shared() }),
        ("unnamespaced require", ImageLayerSpec { requires: BTreeSet::from(["node".into()]), ..shared() }),
        ("pin name", ImageLayerSpec { pins: BTreeMap::from([("Rustup".into(), pin(&[]))]), ..shared() }),
        (
            "pin without version",
            ImageLayerSpec {
                pins: BTreeMap::from([("rustup".into(), ImageLayerPin { version: " ".into(), artifacts: BTreeMap::new() })]),
                ..shared()
            },
        ),
        (
            "url without sha256",
            ImageLayerSpec { pins: BTreeMap::from([("rustup".into(), pin(&[("any", "https://x.test/r", "")]))]), ..shared() },
        ),
        ("empty url", ImageLayerSpec { pins: BTreeMap::from([("rustup".into(), pin(&[("amd64", " ", &hex)]))]), ..shared() }),
        (
            "upper-case sha256",
            ImageLayerSpec { pins: BTreeMap::from([("rustup".into(), pin(&[("any", "https://x.test/r", &"A".repeat(64))]))]), ..shared() },
        ),
        (
            "short per-arch sha256",
            ImageLayerSpec {
                pins: BTreeMap::from([("rustup".into(), pin(&[("amd64", "https://x.test/a", &hex), ("arm64", "https://x.test/b", "ab")]))]),
                ..shared()
            },
        ),
        (
            "shared and per-arch artifacts",
            ImageLayerSpec {
                pins: BTreeMap::from([("rustup".into(), pin(&[("any", "https://x.test/r", &hex), ("arm64", "https://x.test/b", &hex)]))]),
                ..shared()
            },
        ),
        (
            "upper-case architecture",
            ImageLayerSpec { pins: BTreeMap::from([("rustup".into(), pin(&[("AMD64", "https://x.test/r", &hex)]))]), ..shared() },
        ),
        ("harness without its provide", layer("harness-codex", "1.0", ImageLayerTier::AgentRuntime, &["harness:claude"], &[])),
        ("crew utilities without its provide", layer("crew-utilities", "1.0", ImageLayerTier::AgentRuntime, &["tool:gh"], &[])),
        ("absolute context path", ImageLayerSpec { context_files: context("/etc/x"), ..shared() }),
        ("parent context path", ImageLayerSpec { context_files: context("a/../b"), ..shared() }),
        ("dot context path", ImageLayerSpec { context_files: context("./a"), ..shared() }),
        ("trailing slash context path", ImageLayerSpec { context_files: context("a/"), ..shared() }),
        (
            "oversized context",
            ImageLayerSpec {
                context_files: BTreeMap::from([(
                    "big".into(),
                    ImageContextFile { content: "x".repeat(MAX_CONTEXT_FILE_BYTES + 1), executable: false },
                )]),
                ..shared()
            },
        ),
        ("secret name", ImageLayerSpec { build_secrets: BTreeSet::from(["GH_TOKEN".into()]), ..shared() }),
    ];
    for (case, spec) in cases {
        let error = spec.validate().expect_err(case);
        assert!(error.contains(&format!("`{}`", spec.name)) || spec.name.is_empty() || error.contains("name"), "{case}: {error}");
    }
    let valid_pins = ImageLayerSpec {
        pins: BTreeMap::from([
            ("rustup".into(), pin(&[("amd64", "https://x.test/r-x86_64", &hex)])),
            ("toolchain".into(), pin(&[])),
            ("tea".into(), pin(&[("any", "https://x.test/t", &hex)])),
            ("gh".into(), pin(&[("amd64", "https://x.test/gh_amd64.tar.gz", &hex), ("arm64", "https://x.test/gh_arm64.tar.gz", &hex)])),
        ]),
        context_files: context("prelude.d/50-display-x11.sh"),
        build_secrets: BTreeSet::from(["forgejo-token".into()]),
        ..shared()
    };
    valid_pins.validate().expect("pins, context and secrets are valid");
}

fn context(path: &str) -> BTreeMap<String, ImageContextFile> {
    BTreeMap::from([(path.to_string(), ImageContextFile { content: "echo".into(), executable: true })])
}

// The fragment FROM contract: stages start FROM ${BASE} or scratch, aliases
// carry the layer prefix, and only the last stage (FROM ${BASE}) is unaliased.
#[test]
fn fragment_from_contract() {
    let accepted = [
        "FROM ${BASE}\nRUN true",
        "# comment\nfrom ${BASE}\n",
        "FROM scratch AS rust-fetch\nCOPY a b\nFROM ${BASE} AS rust-build\nRUN make\nFROM ${BASE}\nCOPY --from=rust-build /x /x",
        "FROM ${BASE} as rust-tools\nFROM ${BASE}",
    ];
    for fragment in accepted {
        validate_fragment_stages("rust", fragment).unwrap_or_else(|error| panic!("{fragment:?}: {error}"));
    }
    let refused = [
        ("RUN true", "no FROM"),
        ("FROM ubuntu:24.04", "foreign image"),
        ("FROM ${BASE} AS rust-out", "aliased output stage"),
        ("FROM ${BASE}\nFROM scratch AS rust-x", "output not last"),
        ("FROM scratch", "scratch output"),
        ("FROM ${BASE}\nRUN x\nFROM ${BASE}", "two unaliased stages"),
        ("FROM ${BASE} AS node-fetch\nFROM ${BASE}", "foreign alias prefix"),
        ("FROM ${BASE} AS rust\nFROM ${BASE}", "alias without suffix"),
        ("FROM ${BASE} AS rust-a\nFROM ${BASE} AS rust-a\nFROM ${BASE}", "duplicate alias"),
        ("FROM --platform=linux/amd64 ${BASE}", "platform flag"),
        ("FROM ${BASE} AS", "dangling AS"),
    ];
    for (fragment, case) in refused {
        assert!(validate_fragment_stages("rust", fragment).is_err(), "{case}");
    }
    // `flotilla-` is reserved for generated stage names (`flotilla-layer-N`).
    let reserved = validate_fragment_stages("flotilla-layer", "FROM ${BASE} AS flotilla-layer-1\nFROM ${BASE}").expect_err("reserved");
    assert!(reserved.contains("flotilla-"), "{reserved}");
    // An alias equal to the layer name would shadow its named build context.
    assert!(validate_fragment_stages("rust", "FROM scratch AS rust\nFROM ${BASE}").is_err());
}

// ---- stored shapes ----

// ADR 0047: an ADR 0053 record (every retired field present) decodes to an
// inert Retired value, rewrites as the retired marker, and the marker decodes
// back. Current specs round-trip and stay strict.
#[test]
fn retired_layers_decode_inert_and_never_reserialize_old_fields() {
    let old = serde_json::json!({
        "stage": "capability",
        "parent": {"kind": "rebasable"},
        "repository": "https://example.test/images",
        "revision": "1".repeat(40),
        "fragment": "image-layers/display/Dockerfile",
        "args": {"A": "b"},
        "pins": {"zig": {"value": "0.16.0", "source": "https://ziglang.org", "adoption": {"policy": "approve"}}},
        "provides": ["display:x11"],
        "requires": [],
        "probes": {"display:x11": ["xdpyinfo"]},
        "input_stability": "pinned"
    });
    let retired: ImageLayerResourceSpec = serde_json::from_value(old).expect("retired record decodes");
    assert!(retired.is_retired());
    assert!(retired.current().is_none());
    ImageLayer::validate_spec(&InputMeta::builder().name("old".into()).build(), &retired).expect("retired manifests stay admissible");
    let encoded = serde_json::to_value(&retired).expect("encode");
    assert_eq!(encoded, serde_json::json!({"retired": true}));
    assert!(serde_json::from_value::<ImageLayerResourceSpec>(encoded).expect("marker decodes").is_retired());
    let minimal: ImageLayerResourceSpec =
        serde_yml::from_str("stage: base\nparent: {kind: image, value: x}\nrepository: r\nrevision: v\nfragment: f\n")
            .expect("yaml retired");
    assert!(minimal.is_retired());
    assert!(serde_json::from_value::<ImageLayerResourceSpec>(serde_json::json!({"stage": "base", "surprise": 1})).is_err());

    let current = ImageLayerResourceSpec::from(base());
    let encoded = serde_json::to_value(&current).expect("encode current");
    assert!(encoded.get("stage").is_none());
    assert_eq!(serde_json::from_value::<ImageLayerResourceSpec>(encoded.clone()).expect("round-trip"), current);
    let mut unknown = encoded;
    unknown["stage"] = serde_json::json!("base");
    assert!(serde_json::from_value::<ImageLayerResourceSpec>(unknown).is_err(), "the current shape refuses retired fields");
    assert!(serde_json::from_value::<ImageLayerResourceSpec>(serde_json::json!({"name": "x"})).is_err());
}

// ---- identity ----

// Identity is stable for equal inputs, changes with every included field and
// with order, and ignores source, project, provides, requires and secret values.
#[hegel::test]
fn identity_covers_exactly_the_included_fields(tc: hegel::TestCase) {
    let layers = vec![base(), layer("rust", "1.99.0", ImageLayerTier::Shared, &["toolchain:rust"], &[]), runtime().remove(0)];
    let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(layers.len() - 1));
    let text = tc.draw(gs::text().max_size(8));
    let identity = image_identity(&layers);
    assert!(identity.starts_with("sha256:") && crate::is_image_digest(&identity));
    let roundtrip = serde_json::from_value::<Vec<ImageLayerSpec>>(serde_json::to_value(&layers).expect("encode")).expect("decode");
    assert_eq!(image_identity(&roundtrip), identity, "stable across storage");

    for field in 0..3 {
        let mut excluded = layers.clone();
        let target = &mut excluded[index];
        match field {
            0 => {
                target.provides.insert(format!("tool:{}", text.len()));
            }
            1 => {
                target.requires.insert(format!("tool:x{}", text.len()));
            }
            _ => target.source = ImageLayerSource::Installation,
        }
        assert_eq!(image_identity(&excluded), identity, "excluded field {field} does not change identity");
    }

    for field in 0..9 {
        let mut included = layers.clone();
        let target = &mut included[index];
        match field {
            0 => target.name.push_str("-x"),
            1 => target.version.push_str(".1"),
            2 => target.tier = if target.tier == ImageLayerTier::Shared { ImageLayerTier::AgentRuntime } else { ImageLayerTier::Shared },
            3 => target.from = Some(format!("changed{}", target.from.clone().unwrap_or_default())),
            4 => target.fragment.push_str(&format!("\n#{text}")),
            5 => {
                target.pins.insert(format!("p{}", text.len()), ImageLayerPin { version: text.clone(), artifacts: BTreeMap::new() });
            }
            6 => {
                target.context_files.insert(format!("f{}", text.len()), ImageContextFile { content: text.clone(), executable: false });
            }
            7 => target.probe.push_str(&format!(" && true{text}")),
            _ => {
                target.build_secrets.insert(format!("s{}", text.len()));
            }
        }
        assert_ne!(image_identity(&included), identity, "included field {field} changes identity");
    }
    let mut reordered = roundtrip;
    reordered.swap(1, 2);
    assert_ne!(image_identity(&reordered), identity, "order is part of identity");
}

// ---- resolution ----

// Base, selected shared layers sorted by name, project layer, then the agent
// runtime (harnesses for the roles plus crew utilities) sorted by name.
#[test]
fn resolution_orders_tiers_and_derives_the_agent_runtime() {
    let mut layers = catalogue();
    layers.push(layer("flotilla", "1.0", ImageLayerTier::Project, &["project:flotilla"], &["toolchain:rust"]));
    let other = ImageLayerSpec { project: Some("cleat".into()), ..layer("cleat", "1.0", ImageLayerTier::Project, &["project:cleat"], &[]) };
    layers.push(other);
    let resolved = resolve_image(
        &ImageCatalogue::new(layers),
        &ImageRequest {
            selection: Some(ImageSelection { base: None, layers: vec!["zig".into(), "rust".into()] }),
            harness_adapters: BTreeSet::from(["codex".into(), "claude-code".into()]),
            ..request()
        },
    )
    .expect("resolve");
    assert_eq!(
        names(&resolved),
        [
            "base-ubuntu-24.04@2026.10.13",
            "rust@1.99.0",
            "zig@0.16.0",
            "flotilla@1.0",
            "crew-utilities@1.0.0",
            "harness-claude-code@2.1.287",
            "harness-codex@0.160.0"
        ]
    );
    assert_eq!(resolved.identity, image_identity(&resolved.layers));
    let without_project = resolve_image(&ImageCatalogue::new(catalogue()), &request()).expect("resolve");
    assert_eq!(
        names(&without_project),
        ["base-ubuntu-24.04@2026.10.13", "crew-utilities@1.0.0"],
        "no image: still base plus agent runtime"
    );
}

// `zig` selects the highest version; `zig@0.15` the highest matching dotted
// prefix; an unmatched version is refused by name.
#[test]
fn versions_select_highest_matching_definition() {
    let catalogue = ImageCatalogue::new(catalogue());
    let select = |reference: &str| {
        resolve_image(
            &catalogue,
            &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec![reference.into()] }), ..request() },
        )
    };
    assert!(names(&select("zig").expect("default")).contains(&"zig@0.16.0".to_string()));
    assert!(names(&select("zig@0.15").expect("pinned")).contains(&"zig@0.15.2".to_string()));
    assert!(names(&select("zig@0.15.2").expect("exact")).contains(&"zig@0.15.2".to_string()));
    assert_eq!(select("zig@0.1").expect_err("prefix is dotted").to_string(), "image layer `zig@0.1` is not defined");
    assert_eq!(
        select("crew-utilities").expect_err("tier").to_string(),
        "image layer `crew-utilities` is a agent_runtime layer, not a shared layer"
    );
    let twice = resolve_image(
        &catalogue,
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["zig".into(), "zig@0.15".into()] }), ..request() },
    );
    assert!(matches!(twice, Err(ImageResolutionError::DuplicateSelection { .. })));
}

// Project > installation > stdlib per name, hiding every lower version and
// reporting it; a same-source duplicate is a clash naming the layer.
#[test]
fn shadowing_and_clashes() {
    let mut layers = catalogue();
    let installation_rust = ImageLayerSpec {
        source: ImageLayerSource::Installation,
        ..layer("rust", "1.98.0", ImageLayerTier::Shared, &["toolchain:rust"], &[])
    };
    layers.push(installation_rust);
    let catalogue = ImageCatalogue::new(layers.clone());
    let resolved = resolve_image(
        &catalogue,
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["rust".into()] }), ..request() },
    )
    .expect("resolve");
    assert!(names(&resolved).contains(&"rust@1.98.0".to_string()), "installation shadows a higher stdlib version");
    assert_eq!(
        resolved.shadowed,
        [ShadowedImageLayer {
            name: "rust".into(),
            source: ImageLayerSource::Installation,
            shadowed: ImageLayerSource::Stdlib,
            versions: vec!["1.99.0".into()]
        }]
    );
    let pinned = resolve_image(
        &catalogue,
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["rust@1.99".into()] }), ..request() },
    );
    assert_eq!(pinned.expect_err("shadowed versions are hidden").to_string(), "image layer `rust@1.99` is not defined");

    layers.push(layer("node", "24.18.0", ImageLayerTier::Shared, &["toolchain:node"], &[]));
    let catalogue = ImageCatalogue::new(layers);
    let clash = "image layer `node` version `24.18.0` is defined twice in the stdlib source";
    // A clash refuses only resolutions that touch the clashed name.
    resolve_image(&catalogue, &request()).expect("an unrelated resolution is unaffected");
    let selected = ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["node".into()] }), ..request() };
    assert_eq!(resolve_image(&catalogue, &selected).expect_err("selected by name").to_string(), clash);
    let needed = ImageRequest { needs: BTreeSet::from(["toolchain:node".into()]), ..request() };
    assert_eq!(resolve_image(&catalogue, &needed).expect_err("candidate provider").to_string(), clash);
    let required = ImageRequest { needs: BTreeSet::from(["tool:formal-modelling".into()]), ..request() };
    assert_eq!(resolve_image(&catalogue, &required).expect_err("required provider").to_string(), clash);
    let errors = catalogue.effective("flotilla").check().expect_err("check reports the clash");
    assert!(errors.iter().any(|error| error.to_string() == clash), "{errors:?}");
}

// Resource names are injective over (source, project, name, version): names
// that would read alike, and versions that sanitise alike, stay distinct.
#[test]
fn resource_names_are_injective() {
    let project = |project: &str, name: &str, version: &str| ImageLayerSpec {
        project: Some(project.into()),
        ..layer(name, version, ImageLayerTier::Project, &["project:x"], &[])
    };
    let names = [
        project("app", "x-native", "1.0.0").resource_name(),
        project("app-x", "native", "1.0.0").resource_name(),
        layer("rust", "1.0.A", ImageLayerTier::Shared, &["toolchain:rust"], &[]).resource_name(),
        layer("rust", "1.0.B", ImageLayerTier::Shared, &["toolchain:rust"], &[]).resource_name(),
        ImageLayerSpec {
            source: ImageLayerSource::Installation,
            ..layer("rust", "1.0.A", ImageLayerTier::Shared, &["toolchain:rust"], &[])
        }
        .resource_name(),
    ];
    assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len(), "{names:?}");
    assert!(names[0].starts_with("project-app-x-native-1.0.0-"), "{}", names[0]);
    assert!(names[2].starts_with("stdlib-rust-1.0.a-"), "{}", names[2]);
    for name in &names {
        assert!(
            name.len() <= 253
                && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.')
        );
    }
    assert_eq!(names[0], project("app", "x-native", "1.0.0").resource_name(), "names are stable");
}

// Needs resolve by exact provides lookup: provided-already adds nothing, zero
// providers names the need, two providers names both.
#[test]
fn needs_resolve_by_provides_lookup() {
    let catalogue = ImageCatalogue::new(catalogue());
    let with_needs = |needs: &[&str], layers: &[&str]| {
        resolve_image(
            &catalogue,
            &ImageRequest {
                selection: Some(ImageSelection { base: None, layers: layers.iter().map(|name| name.to_string()).collect() }),
                needs: needs.iter().map(|need| need.to_string()).collect(),
                ..request()
            },
        )
    };
    let display = with_needs(&["display:x11"], &[]).expect("display need");
    assert!(names(&display).contains(&"display-x11@1.0.0".to_string()));
    let pinned = with_needs(&["toolchain:zig@0.15"], &["zig@0.15"]).expect("selected layer provides the need");
    assert_eq!(names(&pinned).iter().filter(|name| name.starts_with("zig@")).count(), 1);
    assert_eq!(with_needs(&["display:wayland"], &[]).expect_err("missing").to_string(), "unresolvable image need `display:wayland`");

    let mut layers = self::catalogue();
    layers.push(layer("xvfb", "1.0", ImageLayerTier::Shared, &["display:x11"], &[]));
    let duplicate =
        resolve_image(&ImageCatalogue::new(layers), &ImageRequest { needs: BTreeSet::from(["display:x11".into()]), ..request() });
    assert_eq!(duplicate.expect_err("duplicate").to_string(), "duplicate providers for `display:x11`: display-x11, xvfb");
}

// Needs match providers with capability semantics: unversioned, `@version`
// and `>=minimum` needs accept any matching provide, with duplicate refusal
// over the matching candidates; exact strings behave as before.
#[test]
fn needs_match_provides_by_capability() {
    let catalogue = ImageCatalogue::new(catalogue());
    let need = |need: &str| resolve_image(&catalogue, &ImageRequest { needs: BTreeSet::from([need.to_string()]), ..request() });
    assert!(names(&need("toolchain:rust>=1.90").expect("minimum")).contains(&"rust@1.99.0".to_string()));
    assert!(names(&need("toolchain:rust@1.99.0").expect("exact")).contains(&"rust@1.99.0".to_string()));
    assert_eq!(need("toolchain:rust>=2.0").expect_err("too new").to_string(), "unresolvable image need `toolchain:rust>=2.0`");
    let mut layers = self::catalogue();
    layers.push(layer("rust-nightly", "1.100.0", ImageLayerTier::Shared, &["toolchain:rust@1.100.0"], &[]));
    let duplicate =
        resolve_image(&ImageCatalogue::new(layers), &ImageRequest { needs: BTreeSet::from(["toolchain:rust>=1.90".into()]), ..request() });
    assert_eq!(duplicate.expect_err("two matching").to_string(), "duplicate providers for `toolchain:rust>=1.90`: rust, rust-nightly");
}

// `requires` pulls its providers in transitively and orders them first within
// the tier; a requirement nobody provides names the need and the layer; an
// upward requirement across tiers and a cycle are refused.
#[test]
fn requires_closure_and_ordering() {
    let mut layers = catalogue();
    layers.push(layer("aa-docs", "1.0", ImageLayerTier::Shared, &["tool:docs"], &["tool:formal-modelling"]));
    let resolved = resolve_image(
        &ImageCatalogue::new(layers.clone()),
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["aa-docs".into()] }), ..request() },
    )
    .expect("resolve");
    assert_eq!(names(&resolved)[1..4], ["node@24.18.0", "formal-modelling@1.0.0", "aa-docs@1.0"]);

    let mut missing = layers.clone();
    missing.push(layer("needs-go", "1.0", ImageLayerTier::Shared, &["tool:x"], &["toolchain:go"]));
    let error = resolve_image(
        &ImageCatalogue::new(missing),
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["needs-go".into()] }), ..request() },
    )
    .expect_err("unresolvable require");
    assert_eq!(error.to_string(), "unresolvable image need `toolchain:go` (required by layer `needs-go`)");

    let mut upward = layers.clone();
    upward.push(layer("needs-harness", "1.0", ImageLayerTier::Shared, &["tool:y"], &["harness:codex"]));
    let error = resolve_image(
        &ImageCatalogue::new(upward),
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["needs-harness".into()] }), ..request() },
    )
    .expect_err("upward");
    assert!(matches!(error, ImageResolutionError::RequiresUpward { ref layer, .. } if layer == "needs-harness"), "{error}");

    let mut cycle = layers;
    cycle.push(layer("a", "1.0", ImageLayerTier::Shared, &["tool:a"], &["tool:b"]));
    cycle.push(layer("b", "1.0", ImageLayerTier::Shared, &["tool:b"], &["tool:a"]));
    let error = resolve_image(
        &ImageCatalogue::new(cycle),
        &ImageRequest { selection: Some(ImageSelection { base: None, layers: vec!["a".into()] }), ..request() },
    )
    .expect_err("cycle");
    assert!(matches!(error, ImageResolutionError::RequiresCycle { .. }), "{error}");
}

// Base: the project's, else the installation default, else the only base in
// the catalogue, else refuse. Installation default layers apply only when the
// project declares no image.
#[test]
fn base_and_installation_defaults() {
    let mut layers = catalogue();
    let catalogue_one = ImageCatalogue::new(layers.clone());
    assert_eq!(resolve_image(&catalogue_one, &request()).expect("only base").layers[0].name, "base-ubuntu-24.04");
    layers.push(ImageLayerSpec { name: "base-debian-13".into(), ..base() });
    let catalogue = ImageCatalogue::new(layers);
    assert!(matches!(resolve_image(&catalogue, &request()), Err(ImageResolutionError::NoBase { ref candidates }) if candidates.len() == 2));
    let defaults = ImageSelection { base: Some("base-debian-13".into()), layers: vec!["rust".into()] };
    let installation = resolve_image(&catalogue, &ImageRequest { defaults: Some(defaults.clone()), ..request() }).expect("defaults");
    assert_eq!(names(&installation)[..2], ["base-debian-13@2026.10.13", "rust@1.99.0"]);
    let project = resolve_image(
        &catalogue,
        &ImageRequest {
            selection: Some(ImageSelection { base: None, layers: vec!["node".into()] }),
            defaults: Some(defaults),
            ..request()
        },
    )
    .expect("project selection");
    assert_eq!(names(&project)[..2], ["base-debian-13@2026.10.13", "node@24.18.0"], "default base fills; default layers do not apply");
    let wrong = resolve_image(
        &catalogue,
        &ImageRequest { selection: Some(ImageSelection { base: Some("rust".into()), layers: vec![] }), ..request() },
    );
    assert!(matches!(wrong, Err(ImageResolutionError::WrongTier { .. })));
}

// Harness needs are satisfied by the derived harness layer: they add the
// adapter's layer and check its version rather than searching provides.
#[test]
fn harness_needs_use_the_derived_layer() {
    let catalogue = ImageCatalogue::new(catalogue());
    let need = |value: &str| resolve_image(&catalogue, &ImageRequest { needs: BTreeSet::from([value.into()]), ..request() });
    assert!(names(&need("harness:codex>=0.150").expect("satisfied")).contains(&"harness-codex@0.160.0".to_string()));
    assert_eq!(
        need("harness:codex>=0.170").expect_err("too old").to_string(),
        "harness layer `harness-codex` does not satisfy `harness:codex>=0.170`"
    );
    assert_eq!(need("harness:gemini").expect_err("unknown").to_string(), "unresolvable image need `harness:gemini`");
}

// Resolution is a function of the catalogue's contents, not its order, and
// its output respects tier order with every requirement provided earlier.
// Generate need subsets, selections and catalogue permutations.
#[hegel::test]
fn resolution_is_order_independent_and_well_formed(tc: hegel::TestCase) {
    let mut layers = catalogue();
    let needs = tc.draw(
        gs::btree_sets(gs::sampled_from(vec!["display:x11", "tool:formal-modelling", "toolchain:zig@0.15", "harness:codex"])).max_size(4),
    );
    let selection = tc.draw(gs::btree_sets(gs::sampled_from(vec!["rust", "node", "zig", "display-x11"])).max_size(4));
    let adapters = tc.draw(gs::btree_sets(gs::sampled_from(vec!["codex", "claude-code"])).max_size(2));
    let rotation = tc.draw(gs::integers::<usize>().min_value(0).max_value(layers.len() - 1));
    let request = ImageRequest {
        selection: Some(ImageSelection { base: None, layers: selection.iter().map(|name| name.to_string()).collect() }),
        needs: needs.iter().map(|need| need.to_string()).collect(),
        harness_adapters: adapters.iter().map(|adapter| adapter.to_string()).collect(),
        ..request()
    };
    let expected = resolve_image(&ImageCatalogue::new(layers.clone()), &request);
    layers.rotate_left(rotation);
    if tc.draw(gs::booleans()) {
        layers.reverse();
    }
    let resolved = resolve_image(&ImageCatalogue::new(layers), &request);
    assert_eq!(resolved, expected);
    let Ok(resolved) = resolved else { return };
    assert!(resolved.layers.windows(2).all(|pair| pair[0].tier <= pair[1].tier));
    for (index, layer) in resolved.layers.iter().enumerate() {
        for need in &layer.requires {
            assert!(
                resolved.layers[..index].iter().any(|earlier| earlier.provides.contains(need)),
                "{need} provided before {}",
                layer.name
            );
        }
    }
    for need in &request.needs {
        if !need.starts_with("harness:") {
            assert!(resolved.layers.iter().any(|layer| layer.provides.contains(need)), "{need} provided");
        }
    }
    for adapter in &adapters {
        assert!(resolved.layers.iter().any(|layer| layer.provides.contains(&format!("harness:{adapter}"))));
    }
}

// Catalogue checks report duplicate providers and unresolvable requires by
// name; the fixture catalogue is clean.
#[test]
fn catalogue_check_reports_definition_errors() {
    ImageCatalogue::new(catalogue()).effective("flotilla").check().expect("clean catalogue");
    let mut layers = catalogue();
    layers.push(layer("xvfb", "1.0", ImageLayerTier::Shared, &["display:x11"], &["toolchain:go"]));
    let errors = ImageCatalogue::new(layers).effective("flotilla").check().expect_err("errors");
    let messages = errors.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        messages,
        ["duplicate providers for `display:x11`: display-x11, xvfb", "unresolvable image need `toolchain:go` (required by layer `xvfb`)"]
    );
    let records = [ImageLayerResourceSpec::Retired, ImageLayerResourceSpec::from(base())];
    assert_eq!(ImageCatalogue::from_specs(&records).layers().len(), 1, "retired records are dropped");
}

// ---- layer directories ----

fn directory(yaml: &str) -> BTreeMap<String, Vec<u8>> {
    BTreeMap::from([
        (LAYER_DEFINITION_FILE.to_string(), yaml.as_bytes().to_vec()),
        (LAYER_FRAGMENT_FILE.to_string(), b"FROM ${BASE}\nCOPY --from=display-x11 prelude.d/50-display-x11.sh /x\n".to_vec()),
        ("prelude.d/50-display-x11.sh".to_string(), b"Xvfb :99 &\n".to_vec()),
        ("README.md".to_string(), b"ignored".to_vec()),
    ])
}

const DISPLAY_YAML: &str = r#"
name: display-x11
version: 1.0.0
tier: shared
provides: [display:x11]
pins:
  mesa:
    version: "24.0"
    url: https://example.test/mesa.tar.xz
    sha256: {amd64: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", arm64: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
probe: |
  command -v Xvfb
context_files: [prelude.d/50-display-x11.sh]
build_secrets: []
"#;

#[test]
fn layer_directory_parses_and_inlines_declared_files() {
    let spec = parse_layer_directory_with_executables(
        &directory(DISPLAY_YAML),
        &BTreeSet::from(["prelude.d/50-display-x11.sh".to_string()]),
        ImageLayerSource::Stdlib,
        None,
    )
    .expect("parse");
    assert_eq!(spec.name, "display-x11");
    assert_eq!(spec.tier, ImageLayerTier::Shared);
    assert!(spec.fragment.starts_with("FROM ${BASE}"));
    // The previous `url` + per-architecture `sha256` shape converts (ADR 0047).
    let mesa = spec.pins["mesa"].artifact("arm64").expect("arm64 artifact");
    assert_eq!(
        (mesa.url.as_str(), mesa.sha256.as_str(), mesa.unpack),
        ("https://example.test/mesa.tar.xz", "b".repeat(64).as_str(), PinUnpack::TarXz)
    );
    assert_eq!(spec.context_files.keys().collect::<Vec<_>>(), ["prelude.d/50-display-x11.sh"]);
    assert!(spec.context_files["prelude.d/50-display-x11.sh"].executable);
    let plain = parse_layer_directory(&directory(DISPLAY_YAML), ImageLayerSource::Installation, None).expect("parse");
    assert!(!plain.context_files["prelude.d/50-display-x11.sh"].executable);
    assert!(plain.resource_name().starts_with("installation-display-x11-1.0.0-"));
}

#[test]
fn layer_directory_errors_name_the_problem() {
    let without = |path: &str| {
        let mut files = directory(DISPLAY_YAML);
        files.remove(path);
        parse_layer_directory(&files, ImageLayerSource::Stdlib, None).expect_err(path)
    };
    assert_eq!(without(LAYER_DEFINITION_FILE), "layer directory has no `layer.yaml`");
    assert_eq!(without(LAYER_FRAGMENT_FILE), "layer directory has no `Dockerfile.fragment`");
    assert!(without("prelude.d/50-display-x11.sh").contains("declared context file"));
    let parse = |yaml: &str| parse_layer_directory(&directory(yaml), ImageLayerSource::Stdlib, None).expect_err(yaml);
    assert!(parse(&format!("{DISPLAY_YAML}surprise: 1\n")).contains("surprise"));
    assert!(parse("name: [").starts_with("layer.yaml"));
    assert!(parse(&DISPLAY_YAML.replace("tier: shared", "tier: capability")).contains("tier"));
    assert!(parse(&DISPLAY_YAML.replace("version: 1.0.0", "version: 1.0-rc")).contains("version"));
    let mut binary = directory(DISPLAY_YAML);
    binary.insert("prelude.d/50-display-x11.sh".into(), vec![0xff, 0xfe]);
    assert!(parse_layer_directory(&binary, ImageLayerSource::Stdlib, None).expect_err("binary").contains("not UTF-8"));
    let project = parse_layer_directory(
        &directory(&DISPLAY_YAML.replace("tier: shared", "tier: project")),
        ImageLayerSource::Project,
        Some("wheelhouse"),
    )
    .expect("project layer");
    assert!(project.resource_name().starts_with("project-wheelhouse-display-x11-1.0.0-"));
}

// Decision 3 on #3909 (ADR 0047): a stored or authored pin in the previous
// shape decodes into explicit artifacts and is written back only as
// `artifacts`; the two shapes cannot be mixed, and unknown fields still refuse.
#[test]
fn previous_pin_shapes_decode_and_only_artifacts_are_written() {
    let hex = |c: char| c.to_string().repeat(64);
    let decode = |value: serde_json::Value| serde_json::from_value::<ImageLayerPin>(value);
    let current = decode(serde_json::json!({
        "version": "0.160.0",
        "artifacts": {
            "amd64": {"url": "https://x.test/codex-x86_64.tar.gz", "sha256": hex('a'), "unpack": "tar.gz"},
            "arm64": {"url": "https://x.test/codex-aarch64.tar.gz", "sha256": hex('b'), "unpack": "tar.gz"}
        }
    }))
    .expect("current shape");
    assert_eq!(current.artifact("arm64").map(|artifact| artifact.url.as_str()), Some("https://x.test/codex-aarch64.tar.gz"));
    assert_eq!(current.artifact("riscv64"), None);
    let cases = [
        (serde_json::json!({"version": "1"}), serde_json::json!({"version": "1"})),
        (
            serde_json::json!({"version": "1.7.4", "url": "https://x.test/tla2tools.jar", "sha256": hex('c')}),
            serde_json::json!({"version": "1.7.4", "artifacts": {"any": {"url": "https://x.test/tla2tools.jar", "sha256": hex('c'), "unpack": "file"}}}),
        ),
        (
            serde_json::json!({"version": "0.62.1", "url": "https://x.test/apalache.tgz", "sha256": hex('d')}),
            serde_json::json!({"version": "0.62.1", "artifacts": {"any": {"url": "https://x.test/apalache.tgz", "sha256": hex('d'), "unpack": "tar.gz"}}}),
        ),
        (
            serde_json::json!({"version": "0.16.0", "url": "https://x.test/zig-{arch}.tar.xz", "sha256": {"amd64": hex('a'), "arm64": hex('b')}}),
            serde_json::json!({"version": "0.16.0", "artifacts": {
                "amd64": {"url": "https://x.test/zig-{arch}.tar.xz", "sha256": hex('a'), "unpack": "tar.xz"},
                "arm64": {"url": "https://x.test/zig-{arch}.tar.xz", "sha256": hex('b'), "unpack": "tar.xz"}
            }}),
        ),
    ];
    for (previous, written) in cases {
        let pin = decode(previous.clone()).expect("previous shape decodes");
        assert_eq!(serde_json::to_value(&pin).expect("encode"), written, "{previous}");
        assert_eq!(decode(written).expect("written shape decodes"), pin);
    }
    let mixed = decode(serde_json::json!({"version": "1", "url": "https://x.test/r", "artifacts": {}})).expect_err("mixed shapes");
    assert!(mixed.to_string().contains("together with the previous"), "{mixed}");
    assert!(decode(serde_json::json!({"version": "1", "checksum": hex('a')})).is_err(), "unknown pin field");
    assert!(
        decode(serde_json::json!({"version": "1", "artifacts": {"any": {"url": "u", "sha256": hex('a'), "unpack": "zip"}}})).is_err(),
        "unknown unpack kind"
    );
    let legacy =
        decode(serde_json::json!({"version": "1", "url": "https://x.test/{arch}", "sha256": {"amd64": hex('a')}})).expect("legacy");
    assert!(is_legacy_template(&legacy.artifacts["amd64"].url), "converted templates stay recognisable until the shim is removed");
}
