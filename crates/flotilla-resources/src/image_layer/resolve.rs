//! Pure resolution of a vessel's composed image (ADR 0062, design §2).
//!
//! A selection names layers; needs, `requires` and the agent runtime resolve by
//! exact lookup on `provides`. There is no cover search: zero or several
//! providers is an error naming the capability.
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use super::{image_identity, ImageLayerResourceSpec, ImageLayerSource, ImageLayerSpec, ImageLayerTier};

/// A project's (or the installation default's) image selection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[serde(deny_unknown_fields)]
pub struct ImageSelection {
    /// Base layer name (`base-ubuntu-24.04`, optionally `@version`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Shared layer names, such as `rust` or `zig@0.15`.
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
}

/// Every known layer definition, before per-project effective filtering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageCatalogue {
    layers: Vec<ImageLayerSpec>,
}

impl ImageCatalogue {
    pub fn new(layers: impl IntoIterator<Item = ImageLayerSpec>) -> Self {
        Self { layers: layers.into_iter().collect() }
    }

    /// Stored specs; retired records are dropped.
    pub fn from_specs<'a>(specs: impl IntoIterator<Item = &'a ImageLayerResourceSpec>) -> Self {
        Self::new(specs.into_iter().filter_map(|spec| spec.current().cloned()))
    }

    pub fn layers(&self) -> &[ImageLayerSpec] {
        &self.layers
    }

    /// The catalogue as seen by `project`: other projects' layers are dropped,
    /// and per logical name the highest-precedence source wins (project, then
    /// installation, then stdlib). Two definitions with one (source, name,
    /// version) are a clash, reported by [`EffectiveImageCatalogue::check`]
    /// and refused only by resolutions that touch that name.
    pub fn effective(&self, project: &str) -> EffectiveImageCatalogue {
        let mut by_name: BTreeMap<&str, BTreeMap<ImageLayerSource, Vec<&ImageLayerSpec>>> = BTreeMap::new();
        let mut clashes: BTreeMap<String, Vec<ImageResolutionError>> = BTreeMap::new();
        for layer in &self.layers {
            if layer.source == ImageLayerSource::Project && layer.project.as_deref() != Some(project) {
                continue;
            }
            let versions = by_name.entry(&layer.name).or_default().entry(layer.source).or_default();
            if versions.iter().any(|existing| existing.version == layer.version) {
                clashes.entry(layer.name.clone()).or_default().push(ImageResolutionError::Clash {
                    source: layer.source,
                    name: layer.name.clone(),
                    version: layer.version.clone(),
                });
                continue;
            }
            versions.push(layer);
        }
        let mut effective = BTreeMap::new();
        let mut shadowed = Vec::new();
        for (name, mut sources) in by_name {
            let (winner, mut versions) = sources.pop_last().expect("each name has at least one source");
            for (source, hidden) in sources {
                let mut hidden = hidden.iter().map(|layer| layer.version.clone()).collect::<Vec<_>>();
                hidden.sort_by(|left, right| compare_versions(left, right));
                shadowed.push(ShadowedImageLayer { name: name.to_string(), source: winner, shadowed: source, versions: hidden });
            }
            versions.sort_by(|left, right| compare_versions(&left.version, &right.version));
            effective.insert(name.to_string(), versions.into_iter().cloned().collect());
        }
        EffectiveImageCatalogue { by_name: effective, shadowed, clashes }
    }
}

/// A name defined by a higher-precedence source hides every version of that
/// name from lower sources. Reported, never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowedImageLayer {
    pub name: String,
    /// The winning source.
    pub source: ImageLayerSource,
    /// The hidden source.
    pub shadowed: ImageLayerSource,
    /// Hidden versions, ascending.
    pub versions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveImageCatalogue {
    /// Winning source's versions per name, ascending.
    by_name: BTreeMap<String, Vec<ImageLayerSpec>>,
    shadowed: Vec<ShadowedImageLayer>,
    /// Duplicate (source, name, version) definitions, per name.
    clashes: BTreeMap<String, Vec<ImageResolutionError>>,
}

