use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{version_at_least, ApiPaths, InputMeta, NoStatusPatch, ReplicationClass, Resource, ResourceError};

pub const IMAGE_LAYERS_ANNOTATION: &str = "flotilla.work/frozen-image-layers";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLayer;
impl Resource for ImageLayer {
    type Spec = ImageLayerSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "imagelayers", kind: "ImageLayer" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        spec.validate().map_err(ResourceError::decode)
    }
}

/// Authored build inputs. Revisions freeze intent; builders resolve file contents
/// separately, since a commit or a host path is not an image's cache identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerSpec {
    pub stage: ImageLayerStage,
    pub parent: ImageLayerParent,
    pub repository: String,
    pub revision: String,
    pub fragment: String,
    #[builder(default)]
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    #[builder(default)]
    #[serde(default)]
    pub pins: BTreeMap<String, ImageInputPin>,
    #[builder(default)]
    #[serde(default)]
    pub provides: BTreeSet<String>,
    #[builder(default)]
    #[serde(default)]
    pub requires: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLayerStage {
    Base,
    Utilities,
    Capability,
    Harness,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ImageLayerParent {
    Image(String),
    Layer(String),
    /// The fragment starts FROM ${BASE}; the builder supplies the realised
    /// digest underneath. This permits RUN and therefore does not imply that
    /// OCI manifest-only rebasing (file-only, no RUN) is safe.
    Rebasable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageInputPin {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<ImageInputAdoption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageInputAdoption {
    Auto { constraint: Option<String> },
    Approve,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenImageLayer {
    pub name: String,
    pub spec: ImageLayerSpec,
}

/// A selection is cascaded by the caller; the composer never picks another
/// base or harness to paper over an unsatisfiable need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerSelection {
    pub base: String,
    pub utilities: Option<String>,
    pub harness: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenImageLayers {
    pub layers: BTreeMap<String, ImageLayerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<ImageLayerSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_image: Option<String>,
}
impl FrozenImageLayers {
    /// Append newly used capabilities without changing already frozen inputs.
    pub fn include(&mut self, composition: &ImageComposition) -> Result<(), String> {
        if self.selection.as_ref().is_some_and(|selection| selection != &composition.selection) {
            return Err("convoy vessels must share their frozen image selection".into());
        }
        if self.baseline_image.as_ref().is_some_and(|image| composition.baseline_image.as_ref().is_some_and(|other| other != image)) {
            return Err("convoy baseline image differs from its frozen image".into());
        }
        for layer in &composition.layers {
            if self.layers.get(&layer.name).is_some_and(|previous| previous != &layer.spec) {
                return Err(format!("convoy image layer `{}` differs from its frozen revision", layer.name));
            }
            if matches!(layer.spec.stage, ImageLayerStage::Base | ImageLayerStage::Harness)
                && self.layers.iter().any(|(name, spec)| name != &layer.name && spec.stage == layer.spec.stage)
            {
                return Err(format!("convoy cannot use two {:?} image layers", layer.spec.stage));
            }
        }
        self.layers.extend(composition.layers.iter().map(|layer| (layer.name.clone(), layer.spec.clone())));
        self.selection.get_or_insert_with(|| composition.selection.clone());
        if self.baseline_image.is_none() {
            self.baseline_image = composition.baseline_image.clone();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageComposition {
    pub selection: ImageLayerSelection,
    #[serde(default)]
    pub needs: BTreeSet<String>,
    #[serde(default)]
    pub layers: Vec<FrozenImageLayer>,
    /// Generation 1 runs the authoritative baseline while recording layers.
    /// Remove this bridge when generation 2 placements use built compositions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_image: Option<String>,
    /// Bound only when placed, never inferred from an admission-time tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PlacedImageIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacedImageIdentity {
    pub local_image_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_digest: Option<String>,
}

impl PlacedImageIdentity {
    pub fn validate(&self) -> Result<(), String> {
        if self.local_image_id.trim().is_empty() || self.registry_digest.as_ref().is_some_and(|digest| digest.trim().is_empty()) {
            return Err("placed image identity is empty".into());
        }
        Ok(())
    }
}

impl ImageComposition {
    /// Derive every stage key in frozen chain order. Actual parent digests
    /// remain build inputs, while the parent key supplies Merkle invalidation.
    pub fn recipe_keys(&self, inputs: &[ResolvedImageInputs]) -> Result<Vec<String>, String> {
        if inputs.len() != self.layers.len() {
            return Err("resolved image inputs do not match the frozen layer chain".into());
        }
        let mut keys = Vec::with_capacity(inputs.len());
        for input in inputs {
            if inputs.first().is_some_and(|first| first.architecture != input.architecture) {
                return Err("image composition cannot mix architectures".into());
            }
            let mut resolved = input.clone();
            resolved.parent_recipe_key = keys.last().cloned();
            keys.push(resolved.recipe_key()?);
        }
        Ok(keys)
    }

    pub fn bind(&mut self, identity: PlacedImageIdentity) -> Result<(), String> {
        identity.validate()?;
        if let Some(previous) = &self.identity {
            if previous != &identity {
                return Err("composition is already bound to a different image".into());
            }
        }
        self.identity = Some(identity);
        Ok(())
    }
}

/// Open namespaced capabilities with exact or dotted minimum versions.
/// Unversioned needs accept a versioned provide of the same capability.
pub fn capability_satisfies(provided: &str, need: &str) -> bool {
    let (provided_name, version) = provided.split_once('@').map_or((provided, None), |(name, version)| (name, Some(version)));
    if let Some((name, minimum)) = need.split_once(">=") {
        return name == provided_name && version.is_some_and(|version| version_at_least(version, minimum));
    }
    if let Some((name, exact)) = need.split_once('@') {
        return name == provided_name && version == Some(exact);
    }
    provided_name == need
}

pub fn validate_capability(value: &str) -> Result<(), String> {
    let unversioned = value.split_once(">=").map_or(value, |(name, _)| name);
    let name = unversioned.split_once('@').map_or(unversioned, |(name, _)| name);
    if !name.contains(':') || value.chars().any(char::is_whitespace) || name.split(':').any(str::is_empty) {
        return Err(format!("invalid namespaced capability `{value}`"));
    }
    Ok(())
}

impl ImageLayerSpec {
    pub fn validate(&self) -> Result<(), String> {
        if [&self.repository, &self.revision, &self.fragment].iter().any(|value| value.trim().is_empty()) {
            return Err("image layer requires repository, revision and Dockerfile fragment".into());
        }
        if !matches!(self.revision.len(), 40 | 64) || !self.revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("image layer revision must be a full pinned commit".into());
        }
        match (&self.stage, &self.parent) {
            (ImageLayerStage::Base, ImageLayerParent::Image(image))
                if is_sha256(image) || image.rsplit_once('@').is_some_and(|(_, digest)| is_sha256(digest)) => {}
            (ImageLayerStage::Utilities, ImageLayerParent::Layer(_)) => {}
            (ImageLayerStage::Capability | ImageLayerStage::Harness | ImageLayerStage::Project, ImageLayerParent::Rebasable) => {}
            _ => return Err(format!("invalid parent for {:?} image layer", self.stage)),
        }
        for capability in self.provides.iter().chain(&self.requires) {
            validate_capability(capability)?;
        }
        Ok(())
    }
}

/// Resolve a minimum-cardinality covering set, breaking ties by canonical names.
/// Frozen entries win over current catalogue revisions; newly used capabilities
/// are appended only after a successful composition, leaving failures atomic.
pub fn compose_image(
    selection: &ImageLayerSelection,
    needs: &BTreeSet<String>,
    catalogue: &BTreeMap<String, ImageLayerSpec>,
    frozen: &mut FrozenImageLayers,
) -> Result<ImageComposition, String> {
    let selection = frozen.selection.as_ref().unwrap_or(selection).clone();
    let mut available = catalogue.clone();
    available.extend(frozen.layers.clone());
    let mut fixed = Vec::new();
    for (name, stage) in [
        (Some(&selection.base), ImageLayerStage::Base),
        (selection.utilities.as_ref(), ImageLayerStage::Utilities),
        (selection.harness.as_ref(), ImageLayerStage::Harness),
        (selection.project.as_ref(), ImageLayerStage::Project),
    ] {
        if let Some(name) = name {
            let spec = available.get(name).ok_or_else(|| format!("image layer `{name}` missing"))?;
            spec.validate().map_err(|error| format!("image layer `{name}`: {error}"))?;
            if spec.stage != stage {
                return Err(format!("image layer `{name}` is not a {stage:?} layer"));
            }
            fixed.push(FrozenImageLayer { name: name.clone(), spec: spec.clone() });
        }
    }
    let mut optional = available
        .iter()
        .filter(|(_, spec)| spec.stage == ImageLayerStage::Capability)
        .map(|(name, spec)| FrozenImageLayer { name: name.clone(), spec: spec.clone() })
        .collect::<Vec<_>>();
    // Only providers of requested capabilities or their transitive prerequisites
    // can affect a minimum cover. Fixed upper layers' requires participate too.
    let mut relevant = needs.clone();
    relevant.extend(fixed.iter().flat_map(|layer| layer.spec.requires.iter().cloned()));
    loop {
        let previous = relevant.len();
        for layer in &optional {
            if layer.spec.provides.iter().any(|provided| relevant.iter().any(|need| capability_satisfies(provided, need))) {
                relevant.extend(layer.spec.requires.clone());
            }
        }
        if previous == relevant.len() {
            break;
        }
    }
    optional.retain(|layer| layer.spec.provides.iter().any(|provided| relevant.iter().any(|need| capability_satisfies(provided, need))));
    // Capability order is lexical: an impossible earlier prerequisite cannot
    // become satisfiable by selecting a later layer or the harness/project.
    let mut possible = fixed
        .iter()
        .filter(|layer| layer.spec.stage < ImageLayerStage::Capability)
        .flat_map(|layer| layer.spec.provides.iter().cloned())
        .collect::<BTreeSet<_>>();
    optional.retain(|layer| {
        let viable = layer.spec.validate().is_ok()
            && layer.spec.requires.iter().all(|need| possible.iter().any(|provided| capability_satisfies(provided, need)));
        if viable {
            possible.extend(layer.spec.provides.clone());
        }
        viable
    });
    // Exact set cover is NP-hard even after pruning. Refuse excessive work
    // explicitly rather than monopolising admission or returning a greedy cover.
    if optional.len() > 64 {
        return Err(format!("image composition search limit: {} relevant capability layers for needs {:?}", optional.len(), needs));
    }
    let mut search_budget = 10_000;
    for need in needs {
        validate_capability(need)?;
        if !fixed.iter().chain(&optional).any(|layer| layer.spec.provides.iter().any(|provided| capability_satisfies(provided, need))) {
            return Err(format!("unsatisfiable image need `{need}`"));
        }
    }
    for count in 0..=optional.len() {
        if let Some(layers) = covering_subset(&fixed, &optional, needs, count, 0, &mut Vec::new(), &mut search_budget)
            .map_err(|()| format!("image composition search limit reached for needs {needs:?}"))?
        {
            let composition = ImageComposition::builder()
                .selection(selection.clone())
                .needs(needs.clone())
                .layers(layers)
                .maybe_baseline_image(frozen.baseline_image.clone())
                .build();
            frozen.include(&composition)?;
            return Ok(composition);
        }
    }
    let detail = needs.iter().cloned().collect::<Vec<_>>().join(", ");
    Err(format!("unsatisfiable image needs `{detail}`: layer requirements or fixed parents cannot be satisfied in canonical order"))
}

fn covering_subset(
    fixed: &[FrozenImageLayer],
    optional: &[FrozenImageLayer],
    needs: &BTreeSet<String>,
    remaining: usize,
    offset: usize,
    selected: &mut Vec<FrozenImageLayer>,
    search_budget: &mut usize,
) -> Result<Option<Vec<FrozenImageLayer>>, ()> {
    if *search_budget == 0 {
        return Err(());
    }
    *search_budget -= 1;
    if remaining == 0 {
        let mut chain = fixed.iter().chain(selected.iter()).cloned().collect::<Vec<_>>();
        chain.sort_by(|left, right| (left.spec.stage, &left.name).cmp(&(right.spec.stage, &right.name)));
        let mut provides = BTreeSet::new();
        let mut previous: Option<&str> = None;
        for layer in &chain {
            if layer.spec.validate().is_err()
                || layer.spec.requires.iter().any(|need| !provides.iter().any(|provided: &String| capability_satisfies(provided, need)))
            {
                return Ok(None);
            }
            if let ImageLayerParent::Layer(parent) = &layer.spec.parent {
                if previous != Some(parent.as_str()) {
                    return Ok(None);
                }
            }
            provides.extend(layer.spec.provides.clone());
            previous = Some(&layer.name);
        }
        return Ok(needs.iter().all(|need| provides.iter().any(|provided| capability_satisfies(provided, need))).then_some(chain));
    }
    if optional.len().saturating_sub(offset) < remaining {
        return Ok(None);
    }
    for index in offset..optional.len() {
        selected.push(optional[index].clone());
        let found = covering_subset(fixed, optional, needs, remaining - 1, index + 1, selected, search_budget)?;
        selected.pop();
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// Builder-resolved content, not paths, repository revisions, timestamps or host
/// identity. Ordered maps normalise args and pins before hashing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ResolvedImageInputs {
    pub parent_digest: String,
    pub parent_recipe_key: Option<String>,
    pub content_hashes: Vec<String>,
    #[builder(default)]
    pub args: BTreeMap<String, String>,
    #[builder(default)]
    pub pins: BTreeMap<String, String>,
    pub architecture: String,
    /// Explicit builder evidence: unresolved package/base inputs are refused.
    pub stability: ImageInputStability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageInputStability {
    Pinned,
    Unpinned,
}

impl ResolvedImageInputs {
    /// Only resolved, pinned inputs have a shareable Merkle key. Callers must
    /// refuse unresolved inputs rather than manufacture a cache identity.
    pub fn recipe_key(&self) -> Result<String, String> {
        if self.stability != ImageInputStability::Pinned
            || !is_sha256(&self.parent_digest)
            || self.parent_recipe_key.as_ref().is_some_and(|key| !is_sha256(key))
            || self.architecture.is_empty()
            || self.content_hashes.is_empty()
            || self.content_hashes.iter().any(|hash| !is_sha256(hash))
            || self.pins.values().any(|value| value.is_empty())
        {
            return Err("cannot share an image recipe key with unresolved inputs".into());
        }
        let mut canonical = self.clone();
        canonical.content_hashes.sort();
        let bytes = serde_json::to_vec(&("flotilla-image-recipe-v1", canonical)).map_err(|error| error.to_string())?;
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}
