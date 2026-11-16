//! `flotilla image check --project X --ref <branch>` (ADR 0062, decision B3):
//! build and probe a branch's composed image before merge, with no CI.
//!
//! The project's declaring repository's `.flotilla/image/` is read at the ref
//! through the VCS seam and substituted for the synced declaring-repository
//! layer in the replica-inclusive catalogue; a charter-authored layer is kept,
//! and a branch defining both is refused as the sync refuses it. The
//! composition then resolves exactly as
//! admission resolves it (the project's `image:` or the cascaded installation
//! defaults, `--need`, and the harness adapters), and its image build is
//! planned and demanded at this home through admission's machinery. Builds are
//! once per identity, so a check of an unchanged composition reuses the
//! existing build and its result. Definition errors and resolution refusals are
//! reported per layer and create no build.
use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use flotilla_charter::charter_store::{
    source_read, LayerDirectorySnapshot, CHARTER_IMAGE_LAYER_PATH, PROJECT_IMAGE_LAYER_PATH, PROJECT_IMAGE_MANAGER,
};
use flotilla_protocol::image_check::{
    ImageBuildFailureReport, ImageBuildReport, ImageBuildReportPhase, ImageCheckAgentSource, ImageCheckRefusal, ImageCheckReport,
    ImageCheckRequest, ImageCheckShadow, ImageLayerReport, ImageProbeReport,
};
use flotilla_protocol::LifecycleReading;
use flotilla_provisioning::engine::image_build_actuator::build_logs_of;
use flotilla_resources::{image_build_records::ImageBuildRecord, lifecycle_reads::LifecycleReads};
use flotilla_resources::{
    parse_layer_directory, CapabilityNeed, CharterSource, ImageBuildSpec, ImageCatalogue, ImageDelivery, ImageLayerResourceSpec,
    ImageLayerSource, ImageLayerSpec, ImageRequest, ImageResolutionError, Project, ProjectSpec, ResourceError, MANAGED_BY_LABEL,
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

    let mut branch_layer = None;
    let home = primary_layer_home(backend, namespace, &project, Some(&request.git_ref)).await?;
    if let LayerHome::Refused(reason) = &home {
        report.refusals.push(ImageCheckRefusal { layers: vec![format!("{PROJECT_IMAGE_LAYER_PATH}/")], message: reason.clone() });
    }
    if let LayerHome::Source(source) = home {
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
                "project `{}` defines its image layer twice: `{PROJECT_IMAGE_LAYER_PATH}/` in its declaring repository and \
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
    let demanded = flotilla_admission::image_build::demand_planned_builds(Arc::clone(&reads), std::slice::from_ref(&planned)).await?;
    report.reused = demanded.is_empty();
    report.build = Some(image_build_report(reads, &planned.name).await?);
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

/// One image build as a check reports it: its newest launch at its builder, or its demand here
/// before the builder has launched it.
pub async fn image_build_report(reads: Arc<dyn LifecycleReads>, name: &str) -> Result<ImageBuildReport, String> {
    let owned = name.to_owned();
    let (launched, demanded) = tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
        let builds = reads.image_builds()?;
        match builds.get(&owned)? {
            Some(launched) => Ok((Some(launched), None)),
            None => Ok::<_, String>((None, builds.demanded(&owned)?)),
        }
    }))
    .await
    .map_err(|error| format!("read image build {name}: {error}"))??;
    match (launched, demanded) {
        (Some(launched), _) => Ok(build_report(name, &launched.spec, Some(&launched))),
        (None, Some(spec)) => Ok(build_report(name, &spec, None)),
        (None, None) => Err(format!("image build `{name}` is neither demanded here nor launched")),
    }
}

/// The report of build `name` building `spec`: `launched` is its newest launch, `None` while
/// only demanded. Probe outcomes are kept in each try's build log, not here.
fn build_report(name: &str, spec: &ImageBuildSpec, launched: Option<&ImageBuildRecord>) -> ImageBuildReport {
    let failure_of = |class: &str, reason: &str| Some(ImageBuildFailureReport { class: class.into(), reason: reason.to_owned() });
    let lifecycle_failure = |reason: String| failure_of("lifecycle", &reason);
    let (phase, settled, digest, failure) = match launched.map(|launched| &launched.reading) {
        None => (ImageBuildReportPhase::Queued, false, None, None),
        Some(LifecycleReading::Ready { identity }) => (ImageBuildReportPhase::Built, true, Some(identity.clone()), None),
        Some(LifecycleReading::InProgress) => (ImageBuildReportPhase::Building, false, None, None),
        Some(LifecycleReading::Retrying { cause, .. }) => (ImageBuildReportPhase::Queued, false, None, failure_of("infrastructure", cause)),
        // A build gives up at once only on a recipe failure, whose cause names its owning layer;
        // it is exhausted by infrastructure failures.
        Some(LifecycleReading::GaveUp { cause }) => (ImageBuildReportPhase::Failed, true, None, failure_of("recipe", cause)),
        Some(LifecycleReading::Exhausted { cause }) => (ImageBuildReportPhase::Failed, true, None, failure_of("infrastructure", cause)),
        Some(ending @ (LifecycleReading::Lost { .. } | LifecycleReading::Released | LifecycleReading::Failed { .. })) => {
            (ImageBuildReportPhase::Failed, true, None, lifecycle_failure(ending.reason().unwrap_or_default()))
        }
    };
    let tried = launched.is_some_and(|launched| settled || launched.failures > 0);
    let layers = spec
        .layers
        .iter()
        .map(|layer| ImageLayerReport {
            layer: layer.name.clone(),
            source: layer.source.to_string(),
            version: layer.version.clone(),
            tier: layer.tier.to_string(),
            probe: if tried { ImageProbeReport::InBuildLog } else { ImageProbeReport::Pending },
        })
        .collect();
    ImageBuildReport {
        name: name.to_string(),
        launch: launched.map(|launched| launched.launch.clone()),
        identity: spec.identity.clone(),
        architecture: spec.architecture.clone(),
        builder_host: spec.host_ref.clone(),
        delivery: match &spec.delivery {
            ImageDelivery::Registry { repository } => format!("registry {repository}"),
            ImageDelivery::Local => "local".to_string(),
        },
        phase,
        settled,
        attempt: launched.map_or(0, |launched| launched.failures),
        digest,
        layers,
        failure,
        log_ref: launched.filter(|_| tried).map(|launched| build_logs_of(&launched.launch)),
    }
}
