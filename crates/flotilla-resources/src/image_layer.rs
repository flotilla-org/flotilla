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
    fn validate_spec(meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        spec.validate().map_err(ResourceError::decode)?;
        if let Some(link) = &spec.link {
            link.validate(&meta.name).map_err(ResourceError::decode)?;
        }
        Ok(())
    }
}

/// Authored build inputs. Revisions freeze intent; builders resolve file contents
/// separately, since a commit or a host path is not an image's cache identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerSpec {
    pub stage: ImageLayerStage,
    /// Absence denotes an ordinary layer, including previous-generation records (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkImageLayer>,
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
    /// Capability checks run in the completed stage with an empty Docker config.
    /// ADR 0047: remove the decoder default one roll after probes land.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probes: BTreeMap<String, Vec<String>>,
    /// Authored testimony that every external build input is pinned. Unknown
    /// inputs get a non-shareable execution key. ADR 0047: default old layers
    /// for one roll rather than assuming their package/network inputs are pinned.
    #[builder(default)]
    #[serde(default)]
    pub input_stability: ImageInputStability,
}

/// A self-contained file producer, independent of the realised runtime parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkImageLayer {
    pub builder_image: String,
    /// The private tree is a directory; bin symlinks and preludes are exact paths.
    pub outputs: BTreeSet<String>,
}

pub const LINK_AUDIT_SCRIPT: &str = include_str!("../../../ci/crew-image/link-audit.sh");

