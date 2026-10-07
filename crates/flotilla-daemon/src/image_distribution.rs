//! Digest availability and delivery. Build evidence is immutable; inventory and
//! registry publication are observations which may change independently.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use flotilla_core::providers::{ChannelLabel, CommandRunner};
use flotilla_resources::{
    is_image_digest, FleetDesignation, Host, HostImageAction, ImageBuild, ImageBuildPhase, ImageCacheBinding, PlacedImageIdentity,
    ResourceBackend, ResourceError, ResourceObject, FLEET_DESIGNATION_NAME, IMAGE_DIGESTS_CAPABILITY,
};

use crate::credential::CredentialStore;

const RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// The Docker process seam. All methods operate on exact IDs, never mutable tags.
/// Returning from pull does not attest identity: the caller inspects it.
#[async_trait]
pub(crate) trait ImageDistributionIo: Send + Sync {
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
    async fn remove_local(&self, _id: &str, _tags: &BTreeSet<String>) -> Result<(), String> {
        Err("local image collection unavailable".into())
    }
    async fn remove_registry(&self, _cache: &ImageCacheBinding, _credential: &str, _reference: &str) -> Result<(), String> {
        Err("registry image collection unavailable".into())
    }
    async fn disk_free(&self) -> Result<u64, String> {
        Err("Docker storage accounting unavailable".into())
    }
    async fn inspect(&self, reference: &str) -> Result<PlacedImageIdentity, String>;
    async fn push(&self, cache: &ImageCacheBinding, image_id: &str) -> Result<String, String>;
    async fn pull(&self, cache: &ImageCacheBinding, reference: &str) -> Result<(), String>;
}

#[derive(bon::Builder)]
pub(crate) struct ImageDistributor<I> {
    pub io: Arc<I>,
    pub backend: ResourceBackend,
    pub namespace: String,
    pub host: String,
    #[builder(default)]
    pub jobs: tokio::sync::Mutex<BTreeMap<String, tokio::task::JoinHandle<Result<PlacedImageIdentity, String>>>>,
    #[builder(default)]
    pub(crate) publications: tokio::sync::Mutex<BTreeMap<String, tokio::task::JoinHandle<Result<String, String>>>>,
    #[builder(default)]
    retries: tokio::sync::Mutex<BTreeMap<String, Instant>>,
    #[builder(default)]
    pub(crate) next_collection: tokio::sync::Mutex<Option<Instant>>,
    /// Pending durable health publication must not lose deletion evidence.
    #[builder(default)]
    pub(crate) collection_deleted_registry: tokio::sync::Mutex<BTreeSet<String>>,
    /// Advance even on removal failures so bounded batches cannot starve.
    #[builder(default)]
    pub(crate) collection_cursors: tokio::sync::Mutex<(Option<String>, Option<String>)>,
    #[builder(default)]
    pub(crate) collection_gate: Arc<tokio::sync::RwLock<()>>,
}

