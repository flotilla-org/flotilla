//! The flotilla stdlib image layers (ADR 0062), shipped as release data. The
//! build script embeds every directory under `stdlib/image-layers/`; the daemon
//! applies them at startup as `source: stdlib` ImageLayer definitions and
//! deletes the stdlib records this release no longer ships.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{parse_layer_directory_with_executables, ImageLayer, ImageLayerSource, ImageLayerSpec, InputMeta, ResourceError};
use flotilla_store::ResourceBackend;

/// One embedded layer directory: file paths relative to it, contents and
/// executable bits.
pub(crate) struct EmbeddedLayerDirectory {
    pub directory: &'static str,
    pub files: &'static [(&'static str, &'static [u8], bool)],
}

/// The layer directories this release ships.
pub(crate) static STDLIB_IMAGE_LAYERS: &[EmbeddedLayerDirectory] = include!(concat!(env!("OUT_DIR"), "/stdlib_image_layers.rs"));

const MANAGED_BY_LABEL: &str = "flotilla.work/managed-by";
const STDLIB_MANAGER: &str = "stdlib";

/// Parse embedded layer directories as stdlib definitions.
pub(crate) fn parse_stdlib_layers(directories: &[EmbeddedLayerDirectory]) -> Result<Vec<ImageLayerSpec>, String> {
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

/// What a startup application changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct StdlibApplication {
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
}

/// Apply `layers` idempotently under their conventional names, leaving
/// unchanged definitions untouched, and delete local stdlib definitions that
/// are no longer shipped. Retired records and other sources are never touched.
pub(crate) async fn apply_stdlib_layers(
    backend: &ResourceBackend,
    namespace: &str,
    layers: &[ImageLayerSpec],
) -> Result<StdlibApplication, String> {
    let local = backend.using::<ImageLayer>(namespace);
    let definitions = backend.definitions::<ImageLayer>(namespace);
    let mut shipped = BTreeSet::new();
    let mut application = StdlibApplication::default();
    for layer in layers {
        let name = layer.resource_name();
        if !shipped.insert(name.clone()) {
            return Err(format!("stdlib image layers define `{name}` twice"));
        }
        match local.get(&name).await {
            Ok(current) if current.metadata.deletion_timestamp.is_none() && current.spec.current() == Some(layer) => continue,
            Ok(_) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
        let meta = InputMeta::builder()
            .name(name.clone())
            .labels(BTreeMap::from([(MANAGED_BY_LABEL.to_string(), STDLIB_MANAGER.to_string())]))
            .build();
        definitions.apply(&meta, &layer.clone().into()).await.map_err(|error| format!("apply stdlib image layer `{name}`: {error}"))?;
        application.applied.push(name);
    }
    // An empty shipped set never deletes: a release cannot usefully ship no
    // stdlib, so an empty set means lost release data, and deleting would wipe
    // every stdlib definition the fleet's admissions resolve against.
    if shipped.is_empty() {
        return Ok(application);
    }
    for stale in local.list().await.map_err(|error| error.to_string())?.items {
        let stdlib =
            stale.metadata.deletion_timestamp.is_none() && stale.spec.current().is_some_and(|spec| spec.source == ImageLayerSource::Stdlib);
        if stdlib && !shipped.contains(&stale.metadata.name) {
            match definitions.delete(&stale.metadata.name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => application.deleted.push(stale.metadata.name),
                Err(error) => return Err(format!("delete retired stdlib image layer `{}`: {error}", stale.metadata.name)),
            }
        }
    }
    Ok(application)
}

/// Startup: apply this release's stdlib. A parse failure applies and deletes
/// nothing, keeping the previous generation's definitions in place.
pub(crate) async fn apply_release_stdlib(backend: &ResourceBackend, namespace: &str) -> Result<StdlibApplication, String> {
    let layers = parse_stdlib_layers(STDLIB_IMAGE_LAYERS)?;
    apply_stdlib_layers(backend, namespace, &layers).await
}

/// #3655: a bare host's sessions take the crew image's harness environment
/// (auto-updater and installation checks off, no update nags, the image
/// locale) from these crew-runtime layers' fragments, so a bare host and a
/// vessel read one definition.
const HARNESS_ENVIRONMENT_LAYERS: &[&str] = &["base-ubuntu-24.04", "crew-utilities", "harness-claude-code", "harness-codex"];
/// The host's own facilities stay the host's: the image's search path and
/// login shell describe the image, not the host.
const HOST_OWNED_ENVIRONMENT: &[&str] = &["PATH", "SHELL"];

static HARNESS_SESSION_ENVIRONMENT: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| harness_environment_from(STDLIB_IMAGE_LAYERS));

/// The image harness environment an agentless session receives.
pub(crate) fn harness_session_environment() -> &'static [(String, String)] {
    &HARNESS_SESSION_ENVIRONMENT
}

