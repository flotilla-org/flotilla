//! Project image layers (ADR 0062): the `.flotilla/image/` directory at the
//! root of a project's primary repository, read at the default-branch head as
//! last fetched, becomes its project-source ImageLayer. Every project's
//! primary repository is read, charter-delegated projects included; a
//! delegated charter may instead author the layer at `<charter path>/image/`
//! (charter delegation), and a project defining both is refused, naming both
//! locations. Removing the directory removes the layer.
use std::collections::BTreeMap;

use flotilla_charter::{
    charter_store::{CHARTER_IMAGE_LAYER_PATH, PROJECT_IMAGE_LAYER_PATH, PROJECT_IMAGE_MANAGER},
    ops_entry::{MATERIALIZED_PROJECT_ANNOTATION, SOURCE_COMMIT_ANNOTATION},
};
use flotilla_resources::{
    parse_layer_directory, CharterSource, ConditionValue, HostCondition, ImageLayer, ImageLayerSource, InputMeta, Project,
    ProjectRepositoryRole, ProjectSpec, Repository, ResourceError, ResourceObject, MANAGED_BY_LABEL,
};
use flotilla_store::ResourceBackend;
use tracing::warn;

use super::project_ops::{repository_transport_url_with_backend, ProjectService};

/// Where one project's layer directory lives, as seen from this host.
#[derive(Debug, PartialEq, Eq)]
pub enum LayerHome {
    /// Read the directory here.
    Source(CharterSource),
    /// Nothing can be read yet (no default branch); leave the layer as it is.
    Elsewhere,
}

/// The project's primary (first code) repository's layer directory, at
/// `branch` or else the repository's default branch. `None`: the project has
/// no code repository.
pub async fn primary_layer_home(
    backend: &ResourceBackend,
    namespace: &str,
    project: &ProjectSpec,
    branch: Option<&str>,
) -> Result<Option<LayerHome>, String> {
    let Some(primary) = project.repositories.iter().find(|member| member.roles.contains(&ProjectRepositoryRole::Code)) else {
        return Ok(None);
    };
    let repository = backend
        .including_replicas::<Repository>(namespace)
        .get(&primary.repo.to_string())
        .await
        .map_err(|error| format!("repository {}: {error}", primary.repo))?
        .object;
    let Some(branch) = branch
        .map(str::to_string)
        .or_else(|| primary.default_branch.clone())
        .or_else(|| repository.status.as_ref().and_then(|status| status.default_branch.clone()))
    else {
        return Ok(Some(LayerHome::Elsewhere));
    };
    let repo = repository_transport_url_with_backend(backend, namespace, &repository.spec).await?;
    let path = primary.subpath.as_deref().map_or_else(
        || PROJECT_IMAGE_LAYER_PATH.to_string(),
        |subpath| format!("{}/{PROJECT_IMAGE_LAYER_PATH}", subpath.trim_end_matches('/')),
    );
    Ok(Some(LayerHome::Source(CharterSource::Repository { repo, branch, path })))
}

/// Maximum UTF-8 bytes in one project image-layer heartbeat message.
const MAX_LAYER_DIAGNOSIS_BYTES: usize = 4096;

fn bounded_layer_diagnosis(message: String) -> String {
    if message.len() <= MAX_LAYER_DIAGNOSIS_BYTES {
        return message;
    }
    let marker = "\n...[truncated]...\n";
    let mut head = (MAX_LAYER_DIAGNOSIS_BYTES - marker.len()) / 2;
    while !message.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = message.len() - (MAX_LAYER_DIAGNOSIS_BYTES - marker.len() - head);
    while !message.is_char_boundary(tail) {
        tail += 1;
    }
    // Keep project identity at the front and the fatal cause/lock path at the
    // end, rather than letting a long progress log crowd out the diagnosis.
    format!("{}{}{}", &message[..head], marker, &message[tail..])
}

/// Host-local retry state; successful reads and source changes reset refusal backoff.
#[derive(Default)]
pub struct LayerRefreshState {
    refusals: BTreeMap<(String, String), LayerRefusal>,
}

