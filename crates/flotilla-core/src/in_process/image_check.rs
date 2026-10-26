//! `flotilla image check --project X --ref <branch>` (ADR 0062, decision B3):
//! build and probe a branch's composed image before merge, with no CI.
//!
//! The project's primary repository's `.flotilla/image/` is read at the ref
//! through the VCS seam and substituted for the synced primary-repository
//! layer in the replica-inclusive catalogue; a charter-authored layer is kept,
//! and a branch defining both is refused as the sync refuses it. The
//! composition then resolves exactly as
//! admission resolves it (the project's `image:` or the cascaded installation
//! defaults, `--need`, and the harness adapters), and its ImageBuild is
//! planned and get-or-created through admission's machinery. Builds are once
//! per identity, so a check of an unchanged composition reuses the existing
//! build and its result. Definition errors and resolution refusals are
//! reported per layer and create no build.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_charter::charter_store::{
    source_read, LayerDirectorySnapshot, CHARTER_IMAGE_LAYER_PATH, PROJECT_IMAGE_LAYER_PATH, PROJECT_IMAGE_MANAGER,
};
use flotilla_protocol::image_check::{
    ImageBuildFailureReport, ImageBuildReport, ImageBuildReportPhase, ImageCheckAgentSource, ImageCheckRefusal, ImageCheckReport,
    ImageCheckRequest, ImageCheckShadow, ImageLayerReport, ImageProbeReport,
};
use flotilla_protocol::LifecycleReading;
use flotilla_resources::lifecycle_reads::{LifecycleReads, NoLifecycleReads};
use flotilla_resources::{
    parse_layer_directory, CapabilityNeed, CharterSource, ImageBuildFailureClass, ImageBuildPhase, ImageBuildSpec, ImageBuildStatus,
    ImageCatalogue, ImageDelivery, ImageLayerResourceSpec, ImageLayerSource, ImageLayerSpec, ImageProbeOutcome, ImageRequest,
    ImageResolutionError, Project, ProjectSpec, ResourceError, MANAGED_BY_LABEL,
};
use flotilla_store::ResourceBackend;

use super::project_image_layers::{primary_layer_home, LayerHome};
use flotilla_admission::convoy_admission::installation_image_defaults;

