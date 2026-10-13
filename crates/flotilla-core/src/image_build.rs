//! Demand-side composed images (ADR 0062). Admission resolves each vessel's
//! ordered layer list and get-or-creates the one ImageBuild for that identity
//! and the placement architecture. Executing builds belongs to the build
//! service; this module never runs one.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    canonical_host_id, image_build_name, CapabilityNeed, CrewSource, FleetDesignation, Host, ImageBuild, ImageBuildCapacity,
    ImageBuildReservation, ImageBuildSpec, ImageCatalogue, ImageDelivery, ImageLayer, ImageRequest, ImageSelection, InputMeta,
    ResolvedImage, ResourceError, ResourceObject, ResourceProvenance, VesselRequirement, FLEET_DESIGNATION_NAME,
};
use flotilla_store::{ResourceBackend, StoreQuery};

pub fn canonical_image_architecture(architecture: &str) -> &str {
    match architecture {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Builds on hosts that predate `image_build_capacity` reserve this much.
const DEFAULT_BUILD_RESERVATION: ImageBuildReservation = ImageBuildReservation { cpu: 1, disk_bytes: 1024 * 1024 * 1024 };

/// The inputs of one vessel's composition: the project's declared selection,
/// the installation defaults, the vessel's image needs (its leg needs, its
/// crews' image needs, which already carry convoy `--need`) and the harness
/// adapters its crew roles resolve to.
pub fn vessel_image_request(
    project: &str,
    selection: Option<&ImageSelection>,
    defaults: Option<&ImageSelection>,
    vessel: &VesselRequirement,
) -> Result<ImageRequest, String> {
    let needs = vessel
        .needs
        .iter()
        .chain(vessel.crew.iter().flat_map(|crew| &crew.needs))
        .filter(|need| need.is_image_need())
        .map(CapabilityNeed::capability_string)
        .collect::<BTreeSet<_>>();
    let harness_adapters = flotilla_agent::adapter::required_agent_adapters(
        vessel.crew.iter().filter(|crew| matches!(crew.source, CrewSource::Agent { .. })),
    )?;
    Ok(ImageRequest::builder()
        .project(project.to_string())
        .maybe_selection(selection.cloned())
        .maybe_defaults(defaults.cloned())
        .needs(needs)
        .harness_adapters(harness_adapters)
        .build())
}

/// Every current layer definition visible to this namespace, replicas
/// included. Retired records, records being deleted and definitions whose
/// replicas disagree are not resolvable.
pub async fn read_image_catalogue(backend: &ResourceBackend, namespace: &str) -> Result<ImageCatalogue, String> {
    let layers = backend
        .query_declared::<ImageLayer>(
            namespace,
            &StoreQuery { include_replicas: true, suppress_self_origin: false, merged_definitions: true, ..Default::default() },
        )
        .await
        .map_err(|error| error.to_string())?
        .items
        .into_iter()
        .map(|source| source.object)
        .filter(|layer| {
            layer.metadata.deletion_timestamp.is_none() && layer.metadata.merge.as_ref().is_none_or(|merge| merge.conflicts.is_empty())
        })
        .collect::<Vec<_>>();
    Ok(ImageCatalogue::from_specs(layers.iter().map(|layer| &layer.spec)))
}

/// One record per host name, preferring the authoritative local copy.
async fn hosts(backend: &ResourceBackend, namespace: &str) -> Result<Vec<ResourceObject<Host>>, String> {
    let mut resolved = BTreeMap::<String, (bool, ResourceObject<Host>)>::new();
    for source in backend.including_replicas::<Host>(namespace).list().await.map_err(|error| error.to_string())?.items {
        let local = matches!(source.provenance, ResourceProvenance::Local);
        let name = source.object.metadata.name.clone();
        if resolved.get(&name).is_none_or(|(existing_local, _)| local && !existing_local) {
            resolved.insert(name, (local, source.object));
        }
    }
    Ok(resolved.into_values().map(|(_, host)| host).collect())
}

/// The canonical architecture a host reports, if observed.
pub fn host_architecture(host: &ResourceObject<Host>) -> Option<String> {
    host.status
        .as_ref()
        .and_then(|status| status.description.as_ref())
        .and_then(|description| description.system.arch.as_deref())
        .map(|arch| canonical_image_architecture(arch).to_string())
}

/// The ImageBuild name a composed identity uses on `placement_host`'s architecture.
pub async fn placement_image_build_name(
    backend: &ResourceBackend,
    namespace: &str,
    identity: &str,
    placement_host: &str,
) -> Result<String, String> {
    let hosts = hosts(backend, namespace).await?;
    let (_, architecture) = placement_architecture(&hosts, placement_host)?;
    Ok(image_build_name(identity, &architecture))
}

fn placement_architecture<'a>(
    hosts: &'a [ResourceObject<Host>],
    placement_host: &str,
) -> Result<(&'a ResourceObject<Host>, String), String> {
    let canonical = canonical_host_id(hosts.iter(), placement_host)?.map_or_else(|| placement_host.to_string(), |host| host.to_string());
    let host = hosts
        .iter()
        .find(|host| host.metadata.name == canonical)
        .ok_or_else(|| format!("image placement host `{placement_host}` is unknown"))?;
    let architecture = host_architecture(host).ok_or_else(|| format!("image placement host `{placement_host}` architecture is unknown"))?;
    Ok((host, architecture))
}

fn builder_slot(capacity: Option<&ImageBuildCapacity>, architecture: &str) -> Option<ImageBuildReservation> {
    match capacity {
        Some(ImageBuildCapacity::Builder { architecture: builds, slots, reservation })
            if *slots > 0 && canonical_image_architecture(builds) == architecture =>
        {
            Some(reservation.clone())
        }
        _ => None,
    }
}

/// The deterministic builder and delivery for a new build, so concurrent
/// admissions on different hosts converge on one ImageBuild spec: with a
/// declared image cache, the lowest-named same-architecture host with builder
/// slots pushes to the registry; without one, the placement host builds and
/// keeps the image locally.
pub fn choose_builder(
    hosts: &[ResourceObject<Host>],
    placement: &ResourceObject<Host>,
    architecture: &str,
    image_cache: Option<&flotilla_resources::ImageCacheBinding>,
) -> Result<(String, ImageDelivery, ImageBuildReservation), String> {
    match image_cache {
        Some(cache) => {
            let mut builders = hosts
                .iter()
                .filter(|host| host_architecture(host).is_none_or(|actual| actual == architecture))
                .filter_map(|host| {
                    builder_slot(host.spec.image_build_capacity.as_ref(), architecture)
                        .map(|reservation| (host.metadata.name.clone(), reservation))
                })
                .collect::<Vec<_>>();
            builders.sort_by(|left, right| left.0.cmp(&right.0));
            let (host, reservation) =
                builders.into_iter().next().ok_or_else(|| format!("no host can build images for architecture {architecture}"))?;
            Ok((host, ImageDelivery::Registry { repository: cache.repository.clone() }, reservation))
        }
        None => {
            let reservation = match &placement.spec.image_build_capacity {
                // An absent declaration preserves placement-host builds.
                None => DEFAULT_BUILD_RESERVATION,
                declared => builder_slot(declared.as_ref(), architecture).ok_or_else(|| {
                    format!(
                        "placement host `{}` cannot build {architecture} images and no fleet image cache is declared",
                        placement.metadata.name
                    )
                })?,
            };
            Ok((placement.metadata.name.clone(), ImageDelivery::Local, reservation))
        }
    }
}

/// Get or create the ImageBuild for `resolved` on the placement host's
/// architecture and return its name. An existing build with that name, local
/// or replicated, is reused whatever its builder.
pub async fn ensure_image_build(
    backend: &ResourceBackend,
    namespace: &str,
    resolved: &ResolvedImage,
    placement_host: &str,
) -> Result<String, String> {
    let hosts = hosts(backend, namespace).await?;
    let (placement, architecture) = placement_architecture(&hosts, placement_host)?;
    let name = image_build_name(&resolved.identity, &architecture);
    match flotilla_store::read_image_build(backend, namespace, &name).await {
        Ok(_) => return Ok(name),
        Err(ResourceError::NotFound { .. }) => {}
        Err(error) => return Err(error.to_string()),
    }
    let image_cache = match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(fleet) => fleet.spec.image_cache,
        Err(ResourceError::NotFound { .. }) => None,
        Err(error) => return Err(error.to_string()),
    };
    let (host_ref, delivery, reservation) = choose_builder(&hosts, placement, &architecture, image_cache.as_ref())?;
    let spec = ImageBuildSpec::builder()
        .identity(resolved.identity.clone())
        .architecture(architecture)
        .layers(resolved.layers.clone())
        .host_ref(host_ref)
        .delivery(delivery)
        .reservation(reservation)
        .build();
    match backend.using::<ImageBuild>(namespace).create(&InputMeta::builder().name(name.clone()).build(), &spec.into()).await {
        // A concurrent admission created the same name first; the name fixes
        // identity and architecture, so that build serves this vessel too.
        Ok(_) | Err(ResourceError::Conflict { .. }) => Ok(name),
        Err(error) => Err(format!("create ImageBuild {name}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{HostSpec, HostStatus, ImageCacheBinding};

    use super::*;

    fn host(name: &str, arch: Option<&str>, capacity: Option<ImageBuildCapacity>) -> ResourceObject<Host> {
        let mut status = HostStatus::default();
        if let Some(arch) = arch {
            status.description = Some(
                flotilla_protocol::HostSummary::builder()
                    .environment_id(flotilla_protocol::EnvironmentId::new(name))
                    .node(flotilla_protocol::NodeInfo::new(flotilla_protocol::NodeId::new(name), name.to_string()))
                    .system(flotilla_protocol::SystemInfo { arch: Some(arch.into()), ..Default::default() })
                    .build(),
            );
        }
        ResourceObject {
            metadata: flotilla_resources::ObjectMeta {
                name: name.into(),
                namespace: "flotilla".into(),
                resource_version: "1".into(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: chrono::Utc::now(),
                merge: None,
            },
            spec: HostSpec { image_build_capacity: capacity, ..HostSpec::default() },
            status: Some(status),
        }
    }

    fn builder(arch: &str, slots: u32) -> Option<ImageBuildCapacity> {
        Some(ImageBuildCapacity::Builder {
            architecture: arch.into(),
            slots,
            reservation: ImageBuildReservation { cpu: 2, disk_bytes: 4096 },
        })
    }

    fn cache() -> ImageCacheBinding {
        ImageCacheBinding {
            repository: "registry.test/flotilla/crew-images".into(),
            pull_credential: "pull".into(),
            push_credential: "push".into(),
        }
    }

    // Design §3: with a registry, every admitter picks the lowest-named
    // same-architecture builder with free slots, whatever host it places on;
    // without one, the placement host builds locally (an absent capacity
    // declaration keeps the historical default reservation, an explicit
    // refusal or zero slots refuses).
    #[test]
    fn builder_choice_is_deterministic() {
        let hosts = vec![
            host("udder", Some("x86_64"), builder("amd64", 1)),
            host("feta", Some("x86_64"), builder("amd64", 2)),
            host("arm", Some("aarch64"), builder("arm64", 4)),
            host("busy", Some("x86_64"), builder("amd64", 0)),
            host("kiwi", Some("x86_64"), Some(ImageBuildCapacity::None)),
            host("plain", Some("x86_64"), None),
        ];
        for placement in &hosts[..2] {
            let (builder, delivery, reservation) = choose_builder(&hosts, placement, "amd64", Some(&cache())).expect("builder");
            assert_eq!(builder, "feta");
            assert_eq!(delivery, ImageDelivery::Registry { repository: cache().repository });
            assert_eq!(reservation.cpu, 2);
        }
        assert!(choose_builder(&hosts, &hosts[0], "riscv64", Some(&cache())).expect_err("no builder").contains("riscv64"));
        let local = |name: &str| choose_builder(&hosts, hosts.iter().find(|host| host.metadata.name == name).expect("host"), "amd64", None);
        assert_eq!(local("udder").expect("udder").0, "udder");
        assert_eq!(local("udder").expect("udder").1, ImageDelivery::Local);
        assert_eq!(local("plain").expect("plain").2, DEFAULT_BUILD_RESERVATION);
        assert!(local("kiwi").expect_err("refused").contains("no fleet image cache"));
        assert!(local("busy").is_err());
    }

    // Leg needs, crew image needs and the crews' harness adapters form the
    // request; host needs (hardware) never reach the image.
    #[test]
    fn vessel_request_unites_leg_and_crew_image_needs() {
        let crew = flotilla_resources::CrewSpec::builder()
            .role("coder".into())
            .source(CrewSource::Agent {
                selector: flotilla_resources::Selector { capability: "code".into(), adapter: Some("codex".into()), model: None },
                prompt: None,
                brief_template: None,
            })
            .needs(BTreeSet::from([CapabilityNeed::Gpu, "toolchain:zig".parse().expect("need")]))
            .build();
        let vessel = VesselRequirement::builder()
            .name("work".into())
            .crew(vec![crew])
            .needs(BTreeSet::from(["display:x11".parse().expect("need")]))
            .build();
        let selection = ImageSelection { base: None, layers: vec!["rust".into()] };
        let request = vessel_image_request("flotilla", Some(&selection), None, &vessel).expect("request");
        assert_eq!(request.project, "flotilla");
        assert_eq!(request.selection, Some(selection));
        assert_eq!(request.needs, BTreeSet::from(["display:x11".to_string(), "toolchain:zig".to_string()]));
        assert_eq!(request.harness_adapters, BTreeSet::from(["codex".to_string()]));
    }
}
