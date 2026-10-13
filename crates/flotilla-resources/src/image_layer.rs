//! Crew image layer definitions (ADR 0062).
//!
//! A composed image is an ordered list of layer definitions. Each definition is
//! a Dockerfile fragment plus typed pins, provides, requires, a probe, declared
//! context files and build secret names. The fragment is opaque apart from its
//! declared `FROM` contract.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ApiPaths, InputMeta, NoStatusPatch, ReplicationClass, Resource, ResourceError};

mod directory;
mod resolve;

pub use directory::{parse_layer_directory, parse_layer_directory_with_executables, LAYER_DEFINITION_FILE, LAYER_FRAGMENT_FILE};
pub use resolve::{
    resolve_image, EffectiveImageCatalogue, ImageCatalogue, ImageRequest, ImageResolutionError, ImageSelection, ResolvedImage,
    ShadowedImageLayer,
};

/// Upper bound on the summed content of one layer's declared context files.
pub const MAX_CONTEXT_FILE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLayer;
impl Resource for ImageLayer {
    type Spec = ImageLayerResourceSpec;
    type Status = ();
    type StatusPatch = NoStatusPatch;
    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "imagelayers", kind: "ImageLayer" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::Definitions;
    /// Retired records stay admissible so that previous-generation manifests in
    /// ops repositories apply during the roll; they are inert for resolution.
    fn validate_spec(_meta: &InputMeta, spec: &Self::Spec) -> Result<(), ResourceError> {
        match spec {
            ImageLayerResourceSpec::Current(spec) => spec.validate().map_err(ResourceError::invalid),
            ImageLayerResourceSpec::Retired => Ok(()),
        }
    }
}

/// The stored spec of an ImageLayer: a current definition, or a retired
/// ADR 0053 record that decodes for one generation and is ignored by
/// resolution. Retired values serialize as `{"retired": true}`, never as the
/// old shape.
///
/// ADR 0047: remove `Retired` (and its decoders) one fleet roll after ADR 0062
/// image layers land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageLayerResourceSpec {
    Current(Box<ImageLayerSpec>),
    Retired,
}

impl ImageLayerResourceSpec {
    pub fn current(&self) -> Option<&ImageLayerSpec> {
        match self {
            Self::Current(spec) => Some(spec),
            Self::Retired => None,
        }
    }

    pub fn into_current(self) -> Option<ImageLayerSpec> {
        match self {
            Self::Current(spec) => Some(*spec),
            Self::Retired => None,
        }
    }

    pub fn is_retired(&self) -> bool {
        matches!(self, Self::Retired)
    }
}

impl From<ImageLayerSpec> for ImageLayerResourceSpec {
    fn from(spec: ImageLayerSpec) -> Self {
        Self::Current(Box::new(spec))
    }
}

impl Serialize for ImageLayerResourceSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Current(spec) => spec.serialize(serializer),
            Self::Retired => RetiredMarker { retired: true }.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ImageLayerResourceSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match retired_dispatch(&value, "tier", "stage") {
            Shape::Current => serde_json::from_value(value).map(|spec: ImageLayerSpec| spec.into()).map_err(serde::de::Error::custom),
            Shape::Retired => {
                serde_json::from_value::<RetiredImageLayerRecord>(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Retired)
            }
            Shape::Marker => {
                serde_json::from_value::<RetiredMarker>(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Retired)
            }
            Shape::Unknown => Err(serde::de::Error::custom("image layer spec needs `tier` (ADR 0062 definition)")),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetiredMarker {
    pub(crate) retired: bool,
}

pub(crate) enum Shape {
    Current,
    Retired,
    Marker,
    Unknown,
}

/// Classify a stored object by its discriminating field: the current shape's
/// required field, a retired shape's required field, or the retired marker.
pub(crate) fn retired_dispatch(value: &serde_json::Value, current: &str, retired: &str) -> Shape {
    let Some(object) = value.as_object() else { return Shape::Unknown };
    if object.contains_key(current) {
        Shape::Current
    } else if object.contains_key(retired) {
        Shape::Retired
    } else if object.contains_key("retired") {
        Shape::Marker
    } else {
        Shape::Unknown
    }
}

/// ADR 0053 layer record (stage chain, repository revision, path fragment).
/// Accepted and dropped for one generation; ADR 0047: remove one roll after
/// ADR 0062 image layers land.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredImageLayerRecord {
    #[serde(rename = "stage")]
    _stage: serde::de::IgnoredAny,
    #[serde(default, rename = "parent")]
    _parent: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "repository")]
    _repository: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "revision")]
    _revision: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "fragment")]
    _fragment: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "args")]
    _args: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "pins")]
    _pins: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "provides")]
    _provides: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "requires")]
    _requires: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "probes")]
    _probes: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "input_stability")]
    _input_stability: Option<serde::de::IgnoredAny>,
}