/// What a check reads beyond the store: the layer directory at a ref, and
/// the harness adapters a project's default workflow uses.
#[async_trait]
pub trait ImageCheckSources: Send + Sync {
    /// Fetch `source`'s branch and read its layer directory through the VCS
    /// seam; `None` when the directory is absent at that revision.
    async fn layer_snapshot(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String>;

    /// The harness adapters the project's default workflow's agent roles
    /// resolve to.
    async fn default_harness_adapters(&self, namespace: &str, project_ref: &str, project: &ProjectSpec)
        -> Result<BTreeSet<String>, String>;
}

/// Resolve `request`'s composition and get-or-create its ImageBuild for
/// `placement_host` (the asking daemon's host): the build's architecture is
/// that host's unless `request.architecture` names another. Refusals are
/// returned in the report, which then names no build; `Err` is an
/// infrastructure failure (an unknown project, a failed fetch, a store error).
pub async fn check_image(
    backend: &ResourceBackend,
    namespace: &str,
    placement_host: &str,
    sources: &dyn ImageCheckSources,
    request: &ImageCheckRequest,
) -> Result<ImageCheckReport, String> {
    check_image_with_lifecycle_reads(backend, namespace, placement_host, sources, request, Arc::new(NoLifecycleReads)).await
}

pub async fn check_image_with_lifecycle_reads(
    backend: &ResourceBackend,
    namespace: &str,
    placement_host: &str,
    sources: &dyn ImageCheckSources,
    request: &ImageCheckRequest,
    reads: Arc<dyn LifecycleReads>,
) -> Result<ImageCheckReport, String> {
    let project = match backend.including_replicas::<Project>(namespace).get(&request.project).await {
        Ok(project) => project.object,
        Err(ResourceError::NotFound { .. }) => return Err(format!("project `{}` is not defined", request.project)),
        Err(error) => return Err(error.to_string()),
    };
    let mut report = ImageCheckReport {
        project: request.project.clone(),
        git_ref: request.git_ref.clone(),
        revision: None,
        project_layer: None,
        agents: Vec::new(),
        agents_from: if request.agents.is_empty() { ImageCheckAgentSource::DefaultWorkflow } else { ImageCheckAgentSource::Requested },
        refusals: Vec::new(),
        shadowed: Vec::new(),
        build: None,
        reused: false,
    };

    // The project layer at the ref. A project without a code repository has
    // none at any ref.
    let mut branch_layer = None;
    if let Some(LayerHome::Source(source)) = primary_layer_home(backend, namespace, &project.spec, Some(&request.git_ref)).await? {
        source.validate()?;
        if let Some(snapshot) = source_read(sources.layer_snapshot(&source))
            .await
            .map_err(|error| format!("read `{PROJECT_IMAGE_LAYER_PATH}/` at {}: {error}", request.git_ref))?
        {
            report.revision = Some(snapshot.revision.clone());
            match parse_layer_directory(&snapshot.files, ImageLayerSource::Project, Some(&request.project)) {
                Ok(layer) => {
                    report.project_layer = Some(format!("{}@{}", layer.name, layer.version));
                    branch_layer = Some(layer);
                }
                Err(error) => {
                    report.refusals.push(ImageCheckRefusal { layers: vec![format!("{PROJECT_IMAGE_LAYER_PATH}/")], message: error });
                    return Ok(report);
                }
            }
        }
    }

    let (catalogue, charter_layer) = catalogue_with_branch_layer(backend, namespace, &request.project, branch_layer.clone()).await?;
    if let (Some(layer), Some(charter_layer)) = (&branch_layer, charter_layer) {
        // The sync refuses a project layer defined in both homes; so does a check.
        report.refusals.push(ImageCheckRefusal {
            layers: vec![layer.name.clone(), charter_layer.clone()],
            message: format!(
                "project `{}` defines its image layer twice: `{PROJECT_IMAGE_LAYER_PATH}/` in its primary repository and \
                 `{CHARTER_IMAGE_LAYER_PATH}/` in its charter (ImageLayer `{charter_layer}`)",
                request.project
            ),
        });
        return Ok(report);
    }

    let mut needs = BTreeSet::new();
    for raw in &request.needs {
        match raw.parse::<CapabilityNeed>() {
            Ok(need) if need.is_image_need() => {
                needs.insert(need.capability_string());
            }
            Ok(_) => report.refusals.push(ImageCheckRefusal {
                layers: Vec::new(),
                message: format!("`{raw}` is a host need, not an image need; images cannot provide it"),
            }),
            Err(error) => report.refusals.push(ImageCheckRefusal { layers: Vec::new(), message: format!("--need `{raw}`: {error}") }),
        }
    }
    let harness_adapters = if request.agents.is_empty() {
        sources
            .default_harness_adapters(namespace, &request.project, &project.spec)
            .await
            .map_err(|error| format!("the default workflow's harness adapters: {error} (name them with --agent)"))?
    } else {
        request.agents.iter().cloned().collect()
    };
    report.agents = harness_adapters.iter().cloned().collect();
    if !report.refusals.is_empty() {
        return Ok(report);
    }

    let defaults = installation_image_defaults(backend, namespace, Some(&request.project)).await?;
    let image_request = ImageRequest::builder()
        .project(request.project.clone())
        .maybe_selection(project.spec.image.clone())
        .maybe_defaults(defaults)
        .needs(needs)
        .harness_adapters(harness_adapters)
        .build();
    let resolved = flotilla_resources::resolve_image(&catalogue, &image_request);
    // Catalogue-level definition errors are reported for the names this
    // composition touches: its layers, or the names its refusal names.
    let touched = match &resolved {
        Ok(resolved) => resolved.layers.iter().map(|layer| layer.name.clone()).collect::<BTreeSet<_>>(),
        Err(error) => resolution_layers(error).into_iter().chain(branch_layer.iter().map(|layer| layer.name.clone())).collect(),
    };
    if let Err(error) = &resolved {
        report.refusals.push(ImageCheckRefusal { layers: resolution_layers(error), message: error.to_string() });
    }
    if let Err(errors) = catalogue.effective(&request.project).check() {
        for error in errors {
            let layers = resolution_layers(&error);
            let message = error.to_string();
            let reported = report.refusals.iter().any(|refusal| refusal.message == message);
            if !reported && layers.iter().any(|layer| touched.contains(layer)) {
                report.refusals.push(ImageCheckRefusal { layers, message });
            }
        }
    }
    let resolved = match resolved {
        Ok(resolved) if report.refusals.is_empty() => resolved,
        _ => return Ok(report),
    };
    report.shadowed = resolved
        .shadowed
        .iter()
        .map(|shadow| ImageCheckShadow {
            name: shadow.name.clone(),
            source: shadow.source.to_string(),
            shadowed: shadow.shadowed.to_string(),
            versions: shadow.versions.clone(),
        })
        .collect();

    let planned = flotilla_admission::image_build::plan_image_build_for(
        backend,
        namespace,
        &resolved,
        placement_host,
        request.architecture.as_deref(),
    )
    .await?;
    let created = flotilla_admission::image_build::create_planned_builds(backend, namespace, std::slice::from_ref(&planned)).await?;
    report.reused = created.is_empty();
    report.build = Some(image_build_report_with_lifecycle_reads(backend, namespace, &planned.name, reads).await?);
    Ok(report)
}

/// The replica-inclusive catalogue admission reads
/// ([`flotilla_admission::image_build::declared_image_layers`]), with the project's
/// primary-repository layer replaced by the one at the checked ref (`None`:
/// the ref has no project layer). Also returns a charter-authored project
/// layer for this project, if any.
async fn catalogue_with_branch_layer(
    backend: &ResourceBackend,
    namespace: &str,
    project: &str,
    branch_layer: Option<ImageLayerSpec>,
) -> Result<(ImageCatalogue, Option<String>), String> {
    let mut charter_layer = None;
    let mut specs = Vec::new();
    for layer in flotilla_admission::image_build::declared_image_layers(backend, namespace).await? {
        if let Some(spec) = layer.spec.current() {
            if spec.source == ImageLayerSource::Project && spec.project.as_deref() == Some(project) {
                if layer.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some(PROJECT_IMAGE_MANAGER) {
                    continue;
                }
                charter_layer = Some(layer.metadata.name.clone());
            }
        }
        specs.push(layer.spec);
    }
    specs.extend(branch_layer.map(ImageLayerResourceSpec::from));
    Ok((ImageCatalogue::from_specs(&specs), charter_layer))
}

/// The layer names a resolution error names.
fn resolution_layers(error: &ImageResolutionError) -> Vec<String> {
    let reference_name = |reference: &str| reference.split('@').next().unwrap_or(reference).to_string();
    match error {
        ImageResolutionError::Clash { name, .. } | ImageResolutionError::DuplicateSelection { name, .. } => vec![name.clone()],
        ImageResolutionError::UnknownLayer { reference } | ImageResolutionError::WrongTier { reference, .. } => {
            vec![reference_name(reference)]
        }
        ImageResolutionError::NoBase { candidates } => candidates.clone(),
        ImageResolutionError::MultipleProjectLayers { names } | ImageResolutionError::RequiresCycle { layers: names } => names.clone(),
        ImageResolutionError::Unresolvable { required_by, .. } => required_by.iter().cloned().collect(),
        ImageResolutionError::DuplicateProviders { providers, .. } => providers.clone(),
        ImageResolutionError::NotSelectable { layer, .. }
        | ImageResolutionError::HarnessVersion { layer, .. }
        | ImageResolutionError::Invalid { layer, .. } => vec![layer.clone()],
        ImageResolutionError::RequiresUpward { layer, provider, .. } => vec![layer.clone(), provider.clone()],
    }
}

/// One ImageBuild as a check reports it, from the most advanced copy.
pub async fn image_build_report(backend: &ResourceBackend, namespace: &str, name: &str) -> Result<ImageBuildReport, String> {
    image_build_report_with_lifecycle_reads(backend, namespace, name, Arc::new(NoLifecycleReads)).await
}

pub async fn image_build_report_with_lifecycle_reads(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    reads: Arc<dyn LifecycleReads>,
) -> Result<ImageBuildReport, String> {
    let selected = match flotilla_store::read_image_build_with_lifecycle(backend, namespace, name, reads).await {
        Ok(selected) => selected,
        Err(ResourceError::NotFound { .. }) => return Err(format!("ImageBuild `{name}` does not exist")),
        Err(error) => return Err(error.to_string()),
    };
    let build = selected.build.object;
    let spec = build.spec.current().ok_or_else(|| format!("ImageBuild `{name}` is a retired record"))?;
    Ok(build_report(name, spec, build.status.as_ref(), selected.lifecycle.as_ref()))
}

/// Combine ordered layers and actuator observations with the selected lifecycle judgement.
/// Old phase, digest and failure decide the report only when the engine holds no resource.
fn build_report(
    name: &str,
    spec: &ImageBuildSpec,
    status: Option<&ImageBuildStatus>,
    lifecycle: Option<&LifecycleReading>,
) -> ImageBuildReport {
    let status = status.cloned().unwrap_or_default();
    let legacy_failure = || {
        status.failure.as_ref().map(|failure| ImageBuildFailureReport {
            class: match failure.class {
                ImageBuildFailureClass::Infrastructure => "infrastructure".to_string(),
                ImageBuildFailureClass::Recipe => "recipe".to_string(),
            },
            reason: failure.reason.clone(),
        })
    };
    let lifecycle_failure = |reason: String| Some(ImageBuildFailureReport { class: "lifecycle".into(), reason });
    let running = status.started_at.is_some() && status.finished_at.is_none();
    let pending_phase = if running { ImageBuildReportPhase::Building } else { ImageBuildReportPhase::Queued };
    let (phase, settled, digest, failure) = match lifecycle {
        Some(LifecycleReading::Ready { identity }) => (ImageBuildReportPhase::Built, true, Some(identity.clone()), None),
        Some(LifecycleReading::InProgress) => (pending_phase, false, None, None),
        Some(LifecycleReading::Retrying { next, cause, .. }) => {
            // A started attempt is an actuator observation; the engine alone decides whether it may retry.
            let phase = if running && status.started_at.is_some_and(|started| started >= *next) {
                ImageBuildReportPhase::Building
            } else {
                ImageBuildReportPhase::Queued
            };
            (phase, false, None, lifecycle_failure(cause.clone()))
        }
        Some(LifecycleReading::Exhausted { cause } | LifecycleReading::GaveUp { cause }) => {
            (ImageBuildReportPhase::Failed, true, None, lifecycle_failure(cause.clone()))
        }
        Some(ending @ (LifecycleReading::Lost | LifecycleReading::Released | LifecycleReading::Failed { .. })) => {
            (ImageBuildReportPhase::Failed, true, None, lifecycle_failure(ending.reason().unwrap_or_default()))
        }
        None => {
            let phase = match status.phase {
                ImageBuildPhase::Queued => ImageBuildReportPhase::Queued,
                ImageBuildPhase::Building => ImageBuildReportPhase::Building,
                ImageBuildPhase::Built => ImageBuildReportPhase::Built,
                ImageBuildPhase::Failed => ImageBuildReportPhase::Failed,
            };
            (phase, status.is_settled(), status.digest.clone(), legacy_failure())
        }
    };
    let finished = match lifecycle {
        Some(_) => settled,
        None => matches!(status.phase, ImageBuildPhase::Built | ImageBuildPhase::Failed),
    };
    let probes = status.probes.iter().map(|probe| (probe.layer.as_str(), &probe.outcome)).collect::<BTreeMap<_, _>>();
    let layers = spec
        .layers
        .iter()
        .map(|layer| ImageLayerReport {
            layer: layer.name.clone(),
            source: layer.source.to_string(),
            version: layer.version.clone(),
            tier: layer.tier.to_string(),
            probe: match probes.get(layer.name.as_str()) {
                Some(ImageProbeOutcome::Passed) => ImageProbeReport::Passed,
                Some(ImageProbeOutcome::Failed { reason }) => ImageProbeReport::Failed { reason: reason.clone() },
                Some(ImageProbeOutcome::NotRun) => ImageProbeReport::NotRun,
                None if finished => ImageProbeReport::NotRun,
                None => ImageProbeReport::Pending,
            },
        })
        .collect();
    ImageBuildReport {
        name: name.to_string(),
        identity: spec.identity.clone(),
        architecture: spec.architecture.clone(),
        builder_host: spec.host_ref.clone(),
        delivery: match &spec.delivery {
            ImageDelivery::Registry { repository } => format!("registry {repository}"),
            ImageDelivery::Local => "local".to_string(),
        },
        phase,
        settled,
        attempt: match lifecycle {
            Some(LifecycleReading::InProgress) => u32::from(running),
            Some(LifecycleReading::Retrying { failures, next, .. }) => {
                failures.saturating_add(u32::from(running && status.started_at.is_some_and(|started| started >= *next)))
            }
            _ => status.attempt,
        },
        digest,
        layers,
        failure,
        log_ref: status.log_ref.clone().filter(|reference| !reference.is_empty()),
    }
}

#[cfg(test)]
mod lifecycle_contract {
    use super::*;
    use flotilla_provisioning::engine::image_build::ImageBuildBinding;
    use flotilla_provisioning_testkit::{image_build, store_object, Ending, LifecycleFixture};
    use flotilla_resources::ImageBuild;
    use flotilla_store_testkit::{measure_reads, ReadCountsBackendExt};