impl<I: ImageDistributionIo + 'static> ImageDistributor<I> {
    /// Transfers are asynchronous so Environment's watch loop keeps progressing.
    pub async fn request(self: &Arc<Self>, build: &ResourceObject<ImageBuild>) -> Result<Option<PlacedImageIdentity>, String> {
        let name = build.metadata.name.clone();
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get(&name) {
            if !job.is_finished() {
                return Err("digest delivery in progress".into());
            }
            let result = jobs.remove(&name).expect("finished delivery").await.map_err(|error| error.to_string()).and_then(|result| result);
            if result.is_err() {
                self.retries.lock().await.insert(format!("delivery:{name}"), Instant::now() + RETRY_COOLDOWN);
            }
            return result.map(Some);
        }
        if !retry_ready(self.retries.lock().await.get(&format!("delivery:{name}")).copied(), Instant::now()) {
            return Err("digest delivery retry cooldown".into());
        }
        self.retries.lock().await.remove(&format!("delivery:{name}"));
        let this = Arc::clone(self);
        let build = build.clone();
        jobs.insert(name, tokio::spawn(async move { this.ensure(&build).await }));
        Err("digest delivery in progress".into())
    }

    async fn cache(&self) -> Result<Option<ImageCacheBinding>, String> {
        match self.backend.definitions::<FleetDesignation>(&self.namespace).get(FLEET_DESIGNATION_NAME).await {
            Ok(fleet) => Ok(fleet.spec.image_cache),
            Err(ResourceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Prefer the held exact ID, then a published registry digest; otherwise wait.
    /// Never change a build's identity to whatever a tag happens to name.
    pub async fn ensure(&self, build: &ResourceObject<ImageBuild>) -> Result<PlacedImageIdentity, String> {
        let _gate = self.collection_gate.read().await;
        let current = flotilla_resources::read_image_build(&self.backend, &self.namespace, &build.metadata.name)
            .await
            .map_err(|error| error.to_string())?;
        if current.status.as_ref().is_some_and(|status| status.availability.retired) {
            return Err("image build availability retired".into());
        }
        let status = build
            .status
            .as_ref()
            .filter(|status| status.phase == ImageBuildPhase::Built && !status.availability.retired)
            .ok_or("image build is not built")?;
        let expected = status.identity.as_ref().ok_or("built image has no identity")?;
        if !is_image_digest(&expected.local_image_id) {
            return Err("built image has no valid local digest".into());
        }
        if self.io.inventory().await?.contains(&expected.local_image_id) {
            return self.verify(&expected.local_image_id, expected, None).await;
        }
        if let Some(cache) = self.cache().await? {
            let reference = status.availability.registry_ref.as_ref().ok_or("builder has not published the image to the declared cache")?;
            validate_registry_reference(&cache, reference)?;
            self.io.pull(&cache, reference).await?;
            self.verify(reference, expected, Some(reference)).await
        } else {
            Err("requested digest is not held on this host and no fleet registry cache is declared; waiting for image availability".into())
        }
    }

    async fn verify(&self, reference: &str, expected: &PlacedImageIdentity, registry: Option<&str>) -> Result<PlacedImageIdentity, String> {
        let actual = self.io.inspect(reference).await?;
        if actual.local_image_id != expected.local_image_id {
            return Err("delivered image local digest does not match immutable build evidence".into());
        }
        if let Some(reference) = registry {
            let digest = reference.rsplit_once('@').ok_or("registry reference has no digest")?.1;
            if actual.registry_digest.as_deref() != Some(digest) {
                return Err("pulled registry manifest digest does not match published availability".into());
            }
        }
        Ok(actual)
    }

    /// Only the execution's actuator updates its availability. It derives host
    /// locations from their latest inventories rather than trusting old delivery
    /// successes forever.
    pub async fn refresh(&self) -> Result<(), String> {
        let _gate = self.collection_gate.read().await;
        let held = self.io.inventory().await?;
        let hosts = self.backend.using::<Host>(&self.namespace);
        let host = hosts.get(&self.host).await.map_err(|error| error.to_string())?;
        let mut status = host.status.clone().unwrap_or_default();
        status.capabilities.insert(IMAGE_DIGESTS_CAPABILITY.into(), serde_json::to_value(&held).map_err(|error| error.to_string())?);
        if host.status.as_ref() != Some(&status) {
            hosts.update_status(&self.host, &host.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
        }
        let inventories = self.backend.including_replicas::<Host>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let cache = self.cache().await?;
        let builds = self.backend.using::<ImageBuild>(&self.namespace);
        for build in builds.list().await.map_err(|error| error.to_string())?.items {
            if build.spec.host_ref != self.host {
                continue;
            }
            let result: Result<(), String> = async {
                let Some(mut status) =
                    build.status.clone().filter(|status| status.phase == ImageBuildPhase::Built && !status.availability.retired)
                else {
                    return Ok(());
                };
                let identity = status.identity.as_ref().ok_or("built image has no identity")?;
                status.availability.hosts = inventories
                    .items
                    .iter()
                    .filter_map(|source| {
                        let object = &source.object;
                        let digests = object.status.as_ref()?.capabilities.get(IMAGE_DIGESTS_CAPABILITY)?;
                        let digests: BTreeSet<String> = serde_json::from_value(digests.clone()).ok()?;
                        digests.contains(&identity.local_image_id).then(|| object.metadata.name.clone())
                    })
                    .collect();
                if held.contains(&identity.local_image_id) {
                    if let Some(cache) = &cache {
                        if status
                            .availability
                            .registry_ref
                            .as_ref()
                            .is_none_or(|reference| validate_registry_reference(cache, reference).is_err())
                        {
                            let mut publications = self.publications.lock().await;
                            if publications.get(&build.metadata.name).is_some_and(|job| job.is_finished()) {
                                let result = publications
                                    .remove(&build.metadata.name)
                                    .expect("finished publication")
                                    .await
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result)
                                    .and_then(|reference| {
                                        validate_registry_reference(cache, &reference)?;
                                        Ok(reference)
                                    });
                                match result {
                                    Ok(reference) => {
                                        status.availability.registry_ref = Some(reference);
                                        status.availability.failure = None;
                                    }
                                    Err(reason) => {
                                        status.availability.failure = Some(reason.chars().take(2048).collect());
                                        self.retries
                                            .lock()
                                            .await
                                            .insert(format!("publication:{}", build.metadata.name), Instant::now() + RETRY_COOLDOWN);
                                    }
                                }
                            } else if !publications.contains_key(&build.metadata.name)
                                && retry_ready(
                                    self.retries.lock().await.get(&format!("publication:{}", build.metadata.name)).copied(),
                                    Instant::now(),
                                )
                            {
                                let io = Arc::clone(&self.io);
                                let cache = cache.clone();
                                let id = identity.local_image_id.clone();
                                let gate = Arc::clone(&self.collection_gate);
                                publications.insert(
                                    build.metadata.name.clone(),
                                    tokio::spawn(async move {
                                        let _gate = gate.read().await;
                                        io.push(&cache, &id).await
                                    }),
                                );
                            }
                        }
                    }
                }
                if build.status.as_ref() != Some(&status) {
                    builds
                        .update_status(&build.metadata.name, &build.metadata.resource_version, &status)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            }
            .await;
            if let Err(reason) = result {
                tracing::warn!(build = %build.metadata.name, %reason, "image availability refresh failed; continuing with other builds");
            }
        }
        Ok(())
    }
}

fn retry_ready(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_none_or(|deadline| now >= deadline)
}

fn validate_registry_reference(cache: &ImageCacheBinding, reference: &str) -> Result<(), String> {
    let (repository, digest) = reference.rsplit_once('@').ok_or("cache image reference must pin a manifest digest")?;
    if repository != cache.repository || !is_image_digest(digest) {
        return Err("published image does not match the declared cache and manifest digest".into());
    }
    Ok(())
}

#[derive(bon::Builder)]
pub(crate) struct DockerImageIo {
    pub runner: Arc<dyn CommandRunner>,
    pub credentials: Arc<CredentialStore>,
    pub host: String,
}

impl DockerImageIo {
    async fn run(&self, args: &[&str]) -> Result<String, String> {
        // Even inventory/save/load never consult the host's global Docker config.
        let config = tempfile::tempdir().map_err(|error| error.to_string())?;
        let directory = config.path().to_string_lossy();
        let mut private = vec!["--config", &directory];
        private.extend_from_slice(args);
        self.runner.run_with_timeout("docker", &private, Path::new("/"), &ChannelLabel::Default, Duration::from_secs(30)).await
    }
}

#[async_trait]
impl ImageDistributionIo for DockerImageIo {
    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        let output = self.run(&["image", "ls", "--no-trunc", "--digests", "--format", "{{.ID}} {{.Digest}}"]).await?;
        // Docker local IDs and registry manifest digests are distinct namespaces.
        // Report both because placement checks the corresponding immutable identity.
        Ok(output.split_whitespace().filter(|line| is_image_digest(line)).map(String::from).collect())
    }

    async fn remove_local(&self, id: &str, tags: &BTreeSet<String>) -> Result<(), String> {
        if !is_image_digest(id) {
            return Err("collection requires a full local image ID".into());
        }
        // Docker refuses ID removal when several tags point at it. Remove only
        // build/publication handles proven from immutable Flotilla evidence.
        let containers = self.run(&["ps", "-a", "--filter", &format!("ancestor={id}"), "--format", "{{.ID}}"]).await?;
        if !containers.trim().is_empty() {
            return Err("container still references image".into());
        }
        let output = self.run(&["image", "inspect", "--format", "{{json .RepoTags}}", id]).await?;
        let actual: Option<Vec<String>> = serde_json::from_str(&output).map_err(|error| error.to_string())?;
        let actual = actual.unwrap_or_default();
        if actual.iter().any(|tag| !tags.contains(tag)) {
            return Err("image has tags outside Flotilla ownership".into());
        }
        for tag in actual {
            if self.inspect(&tag).await?.local_image_id != id {
                return Err("collection tag identity changed".into());
            }
            self.run(&["image", "rm", &tag]).await?;
        }
        if self.inventory().await?.contains(id) {
            self.run(&["image", "rm", id]).await?;
        }
        Ok(())
    }

    async fn remove_registry(&self, cache: &ImageCacheBinding, credential: &str, reference: &str) -> Result<(), String> {
        validate_registry_reference(cache, reference)?;
        self.credentials
            .image_registry_operation(&self.host, HostImageAction::ImageDelete, credential, &cache.repository, &["delete", reference])
            .await
            .map(|_| ())
    }

    async fn disk_free(&self) -> Result<u64, String> {
        let root = self.run(&["info", "--format", "{{.DockerRootDir}}"]).await?;
        let output = self.runner.run("df", &["-B1", "--output=avail", root.trim()], Path::new("/"), &ChannelLabel::Default).await?;
        output.split_whitespace().last().ok_or("df has no free space")?.parse::<u64>().map_err(|error| error.to_string())
    }

    async fn inspect(&self, reference: &str) -> Result<PlacedImageIdentity, String> {
        let output = self.run(&["image", "inspect", "--format", "{{json .}}", reference]).await?;
        let value: serde_json::Value = serde_json::from_str(&output).map_err(|error| error.to_string())?;
        let local_image_id =
            value["Id"].as_str().filter(|id| is_image_digest(id)).ok_or("Docker inspect has no valid image ID")?.to_string();
        let registry_digest = value["RepoDigests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .find(|digest| *digest == reference)
            .and_then(|reference| reference.rsplit_once('@'))
            .map(|(_, digest)| digest.to_string());
        Ok(PlacedImageIdentity { local_image_id, registry_digest })
    }

    async fn push(&self, cache: &ImageCacheBinding, image_id: &str) -> Result<String, String> {
        // This tag is only a temporary publication handle. Consumers use the
        // manifest digest returned by Docker; no tag enters a frozen identity.
        let tag = format!("{}:flotilla-{}", cache.repository, image_id.trim_start_matches("sha256:"));
        self.run(&["image", "tag", image_id, &tag]).await?;
        let output = self
            .credentials
            .image_registry_operation(&self.host, HostImageAction::ImagePush, &cache.push_credential, &cache.repository, &["push", &tag])
            .await?;
        let digest = output
            .split_once("digest:")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .filter(|word| is_image_digest(word))
            .ok_or("registry push did not report a manifest digest")?;
        Ok(format!("{}@{digest}", cache.repository))
    }

    async fn pull(&self, cache: &ImageCacheBinding, reference: &str) -> Result<(), String> {
        validate_registry_reference(cache, reference)?;
        self.credentials
            .image_registry_operation(&self.host, HostImageAction::ImagePull, &cache.pull_credential, &cache.repository, &[
                "pull", reference,
            ])
            .await?;
        Ok(())
    }
}

impl<I> Drop for ImageDistributor<I> {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().values() {
            job.abort();
        }
        for job in self.publications.get_mut().values() {
            job.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;
    use flotilla_resources::{
        FleetDesignationSpec, FrozenImageLayer, HostSpec, ImageBuildReason, ImageBuildReservation, ImageBuildSpec, ImageBuildStatus,
        ImageInputStability, ImageLayerParent, ImageLayerSpec, ImageLayerStage, InMemoryBackend, InputMeta, ResolvedImageInputs,
    };

    use super::*;

    // Stands in for Docker inventory, inspection, and registry processes.
    // Inventories and inspect results model distinct Docker stores per host.
    struct Docker {
        held: Mutex<BTreeSet<String>>,
        calls: Mutex<Vec<String>>,
        id: String,
        manifest: String,
        corrupt: bool,
        corrupt_manifest: bool,
        fail_push: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl ImageDistributionIo for Docker {
        async fn inventory(&self) -> Result<BTreeSet<String>, String> {
            Ok(self.held.lock().expect("inventory").clone())
        }
        async fn inspect(&self, reference: &str) -> Result<PlacedImageIdentity, String> {
            Ok(PlacedImageIdentity {
                local_image_id: if self.corrupt { format!("sha256:{}", "f".repeat(64)) } else { self.id.clone() },
                registry_digest: reference.contains('@').then(|| {
                    if self.corrupt_manifest {
                        format!("sha256:{}", "f".repeat(64))
                    } else {
                        self.manifest.clone()
                    }
                }),
            })
        }
        async fn push(&self, cache: &ImageCacheBinding, id: &str) -> Result<String, String> {
            assert_eq!(id, self.id);
            self.calls.lock().expect("calls").push("push".into());
            if self.fail_push.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("registry unavailable".into());
            }
            Ok(format!("{}@{}", cache.repository, self.manifest))
        }
        async fn pull(&self, _cache: &ImageCacheBinding, reference: &str) -> Result<(), String> {
            assert!(reference.ends_with(&self.manifest));
            self.calls.lock().expect("calls").push("pull".into());
            self.held.lock().expect("inventory").insert(self.id.clone());
            Ok(())
        }
    }
    fn cache() -> ImageCacheBinding {
        ImageCacheBinding {
            repository: "registry.test/fleet/images".into(),
            pull_credential: "pull".into(),
            push_credential: "push".into(),
        }
    }
    async fn setup(registry: bool, held: bool, corrupt: bool, byte: u8) -> (ImageDistributor<Docker>, ResourceObject<ImageBuild>) {
        let id = format!("sha256:{:02x}", byte).to_string();
        let id = format!("sha256:{}", id.trim_start_matches("sha256:").repeat(32));
        let manifest = format!("sha256:{}", "e".repeat(64));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        for name in ["builder", "destination"] {
            backend
                .using::<Host>("test")
                .create(&InputMeta::builder().name(name.into()).build(), &HostSpec::default())
                .await
                .expect("host");
        }
        if registry {
            backend
                .definitions::<flotilla_resources::Project>("test")
                .apply(
                    &InputMeta::builder().name("fleet".into()).build(),
                    &flotilla_resources::ProjectSpec::builder().display_name("Fleet".into()).build(),
                )
                .await
                .expect("fleet Project");
            backend
                .definitions::<FleetDesignation>("test")
                .apply(&InputMeta::builder().name("fleet".into()).build(), &FleetDesignationSpec {
                    project: "fleet".into(),
                    image_cache: Some(cache()),
                    image_gc: None,
                })
                .await
                .expect("cache");
        }
        let inputs = ResolvedImageInputs::builder()
            .parent_digest(format!("sha256:{}", "1".repeat(64)))
            .content_hashes(vec![format!("sha256:{}", "2".repeat(64))])
            .architecture("amd64".into())
            .stability(ImageInputStability::Pinned)
            .build();
        let spec = ImageBuildSpec::builder()
            .recipe_key(inputs.recipe_key().expect("key"))
            .inputs(inputs)
            .layer(FrozenImageLayer {
                name: "base".into(),
                spec: ImageLayerSpec::builder()
                    .stage(ImageLayerStage::Base)
                    .parent(ImageLayerParent::Image(format!("sha256:{}", "1".repeat(64))))
                    .repository("https://example.test/repo".into())
                    .revision("a".repeat(40))
                    .fragment("Dockerfile".into())
                    .build(),
            })
            .host_ref("builder".into())
            .reservation(ImageBuildReservation { cpu: 1, disk_bytes: 1024 })
            .attempt(0)
            .reason(ImageBuildReason { description: "test".into(), old_inputs: Default::default(), new_inputs: Default::default() })
            .build();
        let builds = backend.using::<ImageBuild>("test");
        let build = builds.create(&InputMeta::builder().name("build".into()).build(), &spec).await.expect("build");
        let status = ImageBuildStatus {
            phase: ImageBuildPhase::Built,
            finished_at: Some(Utc::now()),
            identity: Some(PlacedImageIdentity { local_image_id: id.clone(), registry_digest: None }),
            availability: flotilla_resources::ImageAvailability {
                retired: false,
                retired_at: None,
                hosts: BTreeSet::from(["builder".into()]),
                registry_ref: registry.then(|| format!("{}@{manifest}", cache().repository)),
                failure: None,
            },
            ..Default::default()
        };
        builds.update_status("build", &build.metadata.resource_version, &status).await.expect("built");
        let build = builds.get("build").await.expect("build");
        let io = Arc::new(Docker {
            held: Mutex::new(if held { BTreeSet::from([id.clone()]) } else { BTreeSet::new() }),
            calls: Mutex::new(Vec::new()),
            id,
            manifest,
            corrupt,
            corrupt_manifest: false,
            fail_push: std::sync::atomic::AtomicBool::new(false),
        });
        (ImageDistributor::builder().io(io).backend(backend).namespace("test".into()).host("destination".into()).build(), build)
    }

    #[tokio::test]
    async fn failed_publication_and_delivery_wait_for_retry_cooldown() {
        let (mut delivery, build) = setup(true, true, false, 3).await;
        delivery.host = "builder".into();
        delivery.io.fail_push.store(true, std::sync::atomic::Ordering::SeqCst);
        let builds = delivery.backend.using::<ImageBuild>("test");
        let mut status = build.status.clone().expect("status");
        status.availability.registry_ref = None;
        builds.update_status("build", &build.metadata.resource_version, &status).await.expect("unpublished");
        delivery.refresh().await.expect("queue");
        tokio::task::yield_now().await;
        delivery.refresh().await.expect("failure observed");
        assert_eq!(
            builds.get("build").await.expect("build").status.expect("status").availability.failure.as_deref(),
            Some("registry unavailable")
        );
        for _ in 0..5 {
            delivery.refresh().await.expect("cooldown");
            tokio::task::yield_now().await;
        }
        assert_eq!(delivery.io.calls.lock().expect("calls").as_slice(), ["push"]);
        delivery.retries.lock().await.insert("publication:build".into(), Instant::now());
        delivery.refresh().await.expect("retry after deadline");
        tokio::task::yield_now().await;
        assert_eq!(delivery.io.calls.lock().expect("calls").as_slice(), ["push", "push"]);

        let (delivery, build) = setup(true, false, true, 4).await;
        let delivery = Arc::new(delivery);
        assert!(delivery.request(&build).await.is_err());
        tokio::task::yield_now().await;
        assert!(delivery.request(&build).await.expect_err("substituted ID").contains("digest"));
        assert!(delivery.request(&build).await.expect_err("cooldown").contains("cooldown"));
        assert_eq!(delivery.io.calls.lock().expect("calls").as_slice(), ["pull"]);
    }

    #[tokio::test]
    async fn one_bad_publication_does_not_stop_later_builds() {
        let (mut delivery, build) = setup(true, true, false, 5).await;
        delivery.host = "builder".into();
        let builds = delivery.backend.using::<ImageBuild>("test");
        let later = builds.create(&InputMeta::builder().name("z-later".into()).build(), &build.spec).await.expect("later build");
        let mut status = build.status.clone().expect("status");
        status.availability.hosts.clear();
        builds.update_status("z-later", &later.metadata.resource_version, &status).await.expect("later built");
        status.availability.registry_ref = None;
        builds.update_status("build", &build.metadata.resource_version, &status).await.expect("unpublished");
        let job = tokio::spawn(async { Ok("wrong-repository@invalid".into()) });
        tokio::task::yield_now().await;
        delivery.publications.lock().await.insert("build".into(), job);
        delivery.refresh().await.expect("refresh continues");
        let status = builds.get("z-later").await.expect("later").status.expect("status");
        assert!(status.availability.hosts.contains("builder"));
        assert!(builds.get("build").await.expect("build").status.expect("status").availability.failure.is_some());
    }

    #[hegel::test]
    fn retry_deadline_includes_boundary(tc: hegel::TestCase) {
        let seconds = tc.draw(hegel::generators::integers::<u64>().min_value(0).max_value(600));
        let start = Instant::now();
        assert!(retry_ready(None, start));
        assert_eq!(retry_ready(Some(start + RETRY_COOLDOWN), start + Duration::from_secs(seconds)), seconds >= 300);
    }

    // #2729 split: held or registry-delivered images retain the exact ID.
    // Without either, delivery refuses with a visible waiting reason and
    // does not invoke another transport or substitute an image.
    #[hegel::test]
    fn held_or_registry_digest_delivers_otherwise_waits(tc: hegel::TestCase) {
        let registry = tc.draw(hegel::generators::booleans());
        let held = tc.draw(hegel::generators::booleans());
        let byte = tc.draw(hegel::generators::integers::<u8>().min_value(0).max_value(254));
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let (delivery, build) = setup(registry, held, false, byte).await;
            if !registry && !held {
                let reason = delivery.ensure(&build).await.expect_err("wait without transport");
                assert!(reason.contains("not held") && reason.contains("no fleet registry cache"));
                assert!(delivery.io.calls.lock().expect("calls").is_empty());
                return;
            }
            let first = delivery.ensure(&build).await.expect("deliver");
            assert_eq!(first.local_image_id, build.status.as_ref().expect("status").identity.as_ref().expect("identity").local_image_id);
            let second = delivery.ensure(&build).await.expect("held");
            assert_eq!(second.local_image_id, first.local_image_id);
            let calls = delivery.io.calls.lock().expect("calls");
            assert_eq!(calls.as_slice(), if held { vec![] } else { vec!["pull"] });
        });
    }

    // #2729: a successful transport is not proof of identity. Inspect must
    // reject a substituted local ID on both paths, including a held image.
    #[hegel::test]
    fn corrupt_delivery_never_becomes_ready(tc: hegel::TestCase) {
        let registry = tc.draw(hegel::generators::booleans());
        let held = tc.draw(hegel::generators::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let (delivery, build) = setup(registry, held || !registry, true, 3).await;
            assert!(delivery.ensure(&build).await.expect_err("reject corruption").contains("does not match"));
        });
    }

    // The OCI manifest digest is distinct from the local config ID. A matching
    // config ID cannot excuse a wrong manifest returned by the registry.
    #[tokio::test]
    async fn registry_pull_also_verifies_manifest_identity() {
        let (mut delivery, build) = setup(true, false, false, 3).await;
        Arc::get_mut(&mut delivery.io).expect("unshared adapter").corrupt_manifest = true;
        assert!(delivery.ensure(&build).await.expect_err("wrong manifest").contains("manifest digest"));
    }

    // Availability can change after a completed build, while its identity,
    // verification and execution evidence remain immutable.
    #[tokio::test]
    async fn inventory_refresh_updates_only_availability_and_removes_evicted_hosts() {
        let (mut delivery, build) = setup(true, true, false, 3).await;
        delivery.host = "builder".into();
        let builds = delivery.backend.using::<ImageBuild>("test");
        let mut status = build.status.clone().expect("status");
        status.availability.registry_ref = None;
        builds.update_status("build", &build.metadata.resource_version, &status).await.expect("clear publication");
        delivery.refresh().await.expect("queue publication");
        tokio::task::yield_now().await;
        delivery.refresh().await.expect("publish");
        let published = builds.get("build").await.expect("build");
        assert_eq!(published.status.as_ref().expect("status").identity, build.status.as_ref().expect("status").identity);
        assert_eq!(published.status.as_ref().expect("status").availability.hosts, BTreeSet::from(["builder".into()]));
        assert!(published.status.as_ref().expect("status").availability.registry_ref.as_ref().expect("registry").contains('@'));
        delivery.io.held.lock().expect("inventory").clear();
        delivery.refresh().await.expect("evict");
        let evicted = builds.get("build").await.expect("build");
        assert!(evicted.status.as_ref().expect("status").availability.hosts.is_empty());
        let mut forbidden = evicted.status.clone().expect("status");
        forbidden.identity.as_mut().expect("identity").local_image_id = format!("sha256:{}", "9".repeat(64));
        assert!(builds.update_status("build", &evicted.metadata.resource_version, &forbidden).await.is_err());
    }

    // A declared cache is required to publish a manifest before pull; a missing
    // publication or wrong repository must not silently choose another transport or a tag.
    #[tokio::test]
    async fn declared_cache_refuses_missing_or_wrong_publication() {
        let (delivery, mut build) = setup(true, false, false, 3).await;
        build.status.as_mut().expect("status").availability.registry_ref = None;
        assert!(delivery.ensure(&build).await.is_err());
        build.status.as_mut().expect("status").availability.registry_ref = Some(format!("evil.test/image@sha256:{}", "1".repeat(64)));
        assert!(delivery.ensure(&build).await.is_err());
        assert!(delivery.io.calls.lock().expect("calls").is_empty());
    }
}
#[cfg(test)]
mod gc_process_tests {
    use std::sync::Mutex;

    use flotilla_core::providers::{
        discovery::{test_support::TestEnvVars, EnvironmentBag},
        CommandOutput,
    };
    use flotilla_resources::InMemoryBackend;

    use super::*;

    // Process boundary stand-in: Docker must see a private config, full IDs,
    // no force, a container check, and only explicitly owned tags for removal.
    #[derive(Default)]
    struct Docker {
        blocked: bool,
        external_tag: bool,
        removed: Mutex<BTreeSet<String>>,
        configs: Mutex<Vec<PathBuf>>,
    }
    #[async_trait]
    impl CommandRunner for Docker {
        async fn run(&self, cmd: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "docker");
            assert_eq!(args[0], "--config");
            assert!(Path::new(args[1]).is_dir());
            assert!(!args.contains(&"--force") && !args.contains(&"-f"));
            self.configs.lock().expect("configs").push(args[1].into());
            let id = format!("sha256:{}", "1".repeat(64));
            let args = &args[2..];
            if args[0] == "ps" {
                assert_eq!(args, ["ps", "-a", "--filter", &format!("ancestor={id}"), "--format", "{{.ID}}"]);
                return Ok(if self.blocked { "container".into() } else { String::new() });
            }
            if args[0..2] == ["image", "inspect"] {
                if args[3] == "{{json .RepoTags}}" {
                    assert_eq!(args[4], id);
                    return Ok(serde_json::to_string(&if self.external_tag {
                        vec!["flotilla-build:owned", "external:latest"]
                    } else {
                        vec!["flotilla-build:owned", "flotilla-parent:owned"]
                    })
                    .expect("tags"));
                }
                assert_eq!(args[3], "{{json .}}");
                return Ok(serde_json::json!({"Id": id, "RepoDigests": []}).to_string());
            }
            if args[0..2] == ["image", "rm"] {
                assert!(args[2] == "flotilla-build:owned" || args[2] == "flotilla-parent:owned");
                self.removed.lock().expect("removed").insert(args[2].into());
                return Ok(String::new());
            }
            assert_eq!(args[0..2], ["image", "ls"]);
            assert_eq!(self.removed.lock().expect("removed").len(), 2);
            Ok(String::new())
        }
        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            Ok(CommandOutput { stdout: self.run(cmd, args, cwd, label).await?, stderr: String::new(), exit_code: Some(0) })
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            true
        }
    }

    // #2733: shared image tags require explicit ownership; running/stopped
    // containers and external tags refuse removal before any tag is touched.
    #[tokio::test]
    async fn image_gc_removes_only_owned_tags_without_force() {
        for (blocked, external_tag) in [(false, false), (true, false), (false, true)] {
            let runner = Arc::new(Docker { blocked, external_tag, ..Default::default() });
            let state = tempfile::tempdir().expect("state");
            let credentials = Arc::new(CredentialStore::new(
                ResourceBackend::InMemory(InMemoryBackend::default()),
                "test",
                Arc::new(TestEnvVars::default()),
                EnvironmentBag::new(),
                runner.clone(),
                state.path().into(),
            ));
            let io = DockerImageIo::builder()
                .runner(runner.clone())
                .credentials(credentials)
                .daemon(Weak::new())
                .host("host".into())
                .namespace("test".into())
                .build();
            let result = io
                .remove_local(
                    &format!("sha256:{}", "1".repeat(64)),
                    &BTreeSet::from(["flotilla-build:owned".into(), "flotilla-parent:owned".into()]),
                )
                .await;
            assert_eq!(result.is_err(), blocked || external_tag);
            assert_eq!(runner.removed.lock().expect("removed").len(), if blocked || external_tag { 0 } else { 2 });
            assert!(runner.configs.lock().expect("configs").iter().all(|path| !path.exists()), "configs cleaned");
        }
    }
}
