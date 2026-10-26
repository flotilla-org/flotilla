//! Builder-side ImageBuild support (ADR 0062). Admission may author the
//! per-identity ImageBuild on any root; the declared builder projects that
//! demand into its own store so its build service writes execution evidence
//! at its own authority, and replication returns the evidence to consumers.
//! Projection is two-way: when the source demand is deleted (the operator
//! retry), the builder's copy is deleted too. This module also holds the
//! daemon's [`ImageBuildMaterials`]: the push credential and build secrets
//! from the credential store, and build logs kept as Artifacts.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_credentials::{BuildSecretError, CredentialStore};
use flotilla_environment::environment::{image::SecretFile, RegistryAuth};
use flotilla_protocol::NodeId;
use flotilla_provisioning::reconcilers::ImageBuildMaterials;
use flotilla_resources::{
    Artifact, ArtifactSpec, FleetDesignation, HostImageAction, ImageBuildFailure, ImageBuildFailureClass, InputMeta, ResourceError,
    FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;

use crate::blob_store::BlobStore;

/// Mirror build demands onto their builder host, following Vessel placement's
/// projection convention, and delete projected copies whose demand is gone.
/// Retired ADR 0053 records are never projected.
pub(crate) async fn project_builds(backend: &ResourceBackend, namespace: &str, host_ref: &str) -> Result<(), ResourceError> {
    use flotilla_resources::{ImageBuild, ResourceProvenance, ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION};
    let builds = backend.using::<ImageBuild>(namespace);
    let mut demanded = BTreeSet::new();
    for source in backend.including_replicas::<ImageBuild>(namespace).list().await?.items {
        let ResourceProvenance::Replica { origin_root, .. } = source.provenance else {
            continue;
        };
        let obj = source.object;
        let Some(spec) = obj.spec.current() else { continue };
        if spec.host_ref != host_ref
            || obj.metadata.deletion_timestamp.is_some()
            || obj.metadata.annotations.contains_key(ACTUATOR_SOURCE_ROOT_ANNOTATION)
        {
            continue;
        }
        demanded.insert((origin_root.to_string(), obj.metadata.name.clone()));
        match builds.get(&obj.metadata.name).await {
            Ok(current) => {
                // Copies of one name may disagree under replication lag (a
                // different reservation or delivery view); like
                // `read_image_build`, keep the existing copy rather than
                // failing, since it serves the same identity and architecture.
                if current.spec != obj.spec {
                    tracing::warn!(image_build = %obj.metadata.name, source_root = %origin_root, "image build demand disagrees with the projected copy; keeping the copy");
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                let mut annotations = obj.metadata.annotations;
                annotations.insert(ACTUATOR_HOST_REF_ANNOTATION.into(), host_ref.into());
                annotations.insert(ACTUATOR_SOURCE_ROOT_ANNOTATION.into(), origin_root.to_string());
                builds.create(&InputMeta::builder().name(obj.metadata.name).annotations(annotations).build(), &obj.spec).await?;
            }
            Err(error) => return Err(error),
        }
    }
    // A projected copy whose demand has left its source root's replica is
    // withdrawn: deleting the demand is the operator retry, and a lingering
    // Failed copy would replicate back and be reused. Only a replica with an
    // established cursor attests absence; a replica awaiting its snapshot
    // may merely lag, so its copies are kept.
    for local in builds.list().await?.items {
        let annotations = &local.metadata.annotations;
        let (Some(origin), Some(projected_for)) =
            (annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION), annotations.get(ACTUATOR_HOST_REF_ANNOTATION))
        else {
            continue;
        };
        if projected_for != host_ref
            || local.metadata.deletion_timestamp.is_some()
            || demanded.contains(&(origin.clone(), local.metadata.name.clone()))
        {
            continue;
        }
        if backend.replica_writer::<ImageBuild>(NodeId::new(origin.as_str()), namespace).cursor().await?.is_none() {
            continue;
        }
        tracing::info!(image_build = %local.metadata.name, source_root = %origin, "image build demand withdrawn; deleting projected copy");
        match builds.delete(&local.metadata.name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The daemon's build materials. The push credential is the fleet image
/// cache's `push_credential` (the fleet names it `crew-image-registry-push`),
/// admitted by an `image-push` host-action grant for this builder; build
/// secrets need an `image-build-secret` grant. Neither reaches a crew.
pub(crate) struct DaemonImageBuildMaterials {
    pub credentials: Arc<CredentialStore>,
    pub blobs: Arc<dyn BlobStore>,
    pub backend: ResourceBackend,
    pub namespace: String,
    pub host: String,
    /// Private parent for per-build secret files.
    pub secrets: PathBuf,
}

fn infrastructure(reason: String) -> ImageBuildFailure {
    ImageBuildFailure { class: ImageBuildFailureClass::Infrastructure, reason }
}

fn recipe(reason: String) -> ImageBuildFailure {
    ImageBuildFailure { class: ImageBuildFailureClass::Recipe, reason }
}

/// Stage a secret in its own private directory, which is removed (with the
/// file) when the returned owner drops at the end of the build.
fn write_secret(parent: &Path, name: &str, material: &[u8]) -> Result<(PathBuf, tempfile::TempDir), String> {
    std::fs::create_dir_all(parent).map_err(|error| format!("build secret directory {}: {error}", parent.display()))?;
    // Removed on drop; made private before the material is written.
    let directory = tempfile::Builder::new().prefix("flotilla-build-secret-").tempdir_in(parent).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
    }
    let path = directory.path().join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| error.to_string())?;
    std::io::Write::write_all(&mut file, material).map_err(|error| error.to_string())?;
    Ok((path, directory))
}

#[async_trait]
impl ImageBuildMaterials for DaemonImageBuildMaterials {
    async fn push_auth(&self, repository: &str) -> Result<RegistryAuth, ImageBuildFailure> {
        // Read the binding per attempt: a rotated or newly installed push
        // credential applies to the next retry.
        let cache = match self.backend.definitions::<FleetDesignation>(&self.namespace).get(FLEET_DESIGNATION_NAME).await {
            Ok(fleet) => fleet.spec.image_cache,
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(infrastructure(format!("fleet designation unavailable: {error}"))),
        }
        .ok_or_else(|| infrastructure("registry delivery needs the fleet image cache's push credential".into()))?;
        self.credentials
            .registry_auth(&self.host, HostImageAction::ImagePush, &cache.push_credential, repository)
            .await
            .map_err(|error| infrastructure(format!("push credential `{}`: {error}", cache.push_credential)))
    }

    async fn secret(&self, name: &str) -> Result<SecretFile, ImageBuildFailure> {
        let material = self.credentials.build_secret(&self.host, name).await.map_err(|error| match error {
            BuildSecretError::Refused(reason) => recipe(reason),
            BuildSecretError::Unavailable(reason) => infrastructure(reason),
        })?;
        let (path, owner) = write_secret(&self.secrets, name, material.expose())
            .map_err(|error| infrastructure(format!("build secret `{name}` could not be staged: {error}")))?;
        Ok(SecretFile::new(path, owner))
    }

    async fn keep_log(&self, build: &str, attempt: u32, log: &str) -> Result<String, String> {
        let name = format!("image-build-log-{build}-{attempt}");
        let digest = self.blobs.put_with_media_type(log.as_bytes(), "text/plain").await?;
        let now = Utc::now();
        let artifact = ArtifactSpec::builder()
            .convoy(String::new())
            .producer("image-build".into())
            .kind("build-log".into())
            .subject(build.into())
            .digest(digest.as_str().into())
            .size(log.len() as u64)
            .media_type("text/plain".into())
            .recorded_at(now)
            .expires_at(now + chrono::Duration::days(30))
            .build();
        match self.backend.using::<Artifact>(&self.namespace).create(&InputMeta::builder().name(name.clone()).build(), &artifact).await {
            Ok(_) => Ok(name),
            // A resumed attempt keeps the log first recorded under its name.
            Err(ResourceError::Conflict { .. }) => Ok(name),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_protocol::NodeId;
    use flotilla_resources::{
        image_identity, ImageBuild, ImageBuildPhase, ImageBuildReservation, ImageBuildSpec, ImageBuildStatusPatch, ImageDelivery,
        ImageLayerSource, ImageLayerSpec, ImageLayerTier,
    };
    use flotilla_store::{apply_status_patch, read_image_build};

    use super::*;

    fn spec(host_ref: &str) -> ImageBuildSpec {
        let layers = vec![ImageLayerSpec::builder()
            .name("base".into())
            .version("1".into())
            .tier(ImageLayerTier::Base)
            .source(ImageLayerSource::Stdlib)
            .from(format!("ubuntu@sha256:{}", "1".repeat(64)))
            .fragment("FROM ${BASE}\n".into())
            .probe("true".into())
            .build()];
        ImageBuildSpec::builder()
            .identity(image_identity(&layers))
            .architecture("amd64".into())
            .layers(layers)
            .host_ref(host_ref.into())
            .delivery(ImageDelivery::Local)
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1 })
            .build()
    }

    // A declared builder projects a remote demand only once, refuses a name
    // that already holds a different spec, writes evidence at its own
    // authority, and replication returns that evidence to admission.
    #[tokio::test]
    async fn declared_builder_projects_demand_and_returns_execution_evidence() {
        let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("builder-root"));
        let spec = spec("builder");
        let name = spec.resource_name();
        home.using::<ImageBuild>("test")
            .create(&InputMeta::builder().name(name.clone()).build(), &spec.clone().into())
            .await
            .expect("demand");
        builder
            .replica_writer::<ImageBuild>(NodeId::new("home"), "test")
            .replace(&home.using::<ImageBuild>("test").list().await.expect("demand snapshot"), Utc::now())
            .await
            .expect("replicate demand");
        for _ in 0..2 {
            project_builds(&builder, "test", "builder").await.expect("project");
        }
        let builds = builder.using::<ImageBuild>("test");
        assert_eq!(builds.list().await.expect("list").items.len(), 1);
        project_builds(&builder, "test", "elsewhere").await.expect("other hosts project nothing");
        assert_eq!(builds.list().await.expect("list").items.len(), 1);
        apply_status_patch(&builds, &name, &ImageBuildStatusPatch::Start { at: Utc::now(), attempt: 1 }).await.expect("start");
        apply_status_patch(
            &builds,
            &name,
            &ImageBuildStatusPatch::Built {
                at: Utc::now(),
                digest: format!("sha256:{}", "3".repeat(64)),
                probes: Vec::new(),
                log_ref: "log".into(),
            },
        )
        .await
        .expect("built");
        home.replica_writer::<ImageBuild>(NodeId::new("builder-root"), "test")
            .replace(&builds.list().await.expect("execution snapshot"), Utc::now())
            .await
            .expect("replicate evidence");
        assert_eq!(
            read_image_build(&home, "test", &name, Arc::new(flotilla_resources::lifecycle_reads::NoLifecycleReads))
                .await
                .expect("read execution")
                .build
                .object
                .status
                .expect("evidence")
                .phase,
            ImageBuildPhase::Built
        );
        assert!(home.using::<ImageBuild>("test").get(&name).await.expect("original demand").status.is_none());
    }

    // A local copy whose spec disagrees with a replicated demand of the same
    // name is kept, not overwritten, matching `read_image_build`.
    #[tokio::test]
    async fn name_collisions_are_not_projected() {
        let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("builder-root"));
        let demand = spec("builder");
        let name = demand.resource_name();
        home.using::<ImageBuild>("test").create(&InputMeta::builder().name(name.clone()).build(), &demand.into()).await.expect("demand");
        builder
            .using::<ImageBuild>("test")
            .create(
                &InputMeta::builder().name(name.clone()).build(),
                &ImageBuildSpec { reservation: ImageBuildReservation { cpu: 2, disk_bytes: 1 }, ..spec("builder") }.into(),
            )
            .await
            .expect("existing");
        builder
            .replica_writer::<ImageBuild>(NodeId::new("home"), "test")
            .replace(&home.using::<ImageBuild>("test").list().await.expect("demand snapshot"), Utc::now())
            .await
            .expect("replicate demand");
        project_builds(&builder, "test", "builder").await.expect("project");
        let local = builder.using::<ImageBuild>("test").get(&name).await.expect("local");
        assert_eq!(local.spec.current().expect("current").reservation.cpu, 2);
    }

    // Projection is two-way: deleting the demand on its admitting root (the
    // operator retry for a recipe failure) deletes the builder's copy once
    // the builder's replica of that root attests the absence, so the Failed
    // copy no longer replicates back. A replica that awaits its snapshot
    // (no cursor) may merely lag, so copies survive it.
    #[tokio::test]
    async fn withdrawn_demand_deletes_the_projected_copy_only_on_a_current_replica() {
        let home = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("home"));
        let builder = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default()).with_local_root(NodeId::new("builder-root"));
        let demand = spec("builder");
        let name = demand.resource_name();
        let demands = home.using::<ImageBuild>("test");
        demands.create(&InputMeta::builder().name(name.clone()).build(), &demand.into()).await.expect("demand");
        let replica = builder.replica_writer::<ImageBuild>(NodeId::new("home"), "test");
        replica.replace(&demands.list().await.expect("demand snapshot"), Utc::now()).await.expect("replicate demand");
        project_builds(&builder, "test", "builder").await.expect("project");
        let builds = builder.using::<ImageBuild>("test");
        apply_status_patch(&builds, &name, &ImageBuildStatusPatch::Start { at: Utc::now(), attempt: 1 }).await.expect("start");
        // A copy projected from a root whose replica has no snapshot yet, and
        // an unannotated local build, are never withdrawn.
        let lagging = spec("builder");
        let mut lagging = lagging;
        lagging.architecture = "arm64".into();
        let lagging_name = lagging.resource_name();
        let annotations = std::collections::BTreeMap::from([
            (flotilla_resources::ACTUATOR_HOST_REF_ANNOTATION.to_string(), "builder".to_string()),
            (flotilla_resources::ACTUATOR_SOURCE_ROOT_ANNOTATION.to_string(), "unsynced-root".to_string()),
        ]);
        builds
            .create(&InputMeta::builder().name(lagging_name.clone()).annotations(annotations).build(), &lagging.into())
            .await
            .expect("lagging copy");
        let mut own = spec("builder");
        own.layers[0].version = "2".into();
        own.identity = image_identity(&own.layers);
        let own_name = own.resource_name();
        builds.create(&InputMeta::builder().name(own_name.clone()).build(), &own.into()).await.expect("own build");

        demands.delete(&name).await.expect("operator retry");
        project_builds(&builder, "test", "builder").await.expect("project before the deletion replicates");
        assert!(builds.get(&name).await.is_ok(), "the demand is still present in the replica");
        replica.replace(&demands.list().await.expect("empty snapshot"), Utc::now()).await.expect("replicate deletion");
        assert!(replica.cursor().await.expect("cursor").is_some(), "a snapshot establishes the replica");
        project_builds(&builder, "test", "builder").await.expect("project withdrawal");
        assert!(matches!(builds.get(&name).await, Err(flotilla_resources::ResourceError::NotFound { .. })));
        assert!(builds.get(&own_name).await.is_ok());
        assert!(builds.get(&lagging_name).await.is_ok(), "a root without a replica snapshot may merely lag");
    }

    // Credential and command boundary: minting and staging never run a command.
    struct NoCommands;
    #[async_trait]
    impl flotilla_provider_runtime::CommandRunner for NoCommands {
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &flotilla_provider_runtime::ChannelLabel) -> Result<String, String> {
            panic!("build materials run no commands")
        }
        async fn run_output(
            &self,
            _: &str,
            _: &[&str],
            _: &Path,
            _: &flotilla_provider_runtime::ChannelLabel,
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            panic!("build materials run no commands")
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
    }

    // The daemon's build materials (ADR 0062 A11, B7): the push credential is
    // the fleet image cache's push credential, admitted by an image-push grant
    // to this builder and scoped to the repository; a granted build secret is
    // staged as a private file that disappears with the build, while an
    // ungranted one is a recipe refusal naming it; the build log is kept as a
    // build-log Artifact whose blob holds the log.
    #[tokio::test]
    async fn daemon_materials_resolve_host_credentials_and_keep_logs() {
        use flotilla_resources::{
            CredentialConsumer, CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, CredentialLifecycle, CredentialSource,
            CredentialSpec, CredentialSpecSpec, FleetDesignationSpec, Host, HostActionSelector, HostSpec, ImageBuildCapacity,
            ImageCacheBinding, Project, ProjectSpec,
        };
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let repository = "registry.test/flotilla/crew-images";
        backend
            .definitions::<Project>("test")
            .create(&InputMeta::builder().name("fleet".into()).build(), &ProjectSpec::builder().display_name("Fleet".into()).build())
            .await
            .expect("project");
        backend
            .definitions::<FleetDesignation>("test")
            .create(
                &InputMeta::builder().name(FLEET_DESIGNATION_NAME.into()).build(),
                &FleetDesignationSpec {
                    fleet: None,
                    project: "fleet".into(),
                    image_cache: Some(ImageCacheBinding {
                        repository: repository.into(),
                        pull_credential: "crew-image-registry-pull".into(),
                        push_credential: "crew-image-registry-push".into(),
                    }),
                },
            )
            .await
            .expect("fleet");
        backend
            .using::<Host>("test")
            .create(
                &InputMeta::builder().name("builder".into()).build(),
                &HostSpec {
                    image_build_capacity: Some(ImageBuildCapacity::Builder {
                        architecture: "amd64".into(),
                        slots: 1,
                        reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("host");
        for (name, consumer, env) in [
            (
                "crew-image-registry-push",
                CredentialConsumer::DockerRegistry { registry: "registry.test".into(), username: "builder".into() },
                "TEST_PUSH",
            ),
            ("npm-token", CredentialConsumer::Codex, "TEST_NPM"),
        ] {
            backend
                .definitions::<CredentialSpec>("test")
                .create(
                    &InputMeta::builder().name(name.into()).build(),
                    &CredentialSpecSpec {
                        consumer,
                        source: CredentialSource::Env { name: env.into() },
                        lifecycle: CredentialLifecycle::Static,
                        placement: Default::default(),
                        authorization_epoch: flotilla_resources::INITIAL_AUTHORIZATION_EPOCH,
                    },
                )
                .await
                .expect("credential");
        }
        for (grant, action, credential) in
            [("push", HostImageAction::ImagePush, "crew-image-registry-push"), ("secrets", HostImageAction::ImageBuildSecret, "npm-token")]
        {
            backend
                .definitions::<CredentialGrant>("test")
                .create(
                    &InputMeta::builder().name(grant.into()).build(),
                    &CredentialGrantSpec::builder()
                        .selector(
                            CredentialGrantSelector::builder().host_action(HostActionSelector::builder().action(action).build()).build(),
                        )
                        .credentials(BTreeSet::from([credential.to_string()]))
                        .build(),
                )
                .await
                .expect("grant");
        }
        let state = tempfile::tempdir().expect("state");
        let credentials = Arc::new(CredentialStore::new(
            backend.clone(),
            "test",
            Arc::new(flotilla_discovery_testkit::TestEnvVars::new([("TEST_PUSH", "push-secret"), ("TEST_NPM", "npm-secret")])),
            flotilla_discovery_api::EnvironmentBag::new(),
            Arc::new(NoCommands),
            state.path().into(),
        ));
        let blobs = Arc::new(crate::blob_store::MemoryBlobStore::default());
        let materials = DaemonImageBuildMaterials {
            credentials,
            blobs: blobs.clone(),
            backend: backend.clone(),
            namespace: "test".into(),
            host: "builder".into(),
            secrets: state.path().join("image-builder/secrets"),
        };

        let auth = materials.push_auth(repository).await.expect("push credential");
        auth.validate(repository, HostImageAction::ImagePush).expect("push admitted for the repository");
        assert!(auth.validate("registry.test/other", HostImageAction::ImagePush).is_err());

        let secret = materials.secret("npm-token").await.expect("granted secret");
        let path = secret.path().to_path_buf();
        assert!(path.starts_with(state.path().join("image-builder/secrets")));
        assert_eq!(std::fs::read_to_string(&path).expect("staged"), "npm-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).expect("file").permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(path.parent().expect("dir")).expect("dir").permissions().mode() & 0o777, 0o700);
        }
        drop(secret);
        assert!(!path.exists() && !path.parent().expect("dir").exists(), "staged secrets disappear with the build");
        let refused = materials.secret("forgejo-token").await.expect_err("ungranted");
        assert_eq!(refused.class, ImageBuildFailureClass::Recipe);
        assert!(refused.reason.contains("`forgejo-token`"));

        let log_ref = materials.keep_log("image-abc-amd64", 2, "step 1\n").await.expect("log kept");
        let artifact = backend.using::<Artifact>("test").get(&log_ref).await.expect("artifact");
        assert_eq!(artifact.spec.kind, "build-log");
        assert_eq!(artifact.spec.subject, "image-abc-amd64");
        let digest = crate::blob_store::BlobDigest::parse(&artifact.spec.digest).expect("digest");
        assert_eq!(crate::blob_store::BlobStore::get(blobs.as_ref(), &digest).await.expect("blob"), Some(b"step 1\n".to_vec()));
    }
}