struct LayerRefusal {
    source: CharterSource,
    error: String,
    delay: std::time::Duration,
    retry_at: tokio::time::Instant,
    observed_at: chrono::DateTime<chrono::Utc>,
}

impl LayerRefreshState {
    pub fn conditions(&self) -> Vec<HostCondition> {
        self.refusals
            .iter()
            .map(|((namespace, project), refusal)| {
                HostCondition::builder()
                    .condition_type(format!("ProjectImageLayer/{namespace}/{project}"))
                    .value(ConditionValue::False)
                    .reason("SourceRefused")
                    .message(bounded_layer_diagnosis(format!(
                        "project {namespace}/{project}: {}; retaining previous layer and retrying with backoff",
                        refusal.error
                    )))
                    .observed_at(refusal.observed_at)
                    .blocks_readiness(false)
                    .blocks_forge_observation(false)
                    .build()
            })
            .collect()
    }

    fn deferred(&self, namespace: &str, project: &str, source: &CharterSource) -> bool {
        self.refusals
            .get(&(namespace.into(), project.into()))
            .is_some_and(|refusal| &refusal.source == source && tokio::time::Instant::now() < refusal.retry_at)
    }

    // Preserve the complete source/error identity. Different refusals get an
    // initial retry; repeated identical errors back off. Dynamic-message error
    // classification can be added when the inspector exposes typed errors.
    fn refused(&mut self, namespace: &str, project: &str, source: &CharterSource, error: String) {
        let key = (namespace.to_string(), project.to_string());
        let previous = self.refusals.get(&key).filter(|previous| previous.source == *source && previous.error == error);
        let observed_at = previous.map_or_else(chrono::Utc::now, |previous| previous.observed_at);
        let delay =
            previous.map_or(std::time::Duration::from_secs(60), |previous| (previous.delay * 2).min(std::time::Duration::from_secs(3600)));
        self.refusals
            .insert(key, LayerRefusal { source: source.clone(), error, delay, retry_at: tokio::time::Instant::now() + delay, observed_at });
    }
}

