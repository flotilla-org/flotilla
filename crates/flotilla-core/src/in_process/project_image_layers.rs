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
    parse_layer_directory, CharterSource, ImageLayer, ImageLayerSource, InputMeta, Project, ProjectRepositoryRole, Repository,
    ResourceError, ResourceObject, MANAGED_BY_LABEL,
};
use tracing::warn;

use super::project_ops::{repository_transport_url_with_backend, ProjectService};

/// Where one project's layer directory lives, as seen from this host.
#[derive(Debug, PartialEq, Eq)]
enum LayerHome {
    /// Read the directory here.
    Source(CharterSource),
    /// Nothing can be read yet (no default branch); leave the layer as it is.
    Elsewhere,
}

impl ProjectService<'_> {
    /// Sync every project's layer directory this host is responsible for.
    /// Each project's failure is reported and leaves its previous layer.
    pub(super) async fn reconcile_project_image_layers(&self) -> Result<(), String> {
        let namespace = self.namespace.read().expect("provisioning namespace lock poisoned").clone();
        let local = self
            .resource_backend
            .using::<Project>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|project| project.metadata.name)
            .collect::<std::collections::BTreeSet<_>>();
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        for project in projects {
            if project.metadata.deletion_timestamp.is_some() || !local.contains(&project.metadata.name) {
                continue;
            }
            let home = match self.layer_home(&namespace, &project).await {
                Ok(Some(home)) => home,
                Ok(None) => continue,
                Err(error) => {
                    warn!(project = %project.metadata.name, %error, "project image layer source unavailable");
                    continue;
                }
            };
            if let LayerHome::Source(source) = home {
                if let Err(error) = self.sync_project_image_layer(&namespace, &project.metadata.name, &source).await {
                    warn!(project = %project.metadata.name, %error, "project image layer refused; keeping the previous layer");
                }
            }
        }
        Ok(())
    }

    /// The primary (first code) repository, read by the project's home.
    /// `None`: the project has no code repository.
    async fn layer_home(&self, namespace: &str, project: &ResourceObject<Project>) -> Result<Option<LayerHome>, String> {
        let Some(primary) = project.spec.repositories.iter().find(|member| member.roles.contains(&ProjectRepositoryRole::Code)) else {
            return Ok(None);
        };
        let repository = self
            .resource_backend
            .including_replicas::<Repository>(namespace)
            .get(&primary.repo.to_string())
            .await
            .map_err(|error| format!("repository {}: {error}", primary.repo))?
            .object;
        let Some(branch) =
            primary.default_branch.clone().or_else(|| repository.status.as_ref().and_then(|status| status.default_branch.clone()))
        else {
            return Ok(Some(LayerHome::Elsewhere));
        };
        let repo = repository_transport_url_with_backend(self.resource_backend, namespace, &repository.spec).await?;
        let path = primary.subpath.as_deref().map_or_else(
            || PROJECT_IMAGE_LAYER_PATH.to_string(),
            |subpath| format!("{}/{PROJECT_IMAGE_LAYER_PATH}", subpath.trim_end_matches('/')),
        );
        Ok(Some(LayerHome::Source(CharterSource::Repository { repo, branch, path })))
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
        in_process::project_ops::{ProjectOperations, RepositoryIndex},
        repository_inspection::{RepositoryInspection, RepositoryInspector},
    };

    /// Stands in for reading a layer directory through the VCS seam: one
    /// directory content for every source, recording what was read.
    #[derive(Default)]
    struct LayerSource {
        files: Mutex<Option<BTreeMap<String, Vec<u8>>>>,
        reads: Mutex<Vec<CharterSource>>,
    }

    #[async_trait]
    impl RepositoryInspector for LayerSource {
        async fn inspect_path(&self, _: &Path, _: Option<&str>) -> Result<RepositoryInspection, String> {
            Err("unexpected inspection".into())
        }

        async fn image_layer_snapshot(&self, source: &CharterSource) -> Result<Option<LayerDirectorySnapshot>, String> {
            self.reads.lock().expect("reads").push(source.clone());
            Ok(self.files.lock().expect("files").clone().map(|files| LayerDirectorySnapshot { revision: "commit".into(), files }))
        }
    }

    struct Operations(Arc<LayerSource>);

    #[async_trait]
    impl ProjectOperations for Operations {
        fn remember_bootstrap_checkout(&self, _: &RepositoryInspection) -> Result<(), String> {
            Ok(())
        }
        async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
            Ok(Arc::clone(&self.0) as Arc<dyn RepositoryInspector>)
        }
        async fn inspect_repository_path(&self, _: &Path, _: Option<&str>) -> Result<RepositoryInspection, String> {
            Err("unexpected inspection".into())
        }
        async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
            RepositorySpec::remote(remote)
        }
        async fn resolve_forge_identity(&self, spec: RepositorySpec) -> Result<RepositorySpec, String> {
            Ok(spec)
        }
        async fn reap_ensured_convoy(&self, _: &str, _: &str, _: &str, _: bool) -> Result<(), String> {
            Err("unexpected reap".into())
        }
    }

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

    // ADR 0062 (B3): the primary repository's `.flotilla/image/` at its
    // default branch becomes the project's layer; unchanged definitions are
    // never rewritten, a version bump replaces the layer, an invalid layer
    // keeps the previous one, and removing the directory removes the layer.
    #[tokio::test]
    async fn primary_repository_layer_follows_the_default_branch() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let observed = ResourceBackend::new(InMemoryBackend::observed());
        seed(&backend, None).await;
        let source = Arc::new(LayerSource::default());
        *source.files.lock().expect("files") = Some(layer_files("1.0.0"));
        let operations = Operations(Arc::clone(&source));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let service = ProjectService {
            resource_backend: &backend,
            observed_resource_backend: &observed,
            clock: &clock,
            namespace: &namespace,
            repository_index: RepositoryIndex { backend: &backend, observed: &observed, namespace: &namespace, host: "local-host" },
            operations: &operations,
        };
        let layers = backend.using::<ImageLayer>("flotilla");
        let names = || async {
            backend
                .definitions::<ImageLayer>("flotilla")
                .list()
                .await
                .expect("layers")
                .into_iter()
                .map(|layer| layer.metadata.name)
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
        service.reconcile_project_image_layers().await.expect("opt out");
        assert!(names().await.is_empty(), "removing the directory removes the layer");
    }

    fn service<'a>(
        backend: &'a ResourceBackend,
        observed: &'a ResourceBackend,
        clock: &'a Arc<dyn Clock>,
        namespace: &'a std::sync::RwLock<String>,
        operations: &'a Operations,
    ) -> ProjectService<'a> {
        ProjectService {
            resource_backend: backend,
            observed_resource_backend: observed,
            clock,
            namespace,
            repository_index: RepositoryIndex { backend, observed, namespace, host: "local-host" },
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
        let operations = Operations(Arc::clone(&source));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let namespace = std::sync::RwLock::new("flotilla".to_string());
        let service = service(&backend, &observed, &clock, &namespace, &operations);
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