/// Literal `ENV NAME=value` assignments of the harness layers. Values that
/// expand variables or name image-only paths (`/opt/flotilla`) cannot hold on
/// a bare host and are left out with the host-owned names. Only the
/// single-assignment form is read: an unquoted value holding whitespace (a
/// multi-pair `ENV A=1 B=2` line, or the legacy `ENV A 1` form) is skipped
/// rather than mis-split, and the pinned test below then reports the loss.
fn harness_environment_from(directories: &[EmbeddedLayerDirectory]) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = Vec::new();
    for layer in HARNESS_ENVIRONMENT_LAYERS {
        let Some(directory) = directories.iter().find(|directory| directory.directory == *layer) else { continue };
        let Some((_, fragment, _)) = directory.files.iter().find(|(path, _, _)| *path == flotilla_resources::LAYER_FRAGMENT_FILE) else {
            continue;
        };
        for line in String::from_utf8_lossy(fragment).lines() {
            let Some((name, value)) = line.trim().strip_prefix("ENV ").and_then(|assignment| assignment.trim().split_once('=')) else {
                continue;
            };
            let value = value.trim();
            let value = match value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) {
                Some(quoted) => quoted,
                None if value.contains(char::is_whitespace) => continue,
                None => value,
            };
            let literal_name = !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if !literal_name || HOST_OWNED_ENVIRONMENT.contains(&name) || value.contains('$') || value.contains("/opt/flotilla") {
                continue;
            }
            environment.retain(|(existing, _)| existing != name);
            environment.push((name.to_string(), value.to_string()));
        }
    }
    environment
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{resolve_image, ImageCatalogue, ImageLayerTier, ImageRequest, ImageSelection, ResolvedImage};
    use flotilla_store_memory::InMemoryBackend;

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

    // #3655 contract 3: agentless sessions get exactly the image's harness
    // environment, read from the shipped layer fragments. A new literal ENV in
    // a harness layer fails here, so reaching bare hosts is a decision.
    #[test]
    fn shipped_harness_layers_define_the_agentless_session_environment() {
        let mut environment = harness_session_environment().to_vec();
        environment.sort();
        assert_eq!(
            environment,
            [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_INSTALLATION_CHECKS", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("LANG", "en_US.UTF-8"),]
                .map(|(name, value)| (name.to_string(), value.to_string()))
        );
    }

    // Image-only values (expansions, `/opt/flotilla` paths) and host-owned
    // names never reach a bare host; a later layer's value wins; layers outside
    // the harness set and malformed or multi-pair lines contribute nothing.
    #[test]
    fn harness_environment_keeps_only_literal_host_applicable_assignments() {
        static LAYERS: &[EmbeddedLayerDirectory] = &[
            EmbeddedLayerDirectory {
                directory: "base-ubuntu-24.04",
                files: &[(
                    "Dockerfile.fragment",
                    b"FROM ${BASE}\nENV SHELL=/bin/bash\nENV LANG=C.UTF-8\nENV TERMINFO_DIRS=/opt/flotilla/share/terminfo\nENV PATH=\"/usr/bin\"\nENV broken\nENV A=1 B=2\n",
                    false,
                )],
            },
            EmbeddedLayerDirectory {
                directory: "harness-claude-code",
                files: &[("Dockerfile.fragment", b"ENV PIN=${PIN_CLAUDE_CODE_VERSION}\nENV LANG=\"en_GB.UTF-8\"\n", false)],
            },
            EmbeddedLayerDirectory { directory: "display-x11", files: &[("Dockerfile.fragment", b"ENV LIBGL_ALWAYS_SOFTWARE=1\n", false)] },
        ];
        assert_eq!(harness_environment_from(LAYERS), vec![("LANG".to_string(), "en_GB.UTF-8".to_string())]);
    }

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

    // Startup application is idempotent (an unchanged release writes
    // nothing), updates changed definitions in place, and deletes stdlib
    // records the release no longer ships while leaving other sources alone.
    #[tokio::test]
    async fn startup_application_is_idempotent_and_prunes_unshipped_stdlib() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let mut layers = parse_stdlib_layers(FIXTURE).expect("fixture parses");
        let (base_name, display_name) = (layers[0].resource_name(), layers[1].resource_name());
        let first = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("first application");
        assert_eq!(first.applied.len(), 2);
        let local = backend.using::<ImageLayer>("flotilla");
        let version = local.get(&display_name).await.expect("display").metadata.resource_version;
        assert_eq!(apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("unchanged"), StdlibApplication::default());
        assert_eq!(local.get(&display_name).await.expect("display").metadata.resource_version, version);

        let mut installation = layers[1].clone();
        installation.source = ImageLayerSource::Installation;
        let installation_name = installation.resource_name();
        backend
            .definitions::<ImageLayer>("flotilla")
            .apply(&InputMeta::builder().name(installation.resource_name()).build(), &installation.into())
            .await
            .expect("installation layer");
        layers[1].probe = "command -v Xvfb && command -v xdpyinfo".into();
        let changed = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("changed release");
        assert_eq!(changed.applied, std::slice::from_ref(&display_name));
        assert_eq!(local.get(&display_name).await.expect("display").spec.current(), Some(&layers[1]));

        let pruned = apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("smaller release");
        assert_eq!(pruned.deleted, std::slice::from_ref(&display_name));
        assert_eq!(apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("settled"), StdlibApplication::default());
        let restored = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("re-shipped");
        assert_eq!(restored.applied, std::slice::from_ref(&display_name));
        assert_eq!(local.get(&display_name).await.expect("restored").spec.current(), Some(&layers[1]));
        apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("pruned again");
        let names = backend
            .definitions::<ImageLayer>("flotilla")
            .list()
            .await
            .expect("layers")
            .into_iter()
            .map(|layer| layer.metadata.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from([installation_name, base_name]));
    }

    // PR #3178 review: an empty shipped set means lost release data (a
    // packaging error), so it must never wipe the definitions admissions use.
    #[tokio::test]
    async fn empty_release_never_deletes_stdlib_definitions() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let layers = parse_stdlib_layers(FIXTURE).expect("fixture parses");
        apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("first application");
        assert_eq!(apply_stdlib_layers(&backend, "flotilla", &[]).await.expect("empty release"), StdlibApplication::default());
        assert_eq!(backend.definitions::<ImageLayer>("flotilla").list().await.expect("layers").len(), layers.len());
    }
}