impl ProjectService<'_> {
    /// Sync every project's layer directory this host is responsible for.
    /// Each project's failure is reported and leaves its previous layer.
    pub async fn reconcile_project_image_layers(&self) -> Result<(), String> {
        let namespace = self.namespace.read().expect("provisioning namespace lock poisoned").clone();
        let local = self
            .resource_backend
            .using::<Project>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|project| project.metadata.deletion_timestamp.is_none())
            .map(|project| project.metadata.name)
            .collect::<std::collections::BTreeSet<_>>();
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        self.image_layer_refresh.lock().await.refusals.retain(|(ns, project), _| ns != &namespace || local.contains(project));
        for project in projects {
            if project.metadata.deletion_timestamp.is_some() || !local.contains(&project.metadata.name) {
                continue;
            }
            let source = match self.layer_home(&namespace, &project).await {
                Ok(Some(LayerHome::Source(source))) => source,
                Ok(None) | Ok(Some(LayerHome::Elsewhere)) => {
                    self.image_layer_refresh.lock().await.refusals.remove(&(namespace.clone(), project.metadata.name.clone()));
                    continue;
                }
                Err(error) => {
                    self.image_layer_refresh.lock().await.refusals.remove(&(namespace.clone(), project.metadata.name.clone()));
                    warn!(project = %project.metadata.name, %error, "project image layer source unavailable");
                    continue;
                }
            };
            if self.image_layer_refresh.lock().await.deferred(&namespace, &project.metadata.name, &source) {
                continue;
            }
            match self.sync_project_image_layer(&namespace, &project.metadata.name, &source).await {
                Ok(()) => {
                    self.image_layer_refresh.lock().await.refusals.remove(&(namespace.clone(), project.metadata.name.clone()));
                }
                Err(error) => self.image_layer_refresh.lock().await.refused(&namespace, &project.metadata.name, &source, error),
            }
        }
        Ok(())
    }

    async fn layer_home(&self, namespace: &str, project: &ResourceObject<Project>) -> Result<Option<LayerHome>, String> {
        primary_layer_home(self.resource_backend, namespace, &project.spec, None).await
    }

    async fn sync_project_image_layer(&self, namespace: &str, project: &str, source: &CharterSource) -> Result<(), String> {
        let inspector = self.operations.repository_inspector().await?;
        let snapshot = flotilla_charter::charter_store::source_read(inspector.image_layer_snapshot(source)).await?;
        let desired = match &snapshot {
            Some(snapshot) => Some(
                parse_layer_directory(&snapshot.files, ImageLayerSource::Project, Some(project))
                    .map_err(|error| format!("{PROJECT_IMAGE_LAYER_PATH}: {error}"))?,
            ),
            None => None,
        };
        let local = self.resource_backend.using::<ImageLayer>(namespace);
        let definitions = self.resource_backend.definitions::<ImageLayer>(namespace);
        let desired_name = desired.as_ref().map(|layer| layer.resource_name());
        if let (Some(layer), Some(snapshot), Some(name)) = (&desired, &snapshot, &desired_name) {
            // One project, one layer source: a charter-authored layer for this
            // project as well is a within-source clash, refused by name.
            for other in definitions.list().await.map_err(|error| error.to_string())? {
                let charter_authored = other.metadata.deletion_timestamp.is_none()
                    && other.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(PROJECT_IMAGE_MANAGER)
                    && other
                        .spec
                        .current()
                        .is_some_and(|spec| spec.source == ImageLayerSource::Project && spec.project.as_deref() == Some(project));
                if charter_authored {
                    return Err(format!(
                        "project `{project}` defines its image layer twice: `{PROJECT_IMAGE_LAYER_PATH}/` in its primary repository and \
                         `{CHARTER_IMAGE_LAYER_PATH}/` in its charter (ImageLayer `{}`)",
                        other.metadata.name
                    ));
                }
            }
            let labels = BTreeMap::from([(MANAGED_BY_LABEL.to_string(), PROJECT_IMAGE_MANAGER.to_string())]);
            let annotations = BTreeMap::from([
                (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project.to_string()),
                (SOURCE_COMMIT_ANNOTATION.to_string(), snapshot.revision.clone()),
            ]);
            let current = match local.get(name).await {
                Ok(current) if current.metadata.deletion_timestamp.is_none() => Some(current),
                Ok(_) | Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            if let Some(current) = &current {
                if current.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str) != Some(PROJECT_IMAGE_MANAGER) {
                    return Err(format!("ImageLayer `{name}` already exists and is not materialized by project `{project}`"));
                }
            }
            // The definition, not the revision, is the layer: an unrelated
            // commit to the repository leaves an unchanged layer untouched.
            if current.as_ref().is_none_or(|current| current.spec.current() != Some(layer)) {
                let meta = InputMeta::builder().name(name.clone()).labels(labels).annotations(annotations).build();
                definitions.apply(&meta, &layer.clone().into()).await.map_err(|error| format!("apply ImageLayer `{name}`: {error}"))?;
            }
        }
        for stale in local.list().await.map_err(|error| error.to_string())?.items {
            let owned = stale.metadata.labels.get(MANAGED_BY_LABEL).map(String::as_str) == Some(PROJECT_IMAGE_MANAGER)
                && stale.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).map(String::as_str) == Some(project);
            if owned && stale.metadata.deletion_timestamp.is_none() && desired_name.as_deref() != Some(stale.metadata.name.as_str()) {
                match definitions.delete(&stale.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(format!("delete ImageLayer `{}`: {error}", stale.metadata.name)),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::{
        path::Path,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use flotilla_charter::charter_store::LayerDirectorySnapshot;
    use flotilla_resources::{
        CharterPointer, Clock, ImageLayerTier, ProjectRepositorySpec, ProjectSpec, RepositorySpec, RepositoryStatus, SystemClock,
    };
    use flotilla_store::ResourceBackend;
    use flotilla_store_memory::InMemoryBackend;

    use super::*;
    use crate::{
        project_ops::RepositoryIndex,
        repository_inspection::{RepositoryInspection, RepositoryInspector},
    };

    /// Stands in for reading a layer directory through the VCS seam: one
    /// directory content for every source, recording what was read.
    #[derive(Default)]
    struct LayerSource {
        files: Mutex<Option<BTreeMap<String, Vec<u8>>>>,
        reads: Mutex<Vec<CharterSource>>,
        error: Mutex<Option<String>>,
    }

    #[async_trait]
    impl RepositoryInspector for LayerSource {
        async fn inspect_path(&self, _: &Path, _: Option<&str>) -> Result<RepositoryInspection, String> {
            Err("unexpected inspection".into())
        }

        async fn image_layer_snapshot(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
            self.reads.lock().expect("reads").push(source.clone());
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error);
            }
            Ok(self.files.lock().expect("files").clone().map(|files| LayerDirectorySnapshot { revision: "commit".into(), files }))
        }
    }

    use crate::test_support::TestProjectOperations as Operations;

    fn layer_files(version: &str) -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "layer.yaml".to_string(),
                format!(
                    "name: app-native\nversion: {version}\ntier: project\nprovides: [project:app]\nrequires: [toolchain:rust]\nprobe: pkg-config --exists x11\ncontext_files: [apt.txt]\n"
                )
                .into_bytes(),
            ),
            ("Dockerfile.fragment".to_string(), b"FROM ${BASE}\nCOPY --from=app-native apt.txt /tmp/apt.txt\n".to_vec()),
            ("apt.txt".to_string(), b"libx11-dev\n".to_vec()),
        ])
    }

    fn layer_name(version: &str) -> String {
        parse_layer_directory(&layer_files(version), ImageLayerSource::Project, Some("app")).expect("layer").resource_name()
    }

    async fn seed(backend: &ResourceBackend, charter: Option<CharterPointer>) {
        let repository = RepositorySpec::remote("https://github.com/example/app").expect("repository");
        let created = backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(repository.key().0.clone()).build(), &repository)
            .await
            .expect("repository");
        backend
            .using::<Repository>("flotilla")
            .update_status(
                &created.metadata.name,
                &created.metadata.resource_version,
                &RepositoryStatus { default_branch: Some("main".into()), ..Default::default() },
            )
            .await
            .expect("default branch");
        let mut spec = ProjectSpec::builder().display_name("app".into()).maybe_charter(charter).build();
        spec.repositories = vec![
            ProjectRepositorySpec::builder()
                .repo(RepositorySpec::remote("https://github.com/example/docs").expect("docs").key())
                .roles([ProjectRepositoryRole::Knowledge].into())
                .build(),
            ProjectRepositorySpec::builder().repo(repository.key()).roles([ProjectRepositoryRole::Code].into()).build(),
        ];
        backend.using::<Project>("flotilla").create(&InputMeta::builder().name("app".into()).build(), &spec).await.expect("project");
    }

    // Repeated failures retain the previous layer, back off reads, and expose
    // exactly one current project diagnosis, cleared by a successful retry.
    #[tokio::test(start_paused = true)]
    async fn image_layer_refusal_backs_off_and_recovers() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let observed = ResourceBackend::new(InMemoryBackend::observed());
        seed(&backend, None).await;
        let source = Arc::new(LayerSource::default());
        let operations = Operations::with_inspector(source.clone());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let roots = flotilla_store::OldStoreManifestRoots::new(backend.clone());
        let service = service(&backend, &observed, &clock, &namespace, &operations, &roots);
        *source.error.lock().unwrap() = Some("Unable to create /cache/shallow.lock: File exists".into());
        service.reconcile_project_image_layers().await.unwrap();
        for _ in 0..10 {
            service.reconcile_project_image_layers().await.unwrap();
        }
        assert_eq!(source.reads.lock().unwrap().len(), 1);
        let diagnoses = service.image_layer_refresh.lock().await.conditions();
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].condition_type, "ProjectImageLayer/flotilla/app");
        assert!(!diagnoses[0].blocks_readiness && !diagnoses[0].blocks_forge_observation);
        assert!(diagnoses[0].message.contains("app") && diagnoses[0].message.contains("/cache/shallow.lock"));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        service.reconcile_project_image_layers().await.unwrap();
        assert_eq!(source.reads.lock().unwrap().len(), 2);
        assert_eq!(service.image_layer_refresh.lock().await.conditions()[0].observed_at, diagnoses[0].observed_at);
        tokio::time::advance(std::time::Duration::from_secs(119)).await;
        service.reconcile_project_image_layers().await.unwrap();
        assert_eq!(source.reads.lock().unwrap().len(), 2);
        *source.error.lock().unwrap() = None;
        *source.files.lock().unwrap() = Some(layer_files("1.0.0"));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        service.reconcile_project_image_layers().await.unwrap();
        assert_eq!(source.reads.lock().unwrap().len(), 3);
        assert!(service.image_layer_refresh.lock().await.conditions().is_empty());
        assert!(backend.using::<ImageLayer>("flotilla").get(&layer_name("1.0.0")).await.is_ok());

        // Deleting projects must lose their diagnosis even while a finalizer
        // keeps their local resource visible during teardown.
        *source.error.lock().unwrap() = Some("again".into());
        service.reconcile_project_image_layers().await.unwrap();
        assert_eq!(service.image_layer_refresh.lock().await.conditions().len(), 1);
        let projects = backend.using::<Project>("flotilla");
        let project = projects.get("app").await.unwrap();
        backend
            .definitions::<Project>("flotilla")
            .apply(&InputMeta::builder().name("app".into()).finalizers(vec!["test/hold".into()]).build(), &project.spec)
            .await
            .unwrap();
        projects.delete("app").await.unwrap();
        assert!(projects.get("app").await.unwrap().metadata.deletion_timestamp.is_some());
        service.reconcile_project_image_layers().await.unwrap();
        assert!(service.image_layer_refresh.lock().await.conditions().is_empty());
    }

    // A refusal belongs to one readable layer home. Removing the code home,
    // losing its default branch, or losing the repository clears that old
    // diagnosis without retrying a fetch still inside its backoff window.
    #[tokio::test(start_paused = true)]
    async fn image_layer_refusal_clears_when_its_home_disappears() {
        for mode in 0..3 {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let observed = ResourceBackend::new(InMemoryBackend::observed());
            seed(&backend, None).await;
            let source = Arc::new(LayerSource::default());
            *source.error.lock().unwrap() = Some("old /cache/shallow.lock refusal".into());
            let operations = Operations::with_inspector(source.clone());
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let namespace = std::sync::RwLock::new("flotilla".to_string());
            let roots = flotilla_store::OldStoreManifestRoots::new(backend.clone());
            let service = service(&backend, &observed, &clock, &namespace, &operations, &roots);
            service.reconcile_project_image_layers().await.unwrap();
            assert_eq!(service.image_layer_refresh.lock().await.conditions().len(), 1);
            let mut project = backend.using::<Project>("flotilla").get("app").await.unwrap();
            let repository_name = project.spec.repositories[1].repo.to_string();
            match mode {
                0 => {
                    project.spec.repositories.clear();
                    backend
                        .definitions::<Project>("flotilla")
                        .apply(&InputMeta::builder().name("app".into()).build(), &project.spec)
                        .await
                        .unwrap();
                }
                1 => {
                    let repositories = backend.using::<Repository>("flotilla");
                    let repository = repositories.get(&repository_name).await.unwrap();
                    repositories
                        .update_status(&repository_name, &repository.metadata.resource_version, &RepositoryStatus::default())
                        .await
                        .unwrap();
                }
                _ => {
                    backend.using::<Repository>("flotilla").delete(&repository_name).await.unwrap();
                }
            }
            service.reconcile_project_image_layers().await.unwrap();
            assert!(service.image_layer_refresh.lock().await.conditions().is_empty(), "obsolete home mode {mode} must lose its refusal");
            assert_eq!(source.reads.lock().unwrap().len(), 1);
        }
    }

    // Host heartbeat diagnoses are capped at 4096 UTF-8 bytes, preserving
    // project identity and the trailing lock cause across short/long errors.
    #[hegel::test]
    fn image_layer_diagnosis_bounds_long_unicode_errors(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Empty through 9000 bytes crosses the cap and multibyte boundaries.
        let repeats = tc.draw(gs::integers::<usize>().min_value(0).max_value(1500));
        let error = format!("git progress: {} fatal: /cache/shallow.lock: File exists", "é界\n".repeat(repeats));
        let mut state = LayerRefreshState::default();
        state.refused("ns", "app", &CharterSource::LocalDirectory { directory: "/source".into() }, error.clone());
        let conditions = state.conditions();
        let message = &conditions[0].message;
        assert!(message.len() <= 4096, "heartbeat message grew to {} bytes", message.len());
        assert!(message.contains("project ns/app") && message.contains("/cache/shallow.lock"));
        if error.len() < 3000 {
            assert!(message.contains(&error));
        }
    }

    // Across duplicate, changed-error, and changed-source sequences the retry
    // delay grows only for identical refusals and stays within 60..=3600s.
    #[hegel::test]
    fn refusal_delay_is_bounded_and_resets(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(20));
        let changes: Vec<_> = (0..count).map(|_| tc.draw(gs::integers::<u8>().min_value(0).max_value(2))).collect();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap();
        runtime.block_on(async {
            let mut state = LayerRefreshState::default();
            let mut directory = String::from("/initial");
            let mut error = String::from("initial");
            let mut expected = 0u64;
            for (step, change) in changes.into_iter().enumerate() {
                if change == 1 {
                    directory = format!("/source-{step}");
                }
                if change == 2 {
                    error = format!("error-{step}");
                }
                expected = if change == 0 && expected != 0 { (expected * 2).min(3600) } else { 60 };
                let source = CharterSource::LocalDirectory { directory: directory.clone() };
                state.refused("ns", "app", &source, error.clone());
                assert!(state.deferred("ns", "app", &source));
                assert!(!state.deferred("ns", "app", &CharterSource::LocalDirectory { directory: "/other".into() }));
                assert_eq!(state.conditions().len(), 1);
                tokio::time::advance(std::time::Duration::from_secs(expected - 1)).await;
                assert!(state.deferred("ns", "app", &source));
                tokio::time::advance(std::time::Duration::from_secs(1)).await;
                assert!(!state.deferred("ns", "app", &source));
            }
        });
    }

    // ADR 0062 (B3): the primary repository's `.flotilla/image/` at its
    // default branch becomes the project's layer; unchanged definitions are
    // never rewritten, a version bump replaces the layer, an invalid layer
    // keeps the previous one, and removing the directory removes the layer.
    #[tokio::test(start_paused = true)]
    async fn primary_repository_layer_follows_the_default_branch() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let observed = ResourceBackend::new(InMemoryBackend::observed());
        seed(&backend, None).await;
        let source = Arc::new(LayerSource::default());
        *source.files.lock().expect("files") = Some(layer_files("1.0.0"));
        let operations = Operations::with_inspector(source.clone());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let roots = flotilla_store::OldStoreManifestRoots::new(backend.clone());
        let service = ProjectService {
            image_layer_refresh: Default::default(),
            resource_backend: &backend,
            manifest_roots: &roots,
            observed_resource_backend: &observed,
            clock: &clock,
            namespace: &namespace,
            repository_index: RepositoryIndex {
                lifecycle_reads: &flotilla_resources::lifecycle_reads::NoLifecycleReads,
                backend: &backend,
                observed: &observed,
                namespace: &namespace,
                host: "local-host",
            },
            operations: &operations,
        };
        let layers = backend.using::<ImageLayer>("flotilla");
        // The shipped stdlib layers are always visible beside the stored ones; only the stored
        // (project) layers are under test here.
        let names = || async {
            backend
                .definitions::<ImageLayer>("flotilla")
                .list()
                .await
                .expect("layers")
                .into_iter()
                .map(|layer| layer.metadata.name)
                .filter(|name| !flotilla_resources::builtins::is_builtin("ImageLayer", name))
                .collect::<Vec<_>>()
        };

        service.reconcile_project_image_layers().await.expect("sync");
        assert_eq!(
            source.reads.lock().expect("reads").last(),
            Some(&CharterSource::Repository {
                repo: "https://github.com/example/app".into(),
                branch: "main".into(),
                path: PROJECT_IMAGE_LAYER_PATH.into()
            })
        );
        let layer = layers.get(&layer_name("1.0.0")).await.expect("project layer");
        let spec = layer.spec.current().expect("current");
        assert_eq!((spec.source, spec.project.as_deref(), spec.tier), (ImageLayerSource::Project, Some("app"), ImageLayerTier::Project));
        assert_eq!(spec.context_files["apt.txt"].content, "libx11-dev\n");
        service.reconcile_project_image_layers().await.expect("unchanged");
        assert_eq!(layers.get(&layer_name("1.0.0")).await.expect("layer").metadata.resource_version, layer.metadata.resource_version);

        *source.files.lock().expect("files") = Some(layer_files("1.1.0"));
        service.reconcile_project_image_layers().await.expect("bump");
        assert_eq!(names().await, [layer_name("1.1.0")]);

        let mut invalid = layer_files("1.2.0");
        invalid.insert("Dockerfile.fragment".into(), b"FROM ubuntu\n".to_vec());
        *source.files.lock().expect("files") = Some(invalid);
        service.reconcile_project_image_layers().await.expect("refusal is reported per project");
        assert_eq!(names().await, [layer_name("1.1.0")], "an invalid layer keeps the previous one");

        *source.files.lock().expect("files") = None;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        service.reconcile_project_image_layers().await.expect("opt out");
        assert!(names().await.is_empty(), "removing the directory removes the layer");
    }

    fn service<'a>(
        backend: &'a ResourceBackend,
        observed: &'a ResourceBackend,
        clock: &'a Arc<dyn Clock>,
        namespace: &'a std::sync::RwLock<String>,
        operations: &'a Operations,
        roots: &'a flotilla_store::OldStoreManifestRoots,
    ) -> ProjectService<'a> {
        ProjectService {
            image_layer_refresh: Default::default(),
            resource_backend: backend,
            manifest_roots: roots,
            observed_resource_backend: observed,
            clock,
            namespace,
            repository_index: RepositoryIndex {
                lifecycle_reads: &flotilla_resources::lifecycle_reads::NoLifecycleReads,
                backend,
                observed,
                namespace,
                host: "local-host",
            },
            operations,
        }
    }

    // A charter-delegated project's primary repository is still read; a
    // layer its charter also authored (`<charter path>/image/`) is a
    // within-source clash, refused naming both locations.
    #[tokio::test]
    async fn charter_projects_read_their_primary_repository_and_refuse_a_second_layer() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let observed = ResourceBackend::new(InMemoryBackend::observed());
        seed(&backend, Some(CharterPointer::Inline { documents: Vec::new(), files: BTreeMap::new() })).await;
        let source = Arc::new(LayerSource::default());
        *source.files.lock().expect("files") = Some(layer_files("1.0.0"));
        let operations = Operations::with_inspector(source.clone());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let roots = flotilla_store::OldStoreManifestRoots::new(backend.clone());
        let service = service(&backend, &observed, &clock, &namespace, &operations, &roots);
        service.reconcile_project_image_layers().await.expect("sync");
        let layers = backend.definitions::<ImageLayer>("flotilla");
        assert!(layers.get(&layer_name("1.0.0")).await.is_ok(), "charter projects read their primary repository");

        let mut charter = parse_layer_directory(&layer_files("2.0.0"), ImageLayerSource::Project, Some("app")).expect("charter layer");
        charter.name = "app-charter".into();
        layers
            .apply(
                &InputMeta::builder()
                    .name(charter.resource_name())
                    .labels(BTreeMap::from([(MANAGED_BY_LABEL.to_string(), "manifest".to_string())]))
                    .build(),
                &charter.clone().into(),
            )
            .await
            .expect("charter-authored layer");
        *source.files.lock().expect("files") = Some(layer_files("1.1.0"));
        let primary = CharterSource::Repository {
            repo: "https://github.com/example/app".into(),
            branch: "main".into(),
            path: PROJECT_IMAGE_LAYER_PATH.into(),
        };
        let error = service.sync_project_image_layer("flotilla", "app", &primary).await.expect_err("clash");
        assert!(error.contains("defines its image layer twice") && error.contains(&charter.resource_name()), "{error}");
        assert!(layers.get(&layer_name("1.1.0")).await.is_err(), "the clashing layer is not applied");
    }
}
