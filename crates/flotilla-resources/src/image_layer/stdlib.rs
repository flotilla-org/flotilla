//! The flotilla stdlib image layers (ADR 0062), shipped as release data. The build script embeds
//! every directory under `stdlib/image-layers/`; they are code-backed definitions, read-only and
//! versioned with the binary, never stored (#3824 layer 2a).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

use super::{parse_layer_directory_with_executables, ImageLayerSource, ImageLayerSpec};

/// One embedded layer directory: file paths relative to it, contents and executable bits.
pub struct EmbeddedLayerDirectory {
    pub directory: &'static str,
    pub files: &'static [(&'static str, &'static [u8], bool)],
}

/// The layer directories this release ships.
pub static STDLIB_IMAGE_LAYERS: &[EmbeddedLayerDirectory] = include!(concat!(env!("OUT_DIR"), "/stdlib_image_layers.rs"));

/// Parse embedded layer directories as stdlib definitions.
pub fn parse_stdlib_layers(directories: &[EmbeddedLayerDirectory]) -> Result<Vec<ImageLayerSpec>, String> {
    directories
        .iter()
        .map(|directory| {
            let files = directory.files.iter().map(|(path, contents, _)| (path.to_string(), contents.to_vec())).collect::<BTreeMap<_, _>>();
            let executable = directory
                .files
                .iter()
                .filter(|(_, _, executable)| *executable)
                .map(|(path, _, _)| path.to_string())
                .collect::<BTreeSet<_>>();
            parse_layer_directory_with_executables(&files, &executable, ImageLayerSource::Stdlib, None)
                .map_err(|error| format!("stdlib image layer `{}`: {error}", directory.directory))
        })
        .collect()
}