/// A layer provides `need` when any of its provides satisfies it: exactly,
/// at an `@version`, or at a `>=minimum` (ADR 0062 provides lookup).
fn provides(layer: &ImageLayerSpec, need: &str) -> bool {
    layer.provides.iter().any(|provided| crate::capability_satisfies(provided, need))
}

impl EffectiveImageCatalogue {
    pub fn shadowed(&self) -> &[ShadowedImageLayer] {
        &self.shadowed
    }

    /// The default (highest) version of each name.
    pub fn default_layers(&self) -> impl Iterator<Item = &ImageLayerSpec> {
        self.by_name.values().filter_map(|versions| versions.last())
    }

    /// `rust` selects the highest version; `zig@0.15` the highest version equal
    /// to or dotted-prefixed by `0.15`.
    pub fn lookup(&self, reference: &str) -> Result<&ImageLayerSpec, ImageResolutionError> {
        let (name, prefix) = reference.split_once('@').map_or((reference, None), |(name, version)| (name, Some(version)));
        self.touch(name)?;
        let unknown = || ImageResolutionError::UnknownLayer { reference: reference.to_string() };
        let versions = self.by_name.get(name).ok_or_else(unknown)?;
        versions
            .iter()
            .rev()
            .find(|layer| {
                prefix.is_none_or(|prefix| {
                    layer.version == prefix || layer.version.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
                })
            })
            .ok_or_else(unknown)
    }