impl LinkImageLayer {
    pub fn validate(&self, name: &str) -> Result<(), String> {
        let safe = |value: &str| {
            !value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) && value != "." && value != ".."
        };
        if !safe(name) || self.builder_image.is_empty() || self.builder_image.chars().any(char::is_whitespace) {
            return Err("link layer needs a safe name and declared builder image".into());
        }
        let tree = format!("/opt/flotilla/layers/{name}/");
        if !self.outputs.contains(&tree) {
            return Err(format!("link layer {name} must export its private tree {tree}"));
        }
        for path in &self.outputs {
            let bin = path.strip_prefix("/opt/flotilla/bin/").is_some_and(safe);
            let prelude = path.strip_prefix("/etc/flotilla/prelude.d/").is_some_and(|file| {
                file.len() > 3 && file.as_bytes()[..2].iter().all(u8::is_ascii_digit) && file[2..] == format!("-{name}.sh")
            });
            if path != &tree && !bin && !prelude {
                return Err(format!("link layer {name} writes outside its allowed paths: {path}"));
            }
        }
        Ok(())
    }

    /// The composer owns FROM, export and audit instructions. Authored fragments
    /// contain only builder-stage instructions and write their output under /out.
    pub fn producer_dockerfile(&self, name: &str, fragment: &str) -> Result<String, String> {
        self.validate(name)?;
        if fragment
            .lines()
            .any(|line| matches!(line.split_whitespace().next().map(str::to_ascii_uppercase).as_deref(), Some("FROM" | "ONBUILD")))
        {
            return Err("link producer fragments cannot declare FROM or ONBUILD".into());
        }
        let outputs = self.outputs.iter().map(|path| format!(" '{path}'")).collect::<String>();
        Ok(format!("ARG BUILDER\nFROM ${{BUILDER}} AS flotilla-link-producer\n{fragment}\nFROM ${{BUILDER}} AS flotilla-link-audit\nCOPY --from=flotilla-link-producer /out/ /out/\nUSER 0:0\nRUN <<'FLOTILLA_LINK_AUDIT'\nunset FLOTILLA_LINK_OUTPUT_ROOT\nset -- '{name}'{outputs}\n{LINK_AUDIT_SCRIPT}\nFLOTILLA_LINK_AUDIT\nFROM scratch\nCOPY --from=flotilla-link-audit --chown=0:0 /out/ /\n"))
    }

    pub fn splice_dockerfile(&self, name: &str, source: &str) -> Result<String, String> {
        self.validate(name)?;
        if !is_sha256(source) && !source.rsplit_once('@').is_some_and(|(_, digest)| is_sha256(digest)) {
            return Err("link splice needs a realised image digest".into());
        }
        let mut text = "ARG BASE\nFROM ${BASE}\n".to_string();
        let mut paths = vec![format!("/opt/flotilla/layers/{name}/")];
        for prefix in ["/opt/flotilla/bin/", "/etc/flotilla/prelude.d/"] {
            if self.outputs.iter().any(|path| path.starts_with(prefix)) {
                paths.push(prefix.into());
            }
        }
        for path in paths {
            text.push_str(&format!("COPY --link --from={source} --chown=0:0 {path} {path}\n"));
        }
        Ok(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLayerStage {
    Base,
    Utilities,
    Project,
    Capability,
    Harness,
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

impl FrozenImageLayer {
    fn order_key(&self) -> (bool, ImageLayerStage, &str) {
        (self.spec.link.is_some(), if self.spec.link.is_some() { ImageLayerStage::Base } else { self.spec.stage }, &self.name)
    }
}

/// A selection is cascaded by the caller; the composer never picks another
/// base to paper over an unsatisfiable need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(try_from = "ImageLayerSelectionRecord")]
pub struct ImageLayerSelection {
    pub base: String,
    pub utilities: Option<String>,
    #[builder(default)]
    #[serde(default)]
    pub harnesses: BTreeSet<String>,
    pub project: Option<String>,
}

// ADR 0047: remove after the next fleet roll, once live-store validation and
// the roll-refreshed corpus contain no legacy single `harness` selections.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageLayerSelectionRecord {
    base: String,
    utilities: Option<String>,
    #[serde(default)]
    harnesses: BTreeSet<String>,
    harness: Option<String>,
    project: Option<String>,
}

impl TryFrom<ImageLayerSelectionRecord> for ImageLayerSelection {
    type Error = String;
    fn try_from(record: ImageLayerSelectionRecord) -> Result<Self, Self::Error> {
        let mut harnesses = record.harnesses;
        if let Some(harness) = record.harness {
            if !harnesses.is_empty() {
                return Err("image selection contains both harness and harnesses".into());
            }
            harnesses.insert(harness);
        }
        Ok(Self { base: record.base, utilities: record.utilities, harnesses, project: record.project })
    }
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
        if composition.literal_base.is_some() && composition.layers.is_empty() {
            return Ok(());
        }
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
            if layer.spec.stage == ImageLayerStage::Base
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
    /// Generation-1 frozen snapshots retain their literal until completion.
    /// Decode bridge: remove after the generation-2 fleet roll (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_image: Option<String>,
    /// A retired baseline resolves to a no-layer composition of its literal.
    /// This is a source image, not a fallback from failed layer builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub literal_base: Option<String>,
    /// Bound only when placed, never inferred from an admission-time tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PlacedImageIdentity>,
    /// ADR 0047: previous-generation compositions omit build dependencies;
    /// remove the decoder default after one fleet roll.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_refs: Vec<String>,
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
    pub fn build_stage_count(&self) -> usize {
        self.layers.len() + self.layers.iter().filter(|layer| layer.spec.link.is_some()).count()
    }

    /// Derive runtime chain keys in frozen order. Link producers have independent
    /// keys folded into their splice keys; ordinary parents supply Merkle invalidation.
    pub fn recipe_keys(&self, inputs: &[ResolvedImageInputs]) -> Result<Vec<String>, String> {
        if inputs.len() != self.layers.len() {
            return Err("resolved image inputs do not match the frozen layer chain".into());
        }
        let mut keys = Vec::with_capacity(inputs.len());
        for (layer, input) in self.layers.iter().zip(inputs) {
            if inputs.first().is_some_and(|first| first.architecture != input.architecture) {
                return Err("image composition cannot mix architectures".into());
            }
            let mut resolved = input.clone();
            if let Some(definition) = &layer.spec.link {
                resolved.link_recipe_keys.clear();
                resolved.link = Some(ResolvedLinkInputs { definition: definition.clone(), requires: layer.spec.requires.clone() });
                let producer = resolved.recipe_key()?;
                resolved.link = None;
                resolved.link_recipe_keys = vec![producer];
            }
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
            (_, ImageLayerParent::Rebasable) if self.link.is_some() && self.stage != ImageLayerStage::Base => {}
            (ImageLayerStage::Base, ImageLayerParent::Image(image))
                if is_sha256(image) || image.rsplit_once('@').is_some_and(|(_, digest)| is_sha256(digest)) => {}
            (ImageLayerStage::Utilities, ImageLayerParent::Layer(_)) => {}
            (ImageLayerStage::Capability | ImageLayerStage::Harness | ImageLayerStage::Project, ImageLayerParent::Rebasable) => {}
            _ => return Err(format!("invalid parent for {:?} image layer", self.stage)),
        }
        if let Some(link) = &self.link {
            if self.stage == ImageLayerStage::Base {
                return Err("a link layer needs an ordinary runtime base".into());
            }
            if self.stage == ImageLayerStage::Harness && self.provides.iter().filter(|provide| provide.starts_with("harness:")).count() != 1
            {
                return Err("a harness link layer must provide exactly one harness capability".into());
            }
            if link.builder_image.is_empty() {
                return Err("link builder image is empty".into());
            }
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
        (selection.project.as_ref(), ImageLayerStage::Project),
    ]
    .into_iter()
    // Defaults apply only without a harness need. Explicit harness needs use
    // the minimal catalogue cover and may omit unrelated default harnesses.
    .chain(
        selection
            .harnesses
            .iter()
            .filter(|_| !needs.iter().any(|need| need.starts_with("harness:")))
            .map(|name| (Some(name), ImageLayerStage::Harness)),
    ) {
        if let Some(name) = name {
            let spec = available.get(name).ok_or_else(|| format!("image layer `{name}` missing"))?;
            spec.validate().map_err(|error| format!("image layer `{name}`: {error}"))?;
            if let Some(link) = &spec.link {
                link.validate(name)?;
            }
            if spec.stage != stage {
                return Err(format!("image layer `{name}` is not a {stage:?} layer"));
            }
            fixed.push(FrozenImageLayer { name: name.clone(), spec: spec.clone() });
        }
    }
    let mut optional = available
        .iter()
        .filter(|(name, spec)| {
            matches!(spec.stage, ImageLayerStage::Capability | ImageLayerStage::Harness) && !fixed.iter().any(|layer| &layer.name == *name)
        })
        .map(|(name, spec)| FrozenImageLayer { name: name.clone(), spec: spec.clone() })
        .collect::<Vec<_>>();
    optional.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
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
    for layer in &optional {
        if let Some(link) = &layer.spec.link {
            link.validate(&layer.name)?;
        }
    }
    // Prune in canonical stage/name order, including fixed predecessors. This
    // is a viability bound; the shared exact search verifies the chosen chain.
    let mut possible = BTreeSet::new();
    let mut predecessors = fixed.iter().collect::<Vec<_>>();
    predecessors.sort_by_key(|layer| layer.order_key());
    let mut predecessors = predecessors.into_iter().peekable();
    optional.retain(|layer| {
        while predecessors.peek().is_some_and(|fixed| fixed.order_key() < layer.order_key()) {
            possible.extend(predecessors.next().expect("peeked predecessor").spec.provides.clone());
        }
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
        return Err(format!("image composition search limit: {} relevant optional image layers for needs {:?}", optional.len(), needs));
    }
    // A conservative first-release ceiling: at most 10,000 nodes, each with
    // at most 64 optional layers. Count work, not time, so identical catalogues
    // refuse identically on different hosts. Tune from fleet measurements;
    // this is a safety bound, not a measured wall-time guarantee.
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
        chain.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
        let mut paths = BTreeSet::new();
        for layer in &chain {
            if let Some(link) = &layer.spec.link {
                link.validate(&layer.name).map_err(|_| ())?;
                for path in &link.outputs {
                    if !paths.insert(path) {
                        return Ok(None);
                    }
                }
            }
        }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<ResolvedLinkInputs>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_recipe_keys: Vec<String>,
    /// A runtime parent for ordinary/splice executions; empty for link producers.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedLinkInputs {
    pub definition: LinkImageLayer,
    pub requires: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageInputStability {
    Pinned,
    #[default]
    Unpinned,
}

impl ResolvedImageInputs {
    /// Unpinned recipes are executable but cannot make a fleet cache claim.
    pub fn execution_key(&self, nonce: Option<&str>) -> Result<String, String> {
        match (self.stability, nonce) {
            (ImageInputStability::Pinned, None) => self.recipe_key(),
            (ImageInputStability::Unpinned, Some(nonce)) if !nonce.is_empty() => {
                let mut validated = self.clone();
                validated.stability = ImageInputStability::Pinned;
                let resolved = validated.canonical_key(false)?;
                Ok(format!(
                    "sha256:{:x}",
                    Sha256::digest(
                        serde_json::to_vec(&("flotilla-unshared-image-v1", resolved, nonce)).map_err(|error| error.to_string())?
                    )
                ))
            }
            _ => Err("unpinned image inputs require a non-shareable execution nonce".into()),
        }
    }

    /// Only resolved, pinned inputs have a shareable Merkle key. Callers must
    /// refuse unresolved inputs rather than manufacture a cache identity.
    pub fn recipe_key(&self) -> Result<String, String> {
        self.canonical_key(true)
    }

    fn canonical_key(&self, shareable: bool) -> Result<String, String> {
        if self.stability != ImageInputStability::Pinned
            || (self.link.is_none() && !is_sha256(&self.parent_digest))
            || (self.link.is_none() && self.parent_recipe_key.as_ref().is_some_and(|key| !is_sha256(key)))
            || self.architecture.is_empty()
            || self.content_hashes.is_empty()
            || self.content_hashes.iter().any(|hash| !is_sha256(hash))
            || (self.link.is_some() && !self.link_recipe_keys.is_empty())
            || self.link_recipe_keys.iter().any(|key| !is_sha256(key))
            || self.pins.values().any(|value| value.is_empty())
        {
            return Err("cannot share an image recipe key with unresolved inputs".into());
        }
        if shareable
            && self.link.as_ref().is_some_and(|link| {
                !is_sha256(&link.definition.builder_image)
                    && !link.definition.builder_image.rsplit_once('@').is_some_and(|(_, digest)| is_sha256(digest))
            })
        {
            return Err("cannot share a link recipe with an unresolved builder image".into());
        }
        let mut canonical = self.clone();
        canonical.content_hashes.sort();
        let bytes = if let Some(link) = &canonical.link {
            serde_json::to_vec(&(
                "flotilla-link-recipe-v1",
                &canonical.architecture,
                &canonical.content_hashes,
                &canonical.args,
                &canonical.pins,
                link,
            ))
            .map_err(|error| error.to_string())?
        } else {
            serde_json::to_vec(&("flotilla-image-recipe-v1", canonical)).map_err(|error| error.to_string())?
        };
        Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
    }
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}