/// This release's stdlib layers, parsed once. A release whose stdlib does not parse is a
/// packaging error the shipped-catalogue test catches; were one to ship, every read of a built-in
/// layer refuses with the parse error, never an empty stdlib.
pub fn stdlib_image_layers() -> Result<&'static [ImageLayerSpec], &'static str> {
    static LAYERS: OnceLock<Result<Vec<ImageLayerSpec>, String>> = OnceLock::new();
    match LAYERS.get_or_init(|| parse_stdlib_layers(STDLIB_IMAGE_LAYERS)) {
        Ok(layers) => Ok(layers),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::{resolve_image, ImageCatalogue, ImageLayerTier, ImageRequest, ImageSelection, ResolvedImage};

    use super::*;

    const DIGEST: &str = "ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    static FIXTURE: &[EmbeddedLayerDirectory] = &[
        EmbeddedLayerDirectory {
            directory: "base-ubuntu",
            files: &[
                ("Dockerfile.fragment", b"FROM ${BASE}\nRUN true\n", false),
                (
                    "layer.yaml",
                    b"name: base-ubuntu\nversion: 24.04.1\ntier: base\nfrom: ubuntu@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nprovides: [os:ubuntu]\nprobe: test -f /etc/os-release\n",
                    false,
                ),
            ],
        },
        EmbeddedLayerDirectory {
            directory: "display-x11",
            files: &[
                ("Dockerfile.fragment", b"FROM ${BASE}\nCOPY --from=display-x11 prelude.d/50-display.sh /opt/flotilla/prelude.d/\n", false),
                (
                    "layer.yaml",
                    b"name: display-x11\nversion: 1.0.0\ntier: shared\nprovides: [display:x11]\nprobe: command -v Xvfb\ncontext_files: [prelude.d/50-display.sh]\n",
                    false,
                ),
                ("prelude.d/50-display.sh", b"#!/bin/sh\nXvfb :99 &\n", true),
                ("notes.txt", b"undeclared files are not part of the layer", false),
            ],
        },
    ];

    // The shipped stdlib parses, every definition is valid, and the catalogue
    // is clean (no clashes, duplicate providers or unresolvable requires).
    // This holds for zero layers as well as for every layer a release adds.
    #[test]
    fn shipped_stdlib_catalogue_is_clean() {
        let layers = parse_stdlib_layers(STDLIB_IMAGE_LAYERS).expect("shipped stdlib parses");
        assert_eq!(layers.len(), STDLIB_IMAGE_LAYERS.len());
        assert!(layers.iter().all(|layer| layer.source == ImageLayerSource::Stdlib && layer.project.is_none()));
        ImageCatalogue::new(layers).effective("any-project").check().expect("clean catalogue");
    }

    fn shipped_catalogue() -> ImageCatalogue {
        ImageCatalogue::new(parse_stdlib_layers(STDLIB_IMAGE_LAYERS).expect("shipped stdlib parses"))
    }

    fn resolve_shipped(catalogue: &ImageCatalogue, layers: &[&str], needs: &[&str]) -> ResolvedImage {
        let request = ImageRequest::builder()
            .project("p".to_string())
            .selection(ImageSelection::builder().layers(layers.iter().map(|layer| layer.to_string()).collect()).build())
            .needs(needs.iter().map(|need| need.to_string()).collect())
            .harness_adapters(BTreeSet::from(["claude-code".to_string(), "codex".to_string()]))
            .build();
        resolve_image(catalogue, &request).unwrap_or_else(|error| panic!("resolve {layers:?} {needs:?}: {error}"))
    }

    fn names(resolved: &ResolvedImage) -> Vec<&str> {
        resolved.layers.iter().map(|layer| layer.name.as_str()).collect()
    }

    const RUNTIME: [&str; 3] = ["crew-utilities", "harness-claude-code", "harness-codex"];

    // The release ships exactly the ADR 0062 C1 catalogue, each layer at its
    // expected tier.
    #[test]
    fn shipped_stdlib_has_the_c1_layer_set() {
        let tiers = shipped_catalogue().layers().iter().map(|layer| (layer.name.clone(), layer.tier)).collect::<BTreeMap<_, _>>();
        let expected = BTreeMap::from(
            [
                ("base-ubuntu-24.04", ImageLayerTier::Base),
                ("crew-utilities", ImageLayerTier::AgentRuntime),
                ("display-x11", ImageLayerTier::Shared),
                ("formal-modelling", ImageLayerTier::Shared),
                ("harness-claude-code", ImageLayerTier::AgentRuntime),
                ("harness-codex", ImageLayerTier::AgentRuntime),
                ("node", ImageLayerTier::Shared),
                ("rust", ImageLayerTier::Shared),
                ("zig", ImageLayerTier::Shared),
            ]
            .map(|(name, tier)| (name.to_string(), tier)),
        );
        assert_eq!(tiers, expected);
    }

    // The C1 projects' selections resolve to the expected ordered layers:
    // base first, shared layers ordered by `requires` then name, and the agent
    // runtime (both harnesses plus crew utilities) last.
    #[test]
    fn c1_selections_resolve_to_expected_layers() {
        let catalogue = shipped_catalogue();
        fn image(shared: &[&'static str]) -> Vec<&'static str> {
            ["base-ubuntu-24.04"].into_iter().chain(shared.iter().copied()).chain(RUNTIME).collect()
        }
        let flotilla = resolve_shipped(&catalogue, &["rust"], &[]);
        assert_eq!(names(&flotilla), image(&["rust"]));
        let cleat = resolve_shipped(&catalogue, &["rust", "zig"], &[]);
        assert_eq!(names(&cleat), image(&["rust", "zig"]));
        let katzensteg = resolve_shipped(&catalogue, &["rust", "zig", "node"], &[]);
        assert_eq!(names(&katzensteg), image(&["node", "rust", "zig"]));
        assert_eq!(names(&resolve_shipped(&catalogue, &["rust"], &["display:x11"])), image(&["display-x11", "rust"]));

        // formal-modelling requires toolchain:node, which pulls the node layer
        // in and orders it first.
        let formal = resolve_shipped(&catalogue, &["rust", "formal-modelling"], &[]);
        assert_eq!(names(&formal), image(&["node", "formal-modelling", "rust"]));
        assert_eq!(names(&resolve_shipped(&catalogue, &["rust"], &["tool:quint"])), names(&formal));

        let identities = [&flotilla, &cleat, &katzensteg, &formal].map(|resolved| resolved.identity.clone());
        assert_eq!(identities.iter().collect::<BTreeSet<_>>().len(), identities.len(), "distinct selections have distinct identities");
    }

    // Two independent loads of the release stdlib compose the same identity.
    #[test]
    fn shipped_identity_is_stable_across_loads() {
        let first = resolve_shipped(&shipped_catalogue(), &["rust", "zig"], &["display:x11"]);
        let second = resolve_shipped(&shipped_catalogue(), &["rust", "zig"], &["display:x11"]);
        assert_eq!(first, second);
    }

    // Fixture directories parse with their declared context files and
    // executable bits; undeclared files are ignored.
    #[test]
    fn fixture_directories_parse_as_stdlib_layers() {
        let layers = parse_stdlib_layers(FIXTURE).expect("fixture parses");
        assert!(layers[0].resource_name().starts_with("stdlib-base-ubuntu-24.04.1-"));
        assert!(layers[1].resource_name().starts_with("stdlib-display-x11-1.0.0-"));
        assert_eq!(layers[0].tier, ImageLayerTier::Base);
        assert_eq!(layers[0].from.as_deref(), Some(DIGEST));
        let prelude = &layers[1].context_files["prelude.d/50-display.sh"];
        assert!(prelude.executable);
        assert_eq!(layers[1].context_files.len(), 1);
        let broken = [EmbeddedLayerDirectory { directory: "broken", files: &[("layer.yaml", b"name: broken\n", false)] }];
        assert!(parse_stdlib_layers(&broken).expect_err("invalid layer").contains("stdlib image layer `broken`"));
    }
}