    // Reports select engine digest/cause at the chosen origin while old Built evidence stays unchanged.
    #[tokio::test]
    async fn image_reports_follow_engine_without_a_copy() {
        for replica in [false, true] {
            let backend = flotilla_store::ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_read_counts();
            let author = if replica {
                flotilla_store::ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default())
            } else {
                backend.clone()
            };
            let obj = image_build(ImageBuildPhase::Built);
            let name = obj.metadata.name.as_str();
            store_object(&author, obj.clone()).await;
            if replica {
                backend
                    .replica_writer::<ImageBuild>(flotilla_protocol::NodeId::new("home"), "flotilla")
                    .replace(&author.using::<ImageBuild>("flotilla").list().await.expect("snapshot"), chrono::Utc::now())
                    .await
                    .expect("replica");
            }
            let fixture = LifecycleFixture::<ImageBuildBinding>::new(replica);
            fixture.request(&obj);
            let report = || image_build_report_with_lifecycle_reads(&backend, "flotilla", name, fixture.reads.clone());
            let (requested, counts) = measure_reads(report()).await;
            counts.of(&backend).assert_no_lists("one named image report");
            let requested = requested.expect("requested");
            assert_eq!((requested.phase, requested.attempt), (ImageBuildReportPhase::Queued, 0));
            assert!(requested.digest.is_none());
            fixture.retry(&obj, "network");
            assert_eq!(report().await.expect("retry").attempt, 1);
            fixture.finish(&obj, Ending::Ready("engine digest"));
            assert_eq!(report().await.expect("ready").digest.as_deref(), Some("engine digest"));
            fixture.finish(&obj, Ending::Lost("engine digest"));
            let lost = report().await.expect("lost");
            assert_eq!(lost.phase, ImageBuildReportPhase::Failed);
            assert_eq!(lost.failure.expect("reason").reason, "observed absent");
            fixture.poison();
            assert!(report().await.expect_err("checked read").contains("poisoned"));
        }
    }
}