/// One layer definition. The spec, not the resource name, is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerSpec {
    /// Logical layer name, such as `rust` or `harness-codex`.
    pub name: String,
    /// Dotted version of this definition, such as `1.99.0`.
    pub version: String,
    pub tier: ImageLayerTier,
    pub source: ImageLayerSource,
    /// Owning project; required iff `source` is `project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Base tier only: an image reference pinned by sha256 (multi-arch index) digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Dockerfile fragment text.
    pub fragment: String,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, ImageLayerPin>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub provides: BTreeSet<String>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub requires: BTreeSet<String>,
    /// Non-empty POSIX sh script run on the composed image.
    pub probe: String,
    /// Relative path to content, consumed through the layer's named build context.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_files: BTreeMap<String, ImageContextFile>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub build_secrets: BTreeSet<String>,
}

/// Fixed composition order, most stable first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLayerTier {
    Base,
    Shared,
    Project,
    AgentRuntime,
}

/// Definition source, ordered by increasing resolution precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageLayerSource {
    Stdlib,
    Installation,
    Project,
}

impl ImageLayerSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdlib => "stdlib",
            Self::Installation => "installation",
            Self::Project => "project",
        }
    }
}

impl std::fmt::Display for ImageLayerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ImageLayerTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Shared => "shared",
            Self::Project => "project",
            Self::AgentRuntime => "agent_runtime",
        }
    }
}

impl std::fmt::Display for ImageLayerTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageLayerPin {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<ImagePinDigest>,
}

/// A download checksum: one lowercase hex digest, or one per architecture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImagePinDigest {
    Single(String),
    PerArchitecture(BTreeMap<String, String>),
}

