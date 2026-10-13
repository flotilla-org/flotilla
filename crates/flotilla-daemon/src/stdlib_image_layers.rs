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

#[cfg(test)]
mod tests {
    use flotilla_resources::{ImageCatalogue, ImageLayerTier};
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

    // The shipped stdlib parses, every definition is valid, and the catalogue
    // is clean (no clashes, duplicate providers or unresolvable requires).
    // This holds for zero layers as well as for every layer a release adds.
    #[test]
    fn shipped_stdlib_catalogue_is_clean() {
        let layers = parse_stdlib_layers(STDLIB_IMAGE_LAYERS).expect("shipped stdlib parses");
        assert_eq!(layers.len(), STDLIB_IMAGE_LAYERS.len());
        assert!(layers.iter().all(|layer| layer.source == ImageLayerSource::Stdlib && layer.project.is_none()));
        ImageCatalogue::new(layers).effective("any-project").expect("no clashes").check().expect("clean catalogue");
    }

    // Fixture directories parse with their declared context files and
    // executable bits; undeclared files are ignored.
    #[test]
    fn fixture_directories_parse_as_stdlib_layers() {
        let layers = parse_stdlib_layers(FIXTURE).expect("fixture parses");
        assert_eq!(
            layers.iter().map(|layer| layer.resource_name()).collect::<Vec<_>>(),
            ["stdlib-base-ubuntu-24.04.1", "stdlib-display-x11-1.0.0"]
        );
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
        let first = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("first application");
        assert_eq!(first.applied.len(), 2);
        let local = backend.using::<ImageLayer>("flotilla");
        let version = local.get("stdlib-display-x11-1.0.0").await.expect("display").metadata.resource_version;
        assert_eq!(apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("unchanged"), StdlibApplication::default());
        assert_eq!(local.get("stdlib-display-x11-1.0.0").await.expect("display").metadata.resource_version, version);

        let mut installation = layers[1].clone();
        installation.source = ImageLayerSource::Installation;
        backend
            .definitions::<ImageLayer>("flotilla")
            .apply(&InputMeta::builder().name(installation.resource_name()).build(), &installation.into())
            .await
            .expect("installation layer");
        layers[1].probe = "command -v Xvfb && command -v xdpyinfo".into();
        let changed = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("changed release");
        assert_eq!(changed.applied, ["stdlib-display-x11-1.0.0"]);
        assert_eq!(local.get("stdlib-display-x11-1.0.0").await.expect("display").spec.current(), Some(&layers[1]));

        let pruned = apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("smaller release");
        assert_eq!(pruned.deleted, ["stdlib-display-x11-1.0.0"]);
        assert_eq!(apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("settled"), StdlibApplication::default());
        let restored = apply_stdlib_layers(&backend, "flotilla", &layers).await.expect("re-shipped");
        assert_eq!(restored.applied, ["stdlib-display-x11-1.0.0"]);
        assert_eq!(local.get("stdlib-display-x11-1.0.0").await.expect("restored").spec.current(), Some(&layers[1]));
        apply_stdlib_layers(&backend, "flotilla", &layers[..1]).await.expect("pruned again");
        let names = backend
            .definitions::<ImageLayer>("flotilla")
            .list()
            .await
            .expect("layers")
            .into_iter()
            .map(|layer| layer.metadata.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from(["installation-display-x11-1.0.0".to_string(), "stdlib-base-ubuntu-24.04.1".to_string()]));
    }
}
