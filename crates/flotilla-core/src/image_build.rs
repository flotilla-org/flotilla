//! Demand-side image build joining. Executors resolve immutable source contents;
//! this module owns sharing, host selection and the immutable execution chain.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_resources::{
    FrozenImageLayer, Host, ImageBuild, ImageBuildCapacity, ImageBuildReason, ImageBuildReservation, ImageBuildSpec, ImageComposition,
    ImageInputStability, ImageLayerParent, InputMeta, ResolvedImageInputs, ResourceBackend, ResourceError,
};

pub fn canonical_image_architecture(architecture: &str) -> &str {
    match architecture {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Source evidence is distinct from an execution's resolved parent identity.
/// It is safe to acquire this before the preceding stage exists.
#[derive(Debug, Clone, bon::Builder)]
pub struct ImageBuildSourceInputs {
    pub architecture: String,
    pub content_hashes: Vec<String>,
    #[builder(default)]
    pub args: BTreeMap<String, String>,
    #[builder(default)]
    pub pins: BTreeMap<String, String>,
    pub stability: ImageInputStability,
}

/// The source/process seam: admission reads pinned inputs but never runs builds.
#[async_trait]
pub trait ImageBuildInputResolver: Send + Sync {
    async fn resolve(&self, layer: &FrozenImageLayer, architecture: &str) -> Result<ImageBuildSourceInputs, String>;
}

pub struct ImageBuildAdmission {
    backend: ResourceBackend,
    namespace: String,
    inputs: Arc<dyn ImageBuildInputResolver>,
}

impl ImageBuildAdmission {
    pub fn new(backend: ResourceBackend, namespace: &str, inputs: Arc<dyn ImageBuildInputResolver>) -> Self {
        Self { backend, namespace: namespace.into(), inputs }
    }

    async fn completed_candidates(&self) -> Result<Vec<flotilla_resources::ResourceObject<ImageBuild>>, String> {
        Ok(flotilla_resources::list_image_builds(&self.backend, &self.namespace)
            .await
            .map_err(|error| error.to_string())?
            .into_values()
            .collect())
    }

    /// Look up a completed pinned chain without authoring demand. Placement
    /// can compare acquisition costs before selecting a host.
    pub async fn completed(
        &self,
        architecture: &str,
        composition: &ImageComposition,
    ) -> Result<Option<flotilla_resources::ResourceObject<ImageBuild>>, String> {
        let architecture = canonical_image_architecture(architecture);
        let candidates = self.completed_candidates().await?;
        let mut parent_key = None;
        let mut parent_digest: Option<String> = None;
        let mut result = None;
        for layer in &composition.layers {
            let source = self.inputs.resolve(layer, architecture).await?;
            if source.architecture != architecture {
                return Err("image input resolver changed target architecture".into());
            }
            if source.stability != ImageInputStability::Pinned {
                return Ok(None);
            }
            let digest = match (&parent_digest, &layer.spec.parent) {
                (Some(digest), _) => digest.clone(),
                (None, ImageLayerParent::Image(image)) => image.rsplit_once('@').map_or(image.as_str(), |(_, digest)| digest).to_string(),
                _ => return Ok(None),
            };
            let inputs = ResolvedImageInputs::builder()
                .parent_digest(digest)
                .maybe_parent_recipe_key(parent_key.clone())
                .architecture(source.architecture)
                .content_hashes(source.content_hashes)
                .args(source.args)
                .pins(source.pins)
                .stability(source.stability)
                .build();
            let key = inputs.recipe_key()?;
            let Some(found) = candidates.iter().find(|build| {
                build.spec.recipe_key == key
                    && build.spec.inputs == inputs
                    && build
                        .status
                        .as_ref()
                        .is_some_and(|status| status.phase == flotilla_resources::ImageBuildPhase::Built && !status.availability.retired)
            }) else {
                return Ok(None);
            };
            let found = flotilla_resources::read_image_build(&self.backend, &self.namespace, &found.metadata.name)
                .await
                .map_err(|error| error.to_string())?;
            let Some(status) = found
                .status
                .as_ref()
                .filter(|status| status.phase == flotilla_resources::ImageBuildPhase::Built && !status.availability.retired)
            else {
                return Ok(None);
            };
            parent_digest = Some(status.identity.as_ref().ok_or("built image has no identity")?.local_image_id.clone());
            parent_key = Some(key);
            result = Some(found);
        }
        Ok(result)
    }

    pub async fn join(&self, placement_host: &str, composition: &ImageComposition) -> Result<Vec<String>, String> {
        if composition.layers.is_empty() {
            return Err("image build requires a composition stage".into());
        }
        let hosts = self
            .backend
            .including_replicas::<Host>(&self.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|source| source.object)
            .collect::<Vec<_>>();
        let placement = hosts.iter().find(|host| host.metadata.name == placement_host).ok_or("image placement host is unknown")?;
        let architecture = placement
            .status
            .as_ref()
            .and_then(|status| status.description.as_ref())
            .and_then(|summary| summary.system.arch.as_deref())
            .ok_or("image placement host architecture is unknown")?;
        let architecture = canonical_image_architecture(architecture);
        if let Some(mut completed) = self.completed(architecture, composition).await? {
            let mut chain = Vec::new();
            for _ in 0..composition.layers.len() {
                chain.push(completed.metadata.name.clone());
                let Some(parent) = &completed.spec.parent_build_ref else {
                    break;
                };
                completed = flotilla_resources::read_image_build(&self.backend, &self.namespace, parent)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            chain.reverse();
            if chain.len() == composition.layers.len() {
                return Ok(chain);
            }
        }
        let capacity = |host: &flotilla_resources::ResourceObject<Host>| -> Result<Option<ImageBuildCapacity>, String> {
            Ok(host.spec.image_build_capacity.clone())
        };
        let default_reservation = ImageBuildReservation { cpu: 1, disk_bytes: 1024 * 1024 * 1024 };
        let selected = match capacity(placement)? {
            None => Some((placement_host.to_string(), default_reservation)),
            Some(ImageBuildCapacity::Builder { architecture: build_arch, slots, reservation })
                if canonical_image_architecture(&build_arch) == architecture && slots > 0 =>
            {
                Some((placement_host.to_string(), reservation))
            }
            _ => None,
        };
        let mut selected = selected;
        if selected.is_none() {
            for host in &hosts {
                if host
                    .status
                    .as_ref()
                    .and_then(|status| status.description.as_ref())
                    .and_then(|description| description.system.arch.as_deref())
                    .is_some_and(|actual| canonical_image_architecture(actual) != architecture)
                {
                    continue;
                }
                if let Some(ImageBuildCapacity::Builder { architecture: build_arch, slots, reservation }) = capacity(host)? {
                    if canonical_image_architecture(&build_arch) == architecture && slots > 0 {
                        selected = Some((host.metadata.name.clone(), reservation));
                        break;
                    }
                }
            }
        }
        let (host_ref, reservation) = selected.ok_or_else(|| format!("no host can build images for architecture {architecture}"))?;
        let mut resolved = Vec::new();
        for layer in &composition.layers {
            let input = self.inputs.resolve(layer, architecture).await?;
            if input.architecture != architecture {
                return Err("image input resolver changed target architecture".into());
            }
            resolved.push(input);
        }
        let builds = self.backend.using::<ImageBuild>(&self.namespace);
        let mut parent: Option<String> = None;
        let mut joined = Vec::new();
        let mut parent_key = None;
        let mut parent_digest: Option<String> = None;
        for (index, (layer, source)) in composition.layers.iter().zip(resolved).enumerate() {
            let digest = match (&parent_digest, &layer.spec.parent) {
                (Some(digest), _) => digest.clone(),
                (None, ImageLayerParent::Image(image)) => image.rsplit_once('@').map_or(image.as_str(), |(_, digest)| digest).to_string(),
                _ => return Err("first image stage requires a pinned base image".into()),
            };
            let inputs = ResolvedImageInputs::builder()
                .parent_digest(digest)
                .maybe_parent_recipe_key(parent_key.clone())
                .architecture(source.architecture)
                .content_hashes(source.content_hashes)
                .args(source.args)
                .pins(source.pins)
                .stability(source.stability)
                .build();
            let old = match composition.build_refs.get(index) {
                Some(name) => match flotilla_resources::read_image_build(&self.backend, &self.namespace, name).await {
                    Ok(old) if old.spec.inputs == inputs && old.spec.host_ref == host_ref => Some(old),
                    Ok(_) | Err(ResourceError::NotFound { .. }) => None,
                    Err(error) => return Err(error.to_string()),
                },
                None => None,
            };
            let nonce = old.and_then(|old| old.spec.execution_nonce).or_else(|| {
                (inputs.stability == flotilla_resources::ImageInputStability::Unpinned)
                    .then(|| format!("{host_ref}:{}", uuid::Uuid::new_v4()))
            });
            let recipe_key = inputs.execution_key(nonce.as_deref())?;
            // A pinned recipe's built execution can serve every same-architecture
            // host. Unpinned execution keys remain scoped to their original demand.
            let shared = if inputs.stability == ImageInputStability::Pinned {
                self.completed_candidates()
                    .await?
                    .into_iter()
                    .filter(|build| {
                        build.spec.recipe_key == recipe_key
                            && build.spec.inputs == inputs
                            && build.status.as_ref().is_some_and(|status| {
                                status.phase == flotilla_resources::ImageBuildPhase::Built && !status.availability.retired
                            })
                    })
                    .min_by_key(|build| (build.spec.host_ref != placement_host, build.metadata.name.clone()))
            } else {
                None
            };
            let mut name = shared
                .as_ref()
                .map(|build| build.metadata.name.clone())
                .unwrap_or_else(|| format!("image-build-{}-{}", recipe_key.trim_start_matches("sha256:"), host_ref));
            if shared.is_none() {
                // Retired evidence remains immutable. A fresh execution gets a
                // deterministic successor name; concurrent demands still join.
                loop {
                    match flotilla_resources::read_image_build(&self.backend, &self.namespace, &name).await {
                        Ok(build) if build.status.as_ref().is_some_and(|status| status.availability.retired) => name.push_str("-rebuilt"),
                        Ok(_) | Err(ResourceError::NotFound { .. }) => break,
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
            let old_inputs = builds
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|old| {
                    old.spec.layer.name == layer.name
                        && old.spec.inputs.architecture == inputs.architecture
                        && old.spec.host_ref == host_ref
                        && old.spec.recipe_key != recipe_key
                })
                .max_by_key(|old| old.metadata.creation_timestamp)
                .map(|old| old.spec.reason.new_inputs)
                .unwrap_or_default();
            let spec = ImageBuildSpec::builder()
                .recipe_key(recipe_key.clone())
                .maybe_execution_nonce(nonce)
                .inputs(inputs.clone())
                .layer(layer.clone())
                .host_ref(host_ref.clone())
                .reservation(reservation.clone())
                .maybe_parent_build_ref(parent.clone())
                .attempt(0)
                .reason(ImageBuildReason {
                    description: "composition demand".into(),
                    old_inputs,
                    new_inputs: BTreeMap::from([
                        ("architecture".into(), inputs.architecture.clone()),
                        ("parent_digest".into(), inputs.parent_digest.clone()),
                        ("source_revision".into(), layer.spec.revision.clone()),
                        ("content_hashes".into(), inputs.content_hashes.join(",")),
                        ("args".into(), serde_json::to_string(&inputs.args).map_err(|error| error.to_string())?),
                        ("pins".into(), serde_json::to_string(&inputs.pins).map_err(|error| error.to_string())?),
                        ("parent_recipe_key".into(), inputs.parent_recipe_key.clone().unwrap_or_default()),
                    ]),
                })
                .build();
            if shared.is_none() {
                match builds.create(&InputMeta::builder().name(name.clone()).build(), &spec).await {
                    Ok(_) => {}
                    Err(ResourceError::Conflict { .. }) => {
                        let existing = builds.get(&name).await.map_err(|error| error.to_string())?;
                        if existing.spec.inputs != spec.inputs || existing.spec.recipe_key != spec.recipe_key {
                            return Err(format!("image build {name} has different immutable inputs"));
                        }
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            parent = Some(name.clone());
            parent_key = Some(recipe_key);
            joined.push(name.clone());
            let mut execution =
                flotilla_resources::read_image_build(&self.backend, &self.namespace, &name).await.map_err(|error| error.to_string())?;
            for _ in 0..3 {
                match flotilla_resources::read_image_build(&self.backend, &self.namespace, &format!("{}-retry", execution.metadata.name))
                    .await
                {
                    Ok(next) => execution = next,
                    Err(ResourceError::NotFound { .. }) => break,
                    Err(error) => return Err(error.to_string()),
                }
            }
            let Some(status) = execution
                .status
                .as_ref()
                .filter(|status| status.phase == flotilla_resources::ImageBuildPhase::Built && !status.availability.retired)
            else {
                break;
            };
            parent_digest = Some(status.identity.as_ref().ok_or("built parent has no identity")?.local_image_id.clone());
        }
        Ok(joined)
    }
}
