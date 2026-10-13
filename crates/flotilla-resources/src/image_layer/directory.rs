//! The layer directory format shared by the stdlib loader, project-layer sync
//! and `flotilla image check`: `layer.yaml`, `Dockerfile.fragment` and the
//! context files `layer.yaml` declares. Undeclared files are ignored.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::{ImageContextFile, ImageLayerPin, ImageLayerSource, ImageLayerSpec, ImageLayerTier};

pub const LAYER_DEFINITION_FILE: &str = "layer.yaml";
pub const LAYER_FRAGMENT_FILE: &str = "Dockerfile.fragment";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerDefinitionFile {
    name: String,
    version: String,
    tier: ImageLayerTier,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    provides: BTreeSet<String>,
    #[serde(default)]
    requires: BTreeSet<String>,
    #[serde(default)]
    pins: BTreeMap<String, ImageLayerPin>,
    probe: String,
    #[serde(default)]
    context_files: Vec<String>,
    #[serde(default)]
    build_secrets: BTreeSet<String>,
}

/// Parse one layer directory, given its files keyed by path relative to the
/// directory. Context files are not executable.
pub fn parse_layer_directory(
    files: &BTreeMap<String, Vec<u8>>,
    source: ImageLayerSource,
    project: Option<&str>,
) -> Result<ImageLayerSpec, String> {
    parse_layer_directory_with_executables(files, &BTreeSet::new(), source, project)
}

/// As [`parse_layer_directory`], marking the context files in `executable`
/// (paths relative to the directory) executable.
pub fn parse_layer_directory_with_executables(
    files: &BTreeMap<String, Vec<u8>>,
    executable: &BTreeSet<String>,
    source: ImageLayerSource,
    project: Option<&str>,
) -> Result<ImageLayerSpec, String> {
    let text = |path: &str| -> Result<String, String> {
        let bytes = files.get(path).ok_or_else(|| format!("layer directory has no `{path}`"))?;
        String::from_utf8(bytes.clone()).map_err(|_| format!("layer file `{path}` is not UTF-8"))
    };
    let definition: LayerDefinitionFile =
        serde_yml::from_str(&text(LAYER_DEFINITION_FILE)?).map_err(|error| format!("{LAYER_DEFINITION_FILE}: {error}"))?;
    let fragment = text(LAYER_FRAGMENT_FILE)?;
    let mut context_files = BTreeMap::new();
    for path in &definition.context_files {
        if path == LAYER_DEFINITION_FILE || path == LAYER_FRAGMENT_FILE {
            return Err(format!("context file `{path}` is part of the layer definition"));
        }
        let content = text(path).map_err(|error| format!("declared context file: {error}"))?;
        if context_files.insert(path.clone(), ImageContextFile { content, executable: executable.contains(path) }).is_some() {
            return Err(format!("context file `{path}` is declared twice"));
        }
    }
    let spec = ImageLayerSpec {
        name: definition.name,
        version: definition.version,
        tier: definition.tier,
        source,
        project: project.map(str::to_owned),
        from: definition.from,
        fragment,
        pins: definition.pins,
        provides: definition.provides,
        requires: definition.requires,
        probe: definition.probe,
        context_files,
        build_secrets: definition.build_secrets,
    };
    spec.validate()?;
    Ok(spec)
}