    /// Catalogue-level definition errors: invalid definitions, duplicate
    /// providers among default versions of selectable (non-base) layers, and
    /// `requires` that no default version provides.
    pub fn check(&self) -> Result<(), Vec<ImageResolutionError>> {
        let mut errors = self.clashes.values().flatten().cloned().collect::<Vec<_>>();
        for layer in self.by_name.values().flatten() {
            if let Err(reason) = layer.validate() {
                errors.push(ImageResolutionError::Invalid { layer: layer.name.clone(), reason });
            }
        }
        let mut providers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for layer in self.default_layers().filter(|layer| layer.tier != ImageLayerTier::Base) {
            for capability in &layer.provides {
                providers.entry(capability).or_default().push(&layer.name);
            }
        }
        for (capability, names) in &providers {
            if names.len() > 1 {
                errors.push(ImageResolutionError::DuplicateProviders {
                    need: capability.to_string(),
                    providers: names.iter().map(|name| name.to_string()).collect(),
                });
            }
        }
        for layer in self.default_layers() {
            for need in &layer.requires {
                if !self.default_layers().any(|provider| provides(provider, need)) {
                    errors.push(ImageResolutionError::Unresolvable { need: need.clone(), required_by: Some(layer.name.clone()) });
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// A resolution that reaches a clashed name refuses with the clash.
    fn touch(&self, name: &str) -> Result<(), ImageResolutionError> {
        match self.clashes.get(name).and_then(|clashes| clashes.first()) {
            Some(clash) => Err(clash.clone()),
            None => Ok(()),
        }
    }

    fn base_names(&self) -> Vec<&str> {
        self.default_layers().filter(|layer| layer.tier == ImageLayerTier::Base).map(|layer| layer.name.as_str()).collect()
    }

    /// Exact `provides` lookup: a selected layer providing the capability wins;
    /// otherwise exactly one default version of an unselected, non-base name.
    fn provider(
        &self,
        selected: &BTreeMap<String, ImageLayerSpec>,
        capability: &str,
        required_by: Option<&str>,
    ) -> Result<Option<ImageLayerSpec>, ImageResolutionError> {
        if selected.values().any(|layer| provides(layer, capability)) {
            return Ok(None);
        }
        let candidates = self
            .default_layers()
            .filter(|layer| layer.tier != ImageLayerTier::Base && !selected.contains_key(&layer.name))
            .filter(|layer| provides(layer, capability))
            .collect::<Vec<_>>();
        for candidate in &candidates {
            self.touch(&candidate.name)?;
        }
        match candidates.as_slice() {
            [] => Err(ImageResolutionError::Unresolvable { need: capability.to_string(), required_by: required_by.map(str::to_owned) }),
            [layer] => Ok(Some((*layer).clone())),
            _ => Err(ImageResolutionError::DuplicateProviders {
                need: capability.to_string(),
                providers: candidates.iter().map(|layer| layer.name.clone()).collect(),
            }),
        }
    }
}

/// Inputs for one vessel's composition.
#[derive(Debug, Clone, Default, PartialEq, Eq, bon::Builder)]
pub struct ImageRequest {
    pub project: String,
    /// The project's declared `image:`; when present, installation default
    /// layers do not apply (the default base still fills an omitted base).
    pub selection: Option<ImageSelection>,
    /// Installation defaults (fleet-root CrewDefaults cascaded with project-scoped layers).
    pub defaults: Option<ImageSelection>,
    /// Image needs: the vessel's leg needs, its crews' image needs and convoy
    /// `--need`, as capability strings. Callers pass image needs only.
    #[builder(default)]
    pub needs: BTreeSet<String>,
    /// Harness adapters the vessel's crew roles resolve to.
    #[builder(default)]
    pub harness_adapters: BTreeSet<String>,
}

/// One vessel's ordered layer list and its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedImage {
    pub identity: String,
    pub layers: Vec<ImageLayerSpec>,
    /// Shadowing that affects the resolved layers' names.
    pub shadowed: Vec<ShadowedImageLayer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageResolutionError {
    Clash { source: ImageLayerSource, name: String, version: String },
    UnknownLayer { reference: String },
    WrongTier { reference: String, expected: ImageLayerTier, actual: ImageLayerTier },
    NoBase { candidates: Vec<String> },
    DuplicateSelection { name: String, versions: Vec<String> },
    MultipleProjectLayers { names: Vec<String> },
    Unresolvable { need: String, required_by: Option<String> },
    DuplicateProviders { need: String, providers: Vec<String> },
    NotSelectable { need: String, layer: String, tier: ImageLayerTier },
    RequiresUpward { layer: String, need: String, provider: String },
    RequiresCycle { layers: Vec<String> },
    HarnessVersion { need: String, layer: String },
    Invalid { layer: String, reason: String },
}

impl fmt::Display for ImageResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clash { source, name, version } => {
                write!(f, "image layer `{name}` version `{version}` is defined twice in the {source} source")
            }
            Self::UnknownLayer { reference } => write!(f, "image layer `{reference}` is not defined"),
            Self::WrongTier { reference, expected, actual } => {
                write!(f, "image layer `{reference}` is a {actual} layer, not a {expected} layer")
            }
            Self::NoBase { candidates } if candidates.is_empty() => f.write_str("no base image layer is defined"),
            Self::NoBase { candidates } => {
                write!(f, "no base image layer selected and the catalogue defines several: {}", candidates.join(", "))
            }
            Self::DuplicateSelection { name, versions } => {
                write!(f, "image layer `{name}` is selected at several versions: {}", versions.join(", "))
            }
            Self::MultipleProjectLayers { names } => write!(f, "a project has at most one project layer: {}", names.join(", ")),
            Self::Unresolvable { need, required_by: None } => write!(f, "unresolvable image need `{need}`"),
            Self::Unresolvable { need, required_by: Some(layer) } => {
                write!(f, "unresolvable image need `{need}` (required by layer `{layer}`)")
            }
            Self::DuplicateProviders { need, providers } => write!(f, "duplicate providers for `{need}`: {}", providers.join(", ")),
            Self::NotSelectable { need, layer, tier } => {
                write!(f, "image need `{need}` is provided by {tier} layer `{layer}`, which needs cannot select")
            }
            Self::RequiresUpward { layer, need, provider } => {
                write!(f, "image layer `{layer}` requires `{need}` from higher-tier layer `{provider}`")
            }
            Self::RequiresCycle { layers } => write!(f, "image layer requires form a cycle: {}", layers.join(", ")),
            Self::HarnessVersion { need, layer } => write!(f, "harness layer `{layer}` does not satisfy `{need}`"),
            Self::Invalid { layer, reason } => write!(f, "image layer `{layer}` is invalid: {reason}"),
        }
    }
}

impl std::error::Error for ImageResolutionError {}

/// Resolve one vessel's composition: base, selected shared layers, needs and
/// their `requires` closure, the project's project layer, and the agent runtime
/// (one harness layer per adapter plus `crew:utilities`). Roles never remove
/// layers.
pub fn resolve_image(catalogue: &ImageCatalogue, request: &ImageRequest) -> Result<ResolvedImage, ImageResolutionError> {
    let effective = catalogue.effective(&request.project);
    let mut selected = BTreeMap::new();

    let base_reference = match request.selection.as_ref().and_then(|selection| selection.base.clone()) {
        Some(base) => base,
        None => match request.defaults.as_ref().and_then(|defaults| defaults.base.clone()) {
            Some(base) => base,
            None => match effective.base_names().as_slice() {
                [only] => only.to_string(),
                names => return Err(ImageResolutionError::NoBase { candidates: names.iter().map(|name| name.to_string()).collect() }),
            },
        },
    };
    select(&mut selected, by_reference(&effective, &base_reference, ImageLayerTier::Base)?)?;

    let shared = request.selection.as_ref().or(request.defaults.as_ref()).map(|selection| selection.layers.as_slice()).unwrap_or_default();
    for reference in shared {
        select(&mut selected, by_reference(&effective, reference, ImageLayerTier::Shared)?)?;
    }

    let project_layers = effective
        .by_name
        .values()
        .filter_map(|versions| versions.last())
        .filter(|layer| layer.tier == ImageLayerTier::Project)
        .collect::<Vec<_>>();
    for layer in &project_layers {
        effective.touch(&layer.name)?;
    }
    match project_layers.as_slice() {
        [] => {}
        [layer] => select(&mut selected, (*layer).clone())?,
        layers => {
            return Err(ImageResolutionError::MultipleProjectLayers { names: layers.iter().map(|layer| layer.name.clone()).collect() })
        }
    }

    let mut adapters = request.harness_adapters.clone();
    let mut harness_needs = Vec::new();
    let mut needs = Vec::new();
    for need in &request.needs {
        match need.strip_prefix("harness:") {
            Some(rest) => {
                let adapter = rest.split(['@', '>']).next().unwrap_or_default();
                adapters.insert(adapter.to_string());
                harness_needs.push((adapter.to_string(), need.clone()));
            }
            None => needs.push(need.clone()),
        }
    }
    let runtime = adapters.iter().map(|adapter| format!("harness:{adapter}")).chain(["crew:utilities".to_string()]).collect::<Vec<_>>();
    for capability in &runtime {
        if let Some(layer) = effective.provider(&selected, capability, None)? {
            if layer.tier != ImageLayerTier::AgentRuntime {
                return Err(ImageResolutionError::NotSelectable { need: capability.clone(), layer: layer.name, tier: layer.tier });
            }
            select(&mut selected, layer)?;
        }
    }
    for need in &needs {
        add_provider(&effective, &mut selected, need, None)?;
    }
    loop {
        let missing = selected
            .values()
            .flat_map(|layer| layer.requires.iter().map(move |need| (need.clone(), layer.name.clone())))
            .find(|(need, _)| !selected.values().any(|layer| provides(layer, need)));
        let Some((need, required_by)) = missing else { break };
        add_provider(&effective, &mut selected, &need, Some(&required_by))?;
    }

    for (adapter, need) in &harness_needs {
        let capability = format!("harness:{adapter}");
        let layer = selected.values().find(|layer| layer.provides.contains(&capability)).expect("harness layer selected above");
        if need != &capability && !layer.provides.iter().any(|provided| crate::capability_satisfies(provided, need)) {
            return Err(ImageResolutionError::HarnessVersion { need: need.clone(), layer: layer.name.clone() });
        }
    }

    for layer in selected.values() {
        layer.validate().map_err(|reason| ImageResolutionError::Invalid { layer: layer.name.clone(), reason })?;
        for need in &layer.requires {
            if provides(layer, need) {
                continue;
            }
            for provider in selected.values().filter(|provider| provides(provider, need)) {
                if provider.tier > layer.tier {
                    return Err(ImageResolutionError::RequiresUpward {
                        layer: layer.name.clone(),
                        need: need.clone(),
                        provider: provider.name.clone(),
                    });
                }
            }
        }
    }

    let mut layers = Vec::with_capacity(selected.len());
    for tier in [ImageLayerTier::Base, ImageLayerTier::Shared, ImageLayerTier::Project, ImageLayerTier::AgentRuntime] {
        layers.extend(topological(selected.values().filter(|layer| layer.tier == tier).collect())?);
    }
    let names = layers.iter().map(|layer| layer.name.as_str()).collect::<BTreeSet<_>>();
    let shadowed = effective.shadowed.iter().filter(|shadow| names.contains(shadow.name.as_str())).cloned().collect();
    Ok(ResolvedImage { identity: image_identity(&layers), layers, shadowed })
}

fn by_reference(
    effective: &EffectiveImageCatalogue,
    reference: &str,
    tier: ImageLayerTier,
) -> Result<ImageLayerSpec, ImageResolutionError> {
    let layer = effective.lookup(reference)?;
    if layer.tier != tier {
        return Err(ImageResolutionError::WrongTier { reference: reference.to_string(), expected: tier, actual: layer.tier });
    }
    Ok(layer.clone())
}

fn select(selected: &mut BTreeMap<String, ImageLayerSpec>, layer: ImageLayerSpec) -> Result<(), ImageResolutionError> {
    match selected.get(&layer.name) {
        Some(existing) if existing.version == layer.version => Ok(()),
        Some(existing) => Err(ImageResolutionError::DuplicateSelection {
            name: layer.name.clone(),
            versions: [existing.version.clone(), layer.version.clone()].into_iter().collect::<BTreeSet<_>>().into_iter().collect(),
        }),
        None => {
            selected.insert(layer.name.clone(), layer);
            Ok(())
        }
    }
}

fn add_provider(
    effective: &EffectiveImageCatalogue,
    selected: &mut BTreeMap<String, ImageLayerSpec>,
    need: &str,
    required_by: Option<&str>,
) -> Result<(), ImageResolutionError> {
    let Some(layer) = effective.provider(selected, need, required_by)? else { return Ok(()) };
    if matches!(layer.tier, ImageLayerTier::Base | ImageLayerTier::Project) {
        return Err(ImageResolutionError::NotSelectable { need: need.to_string(), layer: layer.name, tier: layer.tier });
    }
    select(selected, layer)
}

/// Order one tier by `requires` edges within it, breaking ties by name.
fn topological(layers: Vec<&ImageLayerSpec>) -> Result<Vec<ImageLayerSpec>, ImageResolutionError> {
    let mut blockers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for layer in &layers {
        let entry = blockers.entry(&layer.name).or_default();
        for provider in &layers {
            if provider.name != layer.name && layer.requires.iter().any(|need| provides(provider, need)) {
                entry.insert(&provider.name);
            }
        }
    }
    let mut ordered = Vec::with_capacity(layers.len());
    while !blockers.is_empty() {
        let Some(next) = blockers.iter().find(|(_, waiting)| waiting.is_empty()).map(|(name, _)| *name) else {
            return Err(ImageResolutionError::RequiresCycle { layers: blockers.keys().map(|name| name.to_string()).collect() });
        };
        blockers.remove(next);
        for waiting in blockers.values_mut() {
            waiting.remove(next);
        }
        ordered.push((*layers.iter().find(|layer| layer.name == next).expect("blocker keys are layer names")).clone());
    }
    Ok(ordered)
}

/// Dotted version order: numeric segments compare numerically, others
/// lexically; a prefix sorts first.
pub(crate) fn compare_versions(left: &str, right: &str) -> Ordering {
    let mut left_segments = left.split('.');
    let mut right_segments = right.split('.');
    loop {
        match (left_segments.next(), right_segments.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
                    (Ok(left), Ok(right)) => left.cmp(&right),
                    _ => left.cmp(right),
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}
