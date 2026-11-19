//! The daemon's [`ImageBuildMaterials`] (ADR 0062): the push credential and build secrets from
//! the credential store, and build logs kept as Artifacts.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_credentials::{BuildSecretError, CredentialStore};
use flotilla_environment::environment::{image::SecretFile, RegistryAuth};
use flotilla_provisioning::engine::image_build_actuator::ImageBuildMaterials;
use flotilla_resources::{
    Artifact, ArtifactSpec, FleetDesignation, HostImageAction, ImageBuildFailure, ImageBuildFailureClass, InputMeta, ResourceError,
    FLEET_DESIGNATION_NAME,
};
use flotilla_store::ResourceBackend;

use crate::blob_store::BlobStore;

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

    async fn keep_log(&self, build: &str, name: &str, log: &str) -> Result<String, String> {
        let name = name.to_owned();
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
    use std::collections::BTreeSet;

    use flotilla_resources::ImageBuildReservation;

    use super::*;

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
            CredentialSpec, CredentialSpecSpec, FleetDesignationSpec, HostActionSelector, HostSpec, ImageBuildCapacity, ImageCacheBinding,
            Project, ProjectSpec,
        };
        let home = flotilla_provisioning_testkit::EngineBackend::open_in("home", "test");
        let backend = home.backend.clone();
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
        // The builder publishes its Host at its own daemon; this home receives it.
        home.put_host_at_its_home(
            "builder",
            &HostSpec {
                image_build_capacity: Some(ImageBuildCapacity::Builder {
                    architecture: "amd64".into(),
                    slots: 1,
                    reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1 },
                }),
                ..Default::default()
            },
            None,
        )
        .await;
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

        let log_ref = materials.keep_log("image-abc-amd64", "image-build-log-image-abc-amd64-3-2", "step 1\n").await.expect("log kept");
        assert_eq!(log_ref, "image-build-log-image-abc-amd64-3-2");
        let artifact = backend.using::<Artifact>("test").get(&log_ref).await.expect("artifact");
        assert_eq!(artifact.spec.kind, "build-log");
        assert_eq!(artifact.spec.subject, "image-abc-amd64");
        let digest = crate::blob_store::BlobDigest::parse(&artifact.spec.digest).expect("digest");
        assert_eq!(crate::blob_store::BlobStore::get(blobs.as_ref(), &digest).await.expect("blob"), Some(b"step 1\n".to_vec()));
    }
}
