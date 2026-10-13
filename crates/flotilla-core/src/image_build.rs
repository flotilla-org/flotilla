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

/// The ImageBuild name a composed identity uses on a placement: the
/// registry build when a fleet image cache is declared, otherwise the
/// placement host's own local build.
pub fn expected_build_name(
    identity: &str,
    architecture: &str,
    image_cache: Option<&flotilla_resources::ImageCacheBinding>,
    placement_host: &str,
) -> String {
    match image_cache {
        Some(cache) => image_build_name(identity, architecture, &ImageDelivery::Registry { repository: cache.repository.clone() }, ""),
        None => image_build_name(identity, architecture, &ImageDelivery::Local, placement_host),
    }
}

/// The fleet image cache, if declared.
pub async fn fleet_image_cache(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<Option<flotilla_resources::ImageCacheBinding>, String> {
    match backend.definitions::<FleetDesignation>(namespace).get(FLEET_DESIGNATION_NAME).await {
        Ok(fleet) => Ok(fleet.spec.image_cache),
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
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

/// The ImageBuild a vessel's composed image uses on its placement: its name,
/// and the spec to create when no copy of that name exists yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedImageBuild {
    pub name: String,
    pub create: Option<ImageBuildSpec>,
}

/// Plan, without writing, the ImageBuild for `resolved` on the placement
/// host's architecture. An existing build with the expected name, local or
/// replicated, is reused whatever its copies say.
pub async fn plan_image_build(
    backend: &ResourceBackend,
    namespace: &str,
    resolved: &ResolvedImage,
    placement_host: &str,
) -> Result<PlannedImageBuild, String> {
    plan_image_build_for(backend, namespace, resolved, placement_host, None).await
}

/// [`plan_image_build`] for an explicit architecture (`flotilla image check
/// --arch`). Another architecture than the placement host's needs the fleet
/// image cache: local delivery builds on, and for, the placement host.
pub async fn plan_image_build_for(
    backend: &ResourceBackend,
    namespace: &str,
    resolved: &ResolvedImage,
    placement_host: &str,
    architecture: Option<&str>,
) -> Result<PlannedImageBuild, String> {
    let hosts = hosts(backend, namespace).await?;
    let (placement, placement_architecture) = placement_architecture(&hosts, placement_host)?;
    let image_cache = fleet_image_cache(backend, namespace).await?;
    let architecture = match architecture.map(canonical_image_architecture) {
        Some(requested) if requested != placement_architecture && image_cache.is_none() => {
            return Err(format!(
                "no fleet image cache is declared, so images build on `{}` for {placement_architecture} only, not {requested}",
                placement.metadata.name
            ));
        }
        Some(requested) => requested.to_string(),
        None => placement_architecture,
    };
    let name = expected_build_name(&resolved.identity, &architecture, image_cache.as_ref(), &placement.metadata.name);
    match flotilla_store::read_image_build(backend, namespace, &name).await {
        Ok(_) => return Ok(PlannedImageBuild { name, create: None }),
        Err(ResourceError::NotFound { .. }) => {}
        Err(error) => return Err(error.to_string()),
    }
    let (host_ref, delivery, reservation) = choose_builder(&hosts, placement, &architecture, image_cache.as_ref())?;
    let spec = ImageBuildSpec::builder()
        .identity(resolved.identity.clone())
        .architecture(architecture)
        .layers(resolved.layers.clone())
        .host_ref(host_ref)
        .delivery(delivery)
        .reservation(reservation)
        .build();
    debug_assert_eq!(spec.resource_name(), name);
    Ok(PlannedImageBuild { name, create: Some(spec) })
}

/// Create the planned builds that do not exist yet, once admission has passed
/// every refusal. Returns the names this call created, so a failed commit can
/// remove them again; a concurrent creator's copy is reused.
pub async fn create_planned_builds(
    backend: &ResourceBackend,
    namespace: &str,
    planned: &[PlannedImageBuild],
) -> Result<Vec<String>, String> {
    let builds = backend.using::<ImageBuild>(namespace);
    let mut created = Vec::new();
    for build in planned {
        let Some(spec) = &build.create else { continue };
        if created.contains(&build.name) {
            continue;
        }
        match builds.create(&InputMeta::builder().name(build.name.clone()).build(), &spec.clone().into()).await {
            Ok(_) => created.push(build.name.clone()),
            Err(ResourceError::Conflict { .. }) => {}
            Err(error) => {
                remove_created_builds(backend, namespace, &created).await;
                return Err(format!("create ImageBuild {}: {error}", build.name));
            }
        }
    }
    Ok(created)
}

/// Best-effort removal of builds an abandoned admission created.
pub async fn remove_created_builds(backend: &ResourceBackend, namespace: &str, created: &[String]) {
    for name in created {
        if let Err(error) = backend.using::<ImageBuild>(namespace).delete(name).await {
            tracing::warn!(image_build = %name, %error, "could not remove an abandoned admission's image build");
        }
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