impl ImagePinDigest {
    /// The checksum used when building for `architecture` (`amd64`, `arm64`).
    pub fn for_architecture(&self, architecture: &str) -> Option<&str> {
        match self {
            Self::Single(hex) => Some(hex),
            Self::PerArchitecture(digests) => digests.get(architecture).map(String::as_str),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageContextFile {
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

/// Open namespaced capabilities with exact or dotted minimum versions.
/// Unversioned needs accept a versioned provide of the same capability.
pub fn capability_satisfies(provided: &str, need: &str) -> bool {
    let (provided_name, version) = provided.split_once('@').map_or((provided, None), |(name, version)| (name, Some(version)));
    if let Some((name, minimum)) = need.split_once(">=") {
        return name == provided_name && version.is_some_and(|version| crate::version_at_least(version, minimum));
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
    /// Per-definition validation applied on every write. Catalogue-level rules
    /// (duplicate providers, unresolvable requires, clashes) live in
    /// [`EffectiveImageCatalogue::check`].
    pub fn validate(&self) -> Result<(), String> {
        let layer = &self.name;
        validate_layer_name(&self.name)?;
        if !is_dotted_version(&self.version) {
            return Err(format!("image layer `{layer}`: version `{}` must be dotted alphanumeric segments", self.version));
        }
        if self.fragment.trim().is_empty() || self.probe.trim().is_empty() {
            return Err(format!("image layer `{layer}`: fragment and probe must be non-empty"));
        }
        match (self.source, &self.project) {
            (ImageLayerSource::Project, Some(project)) if !project.trim().is_empty() => {}
            (ImageLayerSource::Project, _) => return Err(format!("image layer `{layer}`: project-source layers name their project")),
            (_, Some(_)) => return Err(format!("image layer `{layer}`: only project-source layers name a project")),
            (_, None) => {}
        }
        if (self.source == ImageLayerSource::Project) != (self.tier == ImageLayerTier::Project) {
            return Err(format!("image layer `{layer}`: the project tier is exactly the project-source layers"));
        }
        match (self.tier, &self.from) {
            (ImageLayerTier::Base, Some(from)) if is_digest_pinned_reference(from) => {}
            (ImageLayerTier::Base, _) => {
                return Err(format!("image layer `{layer}`: base layers declare `from` pinned by a sha256 digest"));
            }
            (_, Some(_)) => return Err(format!("image layer `{layer}`: only base layers declare `from`")),
            (_, None) => {}
        }
        if self.tier == ImageLayerTier::Base && !self.requires.is_empty() {
            return Err(format!("image layer `{layer}`: base layers cannot require capabilities"));
        }
        validate_fragment_stages(&self.name, &self.fragment).map_err(|error| format!("image layer `{layer}`: {error}"))?;
        for (name, pin) in &self.pins {
            validate_pin(name, pin).map_err(|error| format!("image layer `{layer}`: {error}"))?;
        }
        for capability in self.provides.iter().chain(&self.requires) {
            validate_capability(capability).map_err(|error| format!("image layer `{layer}`: {error}"))?;
        }
        if let Some(adapter) = self.name.strip_prefix("harness-").filter(|_| self.tier == ImageLayerTier::AgentRuntime) {
            if !self.provides.contains(&format!("harness:{adapter}")) {
                return Err(format!("image layer `{layer}`: harness layers provide `harness:{adapter}`"));
            }
        }
        if self.name == "crew-utilities" && !self.provides.contains("crew:utilities") {
            return Err(format!("image layer `{layer}`: the crew utilities layer provides `crew:utilities`"));
        }
        let mut total = 0usize;
        for (path, file) in &self.context_files {
            validate_context_path(path).map_err(|error| format!("image layer `{layer}`: {error}"))?;
            total = total.saturating_add(file.content.len());
        }
        if total > MAX_CONTEXT_FILE_BYTES {
            return Err(format!("image layer `{layer}`: context files total {total} bytes, above {MAX_CONTEXT_FILE_BYTES}"));
        }
        for secret in &self.build_secrets {
            if !is_kebab_token(secret) {
                return Err(format!("image layer `{layer}`: build secret `{secret}` must match [a-z0-9-]+"));
            }
        }
        Ok(())
    }

    /// Conventional resource name: `<source>-<name>-<version>` or
    /// `project-<project>-<name>-<version>`, restricted to `[a-z0-9.-]`.
    pub fn resource_name(&self) -> String {
        let raw = match (&self.source, &self.project) {
            (ImageLayerSource::Project, Some(project)) => format!("project-{project}-{}-{}", self.name, self.version),
            (source, _) => format!("{source}-{}-{}", self.name, self.version),
        };
        raw.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' { c } else { '-' }).collect()
    }
}

/// `sha256:<64 hex>` of the canonical identity encoding of an ordered layer
/// list (ADR 0062 A10). Included per layer: name, version, tier, `from`,
/// fragment, pins, context files, probe and build secret names. Excluded:
/// source, project, provides, requires, secret values and architecture.
pub fn image_identity(layers: &[ImageLayerSpec]) -> String {
    #[derive(Serialize)]
    struct IdentityLayer<'a> {
        name: &'a str,
        version: &'a str,
        tier: ImageLayerTier,
        from: &'a Option<String>,
        fragment: &'a str,
        pins: &'a BTreeMap<String, ImageLayerPin>,
        context_files: &'a BTreeMap<String, ImageContextFile>,
        probe: &'a str,
        build_secrets: &'a BTreeSet<String>,
    }
    let canonical = layers
        .iter()
        .map(|layer| IdentityLayer {
            name: &layer.name,
            version: &layer.version,
            tier: layer.tier,
            from: &layer.from,
            fragment: &layer.fragment,
            pins: &layer.pins,
            context_files: &layer.context_files,
            probe: &layer.probe,
            build_secrets: &layer.build_secrets,
        })
        .collect::<Vec<_>>();
    let bytes =
        serde_json::to_vec(&("flotilla-image-identity-v1", canonical)).expect("identity encoding is infallible for owned strings and maps");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn validate_layer_name(name: &str) -> Result<(), String> {
    let valid = name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.');
    if valid {
        Ok(())
    } else {
        Err(format!("image layer name `{name}` must match [a-z0-9][a-z0-9.-]*"))
    }
}

pub(crate) fn is_dotted_version(version: &str) -> bool {
    !version.is_empty() && version.split('.').all(|segment| !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}

fn is_kebab_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_digest_pinned_reference(reference: &str) -> bool {
    reference.rsplit_once('@').is_some_and(|(image, digest)| !image.is_empty() && crate::is_image_digest(digest))
}

fn validate_pin(name: &str, pin: &ImageLayerPin) -> Result<(), String> {
    if !is_kebab_token(name) {
        return Err(format!("pin `{name}` must match [a-z0-9-]+"));
    }
    if pin.version.trim().is_empty() {
        return Err(format!("pin `{name}` needs a version"));
    }
    if pin.url.as_ref().is_some_and(|url| url.trim().is_empty()) {
        return Err(format!("pin `{name}` has an empty url"));
    }
    match &pin.sha256 {
        None if pin.url.is_some() => Err(format!("pin `{name}` downloads a url without a sha256")),
        None => Ok(()),
        Some(ImagePinDigest::Single(hex)) if is_lower_hex_sha256(hex) => Ok(()),
        Some(ImagePinDigest::PerArchitecture(digests))
            if !digests.is_empty()
                && digests.iter().all(|(architecture, hex)| is_kebab_token(architecture) && is_lower_hex_sha256(hex)) =>
        {
            Ok(())
        }
        Some(_) => Err(format!("pin `{name}` sha256 must be 64 lowercase hex (or a non-empty per-architecture map of them)")),
    }
}

fn validate_context_path(path: &str) -> Result<(), String> {
    let normalised = !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if normalised {
        Ok(())
    } else {
        Err(format!("context file path `{path}` must be relative and normalised"))
    }
}

/// The declared fragment contract, the only structural reading of fragment
/// text: every `FROM` is `${BASE}` or `scratch`, optionally `AS <layer>-<suffix>`;
/// the last `FROM` is the layer's output stage, `FROM ${BASE}` without an alias,
/// and every earlier stage is aliased.
fn validate_fragment_stages(layer: &str, fragment: &str) -> Result<(), String> {
    let mut stages = Vec::new();
    for line in fragment.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if !first.eq_ignore_ascii_case("FROM") {
            continue;
        }
        let words = words.collect::<Vec<_>>();
        let (image, alias) = match words.as_slice() {
            [image] => (*image, None),
            [image, keyword, alias] if keyword.eq_ignore_ascii_case("AS") => (*image, Some(*alias)),
            _ => return Err(format!("fragment line `{}` must be `FROM ${{BASE}}|scratch [AS <alias>]`", line.trim())),
        };
        if image != "${BASE}" && image != "scratch" {
            return Err(format!("fragment stages start FROM ${{BASE}} or scratch, not `{image}`"));
        }
        if let Some(alias) = alias {
            let suffix = alias.strip_prefix(layer).and_then(|rest| rest.strip_prefix('-'));
            if suffix.is_none_or(str::is_empty) {
                return Err(format!("fragment stage alias `{alias}` must be `{layer}-<suffix>`"));
            }
        }
        stages.push((image, alias));
    }
    let Some((&(image, alias), earlier)) = stages.split_last() else {
        return Err("fragment has no FROM ${BASE} output stage".into());
    };
    if image != "${BASE}" || alias.is_some() {
        return Err("the last fragment stage must be `FROM ${BASE}` without an alias".into());
    }
    if earlier.iter().any(|(_, alias)| alias.is_none()) {
        return Err("only the final output stage may omit `AS <alias>`".into());
    }
    let aliases = earlier.iter().filter_map(|(_, alias)| *alias).collect::<BTreeSet<_>>();
    if aliases.len() != earlier.len() {
        return Err("fragment stage aliases must be distinct".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
