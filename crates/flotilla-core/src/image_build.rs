//! Demand-side image build joining. Executors resolve immutable source contents;
//! this module owns sharing, host selection and the immutable execution chain.
use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use flotilla_resources::{
    FrozenImageLayer, Host, ImageBuild, ImageBuildCapacity, ImageBuildReason, ImageBuildReservation, ImageBuildSpec, ImageComposition,
    ImageInputStability, ImageLayerParent, InputMeta, LinkImageSource, ResolvedImageInputs, ResolvedLinkInputs, ResourceError,
};
use flotilla_store::{ResourceBackend, StoreQuery};

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

    /// Look up a completed pinned chain without authoring demand. Placement
    /// can compare acquisition costs before selecting a host.
    pub async fn completed(
        &self,
        architecture: &str,
        composition: &ImageComposition,
    ) -> Result<Option<flotilla_resources::ResourceObject<ImageBuild>>, String> {
        let architecture = canonical_image_architecture(architecture);
        let candidates =
            self.backend.including_replicas::<ImageBuild>(&self.namespace).list().await.map_err(|error| error.to_string())?.items;
        let mut parent_key = None;
        let mut parent_digest: Option<String> = None;
        let mut result = None;
        let mut link_sources = BTreeMap::new();
        let plan = composition
            .layers
            .iter()
            .filter(|layer| layer.spec.link.is_some())
            .map(|layer| (layer, true))
            .chain(composition.layers.iter().map(|layer| (layer, false)));
        for (layer, producer) in plan {
            let source = self.inputs.resolve(layer, architecture).await?;
            if source.architecture != architecture {
                return Err("image input resolver changed target architecture".into());
            }
            if source.stability != ImageInputStability::Pinned {
                return Ok(None);
            }
            let link_source = if !producer && layer.spec.link.is_some() {
                Some(link_sources.get(&layer.name).cloned().ok_or("link producer is not built")?)
            } else {
                None
            };
            let digest = if producer {
                String::new()
            } else {
                match (&parent_digest, &layer.spec.parent) {
                    (Some(digest), _) => digest.clone(),
                    (None, ImageLayerParent::Image(image)) => {
                        image.rsplit_once('@').map_or(image.as_str(), |(_, digest)| digest).to_string()
                    }
                    _ => return Ok(None),
                }
            };
            let inputs = ResolvedImageInputs::builder()
                .parent_digest(digest)
                .maybe_parent_recipe_key(if producer { None } else { parent_key.clone() })
                .maybe_link(if producer {
                    layer.spec.link.clone().map(|definition| ResolvedLinkInputs { definition, requires: layer.spec.requires.clone() })
                } else {
                    None
                })
                .link_recipe_keys(link_source.as_ref().map(|source: &LinkImageSource| vec![source.recipe_key.clone()]).unwrap_or_default())
                .architecture(source.architecture.clone())
                .content_hashes(source.content_hashes.clone())
                .args(source.args.clone())
                .pins(source.pins.clone())
                .stability(source.stability)
                .build();
            let key = inputs.recipe_key()?;
            let Some(found) = candidates.iter().map(|source| &source.object).find(|build| {
                build.spec.recipe_key == key
                    && build.spec.inputs == inputs
                    && build.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ImageBuildPhase::Built)
            }) else {
                return Ok(None);
            };
            let found = flotilla_store::read_image_build(&self.backend, &self.namespace, &found.metadata.name)
                .await
                .map_err(|error| error.to_string())?;
            let Some(status) = found.status.as_ref().filter(|status| status.phase == flotilla_resources::ImageBuildPhase::Built) else {
                return Ok(None);
            };
            if producer {
                link_sources.insert(
                    layer.name.clone(),
                    LinkImageSource {
                        build_ref: found.metadata.name.clone(),
                        recipe_key: key,
                        identity: status.identity.clone().ok_or("built link has no identity")?,
                    },
                );
                continue;
            }
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
            let mut producers = BTreeMap::new();
            for _ in 0..composition.layers.len() {
                chain.push(completed.metadata.name.clone());
                if let Some(link) = &completed.spec.link_source {
                    producers.insert(completed.spec.layer.name.clone(), link.build_ref.clone());
                }
                let Some(parent) = &completed.spec.parent_build_ref else {
                    break;
                };
                completed =
                    flotilla_store::read_image_build(&self.backend, &self.namespace, parent).await.map_err(|error| error.to_string())?;
            }
            chain.reverse();
            if chain.len() == composition.layers.len() {
                let mut refs = producers.into_values().collect::<Vec<_>>();
                refs.extend(chain);
                if refs.len() == composition.build_stage_count() {
                    return Ok(refs);
                }
            }
        }
        let capacity = |host: &flotilla_resources::ResourceObject<Host>| -> Result<Option<ImageBuildCapacity>, String> {
            Ok(host.spec.image_build_capacity.clone())
        };
        let default_reservation = ImageBuildReservation { cpu: 1, disk_bytes: 1024 * 1024 * 1024 };
        let selected = match capacity(placement)? {
            None => Some((placement_host.to_string(), default_reservation.clone())),
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
        // Pinned producers have no placement parent: select one canonical
        // eligible builder per architecture, so concurrent demands from different
        // hosts author the same execution even before replication catches up.
        let producer_builder = hosts
            .iter()
            .filter(|host| {
                host.metadata.deletion_timestamp.is_none()
                    && host.status.as_ref().is_none_or(|status| {
                        status.ready
                            && status
                                .description
                                .as_ref()
                                .and_then(|summary| summary.system.arch.as_deref())
                                .is_none_or(|actual| canonical_image_architecture(actual) == architecture)
                    })
            })
            .filter_map(|host| match &host.spec.image_build_capacity {
                Some(ImageBuildCapacity::Builder { architecture: target, slots, reservation })
                    if canonical_image_architecture(target) == architecture && *slots > 0 =>
                {
                    Some((host.metadata.name.clone(), reservation.clone()))
                }
                None if host.status.as_ref().is_some_and(|status| {
                    status.capabilities.get("docker").and_then(serde_json::Value::as_bool) == Some(true)
                        && status
                            .description
                            .as_ref()
                            .and_then(|summary| summary.system.arch.as_deref())
                            .is_some_and(|target| canonical_image_architecture(target) == architecture)
                }) =>
                {
                    Some((host.metadata.name.clone(), default_reservation.clone()))
                }
                _ => None,
            })
            .min_by_key(|(host, _)| host.clone())
            .unwrap_or_else(|| (host_ref.clone(), reservation.clone()));
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
        // Pinned layers share one replica-inclusive inventory per join; builds
        // created by earlier layers are not yet Built, so they cannot match.
        let shareable_builds = tokio::sync::OnceCell::new();
        let mut link_sources = BTreeMap::new();
        let producers = composition
            .layers
            .iter()
            .zip(resolved.iter())
            .filter(|(layer, _)| layer.spec.link.is_some())
            .map(|(layer, source)| (layer, source, true));
        let ordinary = composition.layers.iter().zip(resolved.iter()).map(|(layer, source)| (layer, source, false));
        for (index, (layer, source, producer)) in producers.chain(ordinary).enumerate() {
            let (stage_host, stage_reservation) = if producer && source.stability == ImageInputStability::Pinned {
                (&producer_builder.0, &producer_builder.1)
            } else {
                (&host_ref, &reservation)
            };
            let link_source = if !producer && layer.spec.link.is_some() {
                let Some(source) = link_sources.get(&layer.name).cloned() else {
                    break;
                };
                Some(source)
            } else {
                None
            };
            let digest = if producer {
                String::new()
            } else {
                match (&parent_digest, &layer.spec.parent) {
                    (Some(digest), _) => digest.clone(),
                    (None, ImageLayerParent::Image(image)) => {
                        image.rsplit_once('@').map_or(image.as_str(), |(_, digest)| digest).to_string()
                    }
                    _ => return Err("first image stage requires a pinned base image".into()),
                }
            };
            let inputs = ResolvedImageInputs::builder()
                .parent_digest(digest)
                .maybe_parent_recipe_key(if producer { None } else { parent_key.clone() })
                .maybe_link(if producer {
                    layer.spec.link.clone().map(|definition| ResolvedLinkInputs { definition, requires: layer.spec.requires.clone() })
                } else {
                    None
                })
                .link_recipe_keys(link_source.as_ref().map(|source: &LinkImageSource| vec![source.recipe_key.clone()]).unwrap_or_default())
                .architecture(source.architecture.clone())
                .content_hashes(source.content_hashes.clone())
                .args(source.args.clone())
                .pins(source.pins.clone())
                .stability(source.stability)
                .build();
            let old = match composition.build_refs.get(index) {
                Some(name) => match flotilla_store::read_image_build(&self.backend, &self.namespace, name).await {
                    Ok(old) if old.spec.inputs == inputs && old.spec.host_ref == *stage_host => Some(old),
                    Ok(_) | Err(ResourceError::NotFound { .. }) => None,
                    Err(error) => return Err(error.to_string()),
                },
                None => None,
            };
            let nonce = old.and_then(|old| old.spec.execution_nonce).or_else(|| {
                (inputs.stability == flotilla_resources::ImageInputStability::Unpinned)
                    .then(|| format!("{stage_host}:{}", uuid::Uuid::new_v4()))
            });
            let recipe_key = inputs.execution_key(nonce.as_deref())?;
            // A pinned recipe's built execution can serve every same-architecture
            // host. Unpinned execution keys remain scoped to their original demand.
            let shared = if inputs.stability == ImageInputStability::Pinned {
                shareable_builds
                    .get_or_try_init(|| async { self.backend.including_replicas::<ImageBuild>(&self.namespace).list().await })
                    .await
                    .map_err(|error| error.to_string())?
                    .items
                    .iter()
                    .map(|source| &source.object)
                    .filter(|build| {
                        build.spec.recipe_key == recipe_key
                            && build.spec.inputs == inputs
                            && (build.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ImageBuildPhase::Built)
                                || (producer
                                    && build.status.as_ref().is_none_or(|status| {
                                        matches!(
                                            status.phase,
                                            flotilla_resources::ImageBuildPhase::Queued | flotilla_resources::ImageBuildPhase::Building
                                        )
                                    })))
                    })
                    .min_by_key(|build| (build.spec.host_ref != placement_host, build.metadata.name.clone()))
                    .cloned()
            } else {
                None
            };
            let name = shared
                .as_ref()
                .map(|build| build.metadata.name.clone())
                .unwrap_or_else(|| format!("image-build-{}-{}", recipe_key.trim_start_matches("sha256:"), stage_host));
            // The layer name is a declared field: read this layer's history only.
            let old_inputs = builds
                .query_declared(&StoreQuery {
                    fields: BTreeMap::from([("spec.layer.name".into(), layer.name.clone())]),
                    ..StoreQuery::default()
                })
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .map(|old| old.object)
                .filter(|old| {
                    old.spec.layer.name == layer.name
                        && old.spec.inputs.architecture == inputs.architecture
                        && old.spec.is_link_producer() == producer
                        && old.spec.link_source.is_some() == link_source.is_some()
                        && old.spec.host_ref == *stage_host
                        && old.spec.recipe_key != recipe_key
                })
                .max_by_key(|old| old.metadata.creation_timestamp)
                .map(|old| old.spec.reason.new_inputs)
                .unwrap_or_default();
            let description = if producer {
                "link producer demand"
            } else if link_source.is_some() {
                "link splice demand"
            } else {
                "composition demand"
            };
            let spec = ImageBuildSpec::builder()
                .recipe_key(recipe_key.clone())
                .maybe_execution_nonce(nonce)
                .inputs(inputs.clone())
                .layer(layer.clone())
                .host_ref(stage_host.clone())
                .reservation(stage_reservation.clone())
                .maybe_parent_build_ref(if producer { None } else { parent.clone() })
                .maybe_link_source(link_source)
                .attempt(0)
                .reason(ImageBuildReason {
                    description: description.into(),
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
            joined.push(name.clone());
            let mut execution =
                flotilla_store::read_image_build(&self.backend, &self.namespace, &name).await.map_err(|error| error.to_string())?;
            for _ in 0..3 {
                match flotilla_store::read_image_build(&self.backend, &self.namespace, &format!("{}-retry", execution.metadata.name)).await
                {
                    Ok(next) => execution = next,
                    Err(ResourceError::NotFound { .. }) => break,
                    Err(error) => return Err(error.to_string()),
                }
            }
            let status = execution.status.as_ref().filter(|status| status.phase == flotilla_resources::ImageBuildPhase::Built);
            if producer {
                if let Some(status) = status {
                    link_sources.insert(
                        layer.name.clone(),
                        LinkImageSource {
                            build_ref: execution.metadata.name.clone(),
                            recipe_key,
                            identity: status.identity.clone().ok_or("built link producer has no identity")?,
                        },
                    );
                }
                continue;
            }
            parent = Some(name.clone());
            parent_key = Some(recipe_key);
            let Some(status) = status else {
                break;
            };
            parent_digest = Some(status.identity.as_ref().ok_or("built parent has no identity")?.local_image_id.clone());
        }
        Ok(joined)
    }
}
