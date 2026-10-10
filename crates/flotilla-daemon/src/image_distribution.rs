//! Digest availability and delivery. Build evidence is immutable; inventory and
//! registry publication are observations which may change independently.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use flotilla_credentials::CredentialStore;
use flotilla_resources::{
    is_image_digest, FleetDesignation, Host, HostImageAction, ImageBuild, ImageBuildPhase, ImageCacheBinding, LocalImageCacheKey,
    LocalImageInventories, PlacedImageIdentity, ResourceBackend, ResourceError, ResourceObject, FLEET_DESIGNATION_NAME,
    IMAGE_DIGESTS_CAPABILITY,
};

const RETRY_COOLDOWN: Duration = Duration::from_secs(300);

/// Distribution orchestration over exact IDs and pinned registry references.
/// Returning from pull does not attest identity: the caller inspects it.
#[async_trait]
pub(crate) trait ImageDistributionIo: Send + Sync {
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
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
    pub provider_instance: String,
    pub registered_instances: BTreeSet<String>,
    #[builder(default)]
    pub jobs: tokio::sync::Mutex<BTreeMap<String, tokio::task::JoinHandle<Result<PlacedImageIdentity, String>>>>,
    #[builder(default)]
    publications: tokio::sync::Mutex<BTreeMap<String, tokio::task::JoinHandle<Result<String, String>>>>,
    #[builder(default)]
    retries: tokio::sync::Mutex<BTreeMap<String, Instant>>,
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
        let status = build.status.as_ref().filter(|status| status.phase == ImageBuildPhase::Built).ok_or("image build is not built")?;
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
        let held = self.io.inventory().await?;
        let hosts = self.backend.using::<Host>(&self.namespace);
        let host = hosts.get(&self.host).await.map_err(|error| error.to_string())?;
        let mut status = host.status.clone().unwrap_or_default();
        let mut inventory = decode_inventory(status.capabilities.get(IMAGE_DIGESTS_CAPABILITY), &self.host);
        inventory.0.retain(|instance, _| self.registered_instances.contains(instance));
        inventory.0.insert(self.provider_instance.clone(), held.clone());
        status.capabilities.insert(IMAGE_DIGESTS_CAPABILITY.into(), serde_json::to_value(&inventory).map_err(|error| error.to_string())?);
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
                let Some(mut status) = build.status.clone().filter(|status| status.phase == ImageBuildPhase::Built) else {
                    return Ok(());
                };
                let identity = status.identity.as_ref().ok_or("built image has no identity")?;
                status.availability.caches = inventories
                    .items
                    .iter()
                    .flat_map(|source| {
                        let object = &source.object;
                        let inventory = object
                            .status
                            .as_ref()
                            .and_then(|status| status.capabilities.get(IMAGE_DIGESTS_CAPABILITY))
                            .and_then(|value| serde_json::from_value::<LocalImageInventories>(value.clone()).ok())
                            .unwrap_or_default();
                        inventory
                            .0
                            .into_iter()
                            .filter(|(_, digests)| digests.contains(&identity.local_image_id))
                            .map(|(provider_instance, _)| LocalImageCacheKey { host: object.metadata.name.clone(), provider_instance })
                            .collect::<Vec<_>>()
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
                                publications.insert(build.metadata.name.clone(), tokio::spawn(async move { io.push(&cache, &id).await }));
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

fn decode_inventory(value: Option<&serde_json::Value>, host: &str) -> LocalImageInventories {
    match value.map(|value| serde_json::from_value(value.clone())).transpose() {
        Ok(inventory) => inventory.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(%host, %error, "invalid image inventory; replacing it with fresh cache observations");
            LocalImageInventories::default()
        }
    }
}

/// Registration is authoritative for cache identities. Run even on hosts with
/// no image provider, where no distributor exists to refresh old observations.
pub(crate) async fn prune_unregistered_caches(
    backend: &ResourceBackend,
    namespace: &str,
    host: &str,
    registered: &BTreeSet<String>,
) -> Result<(), String> {
    let hosts = backend.using::<Host>(namespace);
    let object = hosts.get(host).await.map_err(|error| error.to_string())?;
    if let Some(mut status) = object.status.clone() {
        if status.capabilities.contains_key(IMAGE_DIGESTS_CAPABILITY) {
            let mut inventory = decode_inventory(status.capabilities.get(IMAGE_DIGESTS_CAPABILITY), host);
            inventory.0.retain(|instance, _| registered.contains(instance));
            status
                .capabilities
                .insert(IMAGE_DIGESTS_CAPABILITY.into(), serde_json::to_value(inventory).map_err(|error| error.to_string())?);
            if object.status.as_ref() != Some(&status) {
                hosts.update_status(host, &object.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
            }
        }
    }
    let builds = backend.using::<ImageBuild>(namespace);
    for object in builds.list().await.map_err(|error| error.to_string())?.items {
        if object.spec.host_ref != host {
            continue;
        }
        if let Some(mut status) = object.status.clone() {
            status.availability.caches.retain(|cache| cache.host != host || registered.contains(&cache.provider_instance));
            if object.status.as_ref() != Some(&status) {
                builds
                    .update_status(&object.metadata.name, &object.metadata.resource_version, &status)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    Ok(())
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
pub(crate) struct ProviderImageIo {
    pub provider: Arc<dyn flotilla_core::providers::environment::EnvironmentProvider>,
    pub publisher: Option<Arc<dyn flotilla_core::providers::environment::ImageBuilder>>,
    pub credentials: Arc<CredentialStore>,
    pub host: String,
}

#[async_trait]
impl ImageDistributionIo for ProviderImageIo {
    async fn inventory(&self) -> Result<BTreeSet<String>, String> {
        self.cache()?.inventory().await
    }
    async fn inspect(&self, reference: &str) -> Result<PlacedImageIdentity, String> {
        self.cache()?.inspect(reference).await?.ok_or_else(|| "image digest is absent from provider cache".into())
    }
    async fn push(&self, cache: &ImageCacheBinding, image_id: &str) -> Result<String, String> {
        let publisher = self.publisher.as_ref().ok_or("image publication capability unavailable")?;
        let auth =
            self.credentials.registry_auth(&self.host, HostImageAction::ImagePush, &cache.push_credential, &cache.repository).await?;
        publisher.publish(image_id, &cache.repository, Some(&auth)).await
    }
    async fn pull(&self, cache: &ImageCacheBinding, reference: &str) -> Result<(), String> {
        validate_registry_reference(cache, reference)?;
        let auth =
            self.credentials.registry_auth(&self.host, HostImageAction::ImagePull, &cache.pull_credential, &cache.repository).await?;
        self.cache()?.pull(reference, Some(&auth)).await
    }
}
impl ProviderImageIo {
    fn cache(&self) -> Result<&dyn flotilla_core::providers::environment::LocalImageCache, String> {
        self.provider.local_image_cache().ok_or_else(|| "provider has no local image cache".into())
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
                .apply(
                    &InputMeta::builder().name("fleet".into()).build(),
                    &FleetDesignationSpec { project: "fleet".into(), image_cache: Some(cache()) },
                )
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
                caches: BTreeSet::from([LocalImageCacheKey { host: "builder".into(), provider_instance: "cache-a".into() }]),
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
        (
            ImageDistributor::builder()
                .io(io)
                .backend(backend)
                .namespace("test".into())
                .host("destination".into())
                .provider_instance("cache-a".into())
                .registered_instances(BTreeSet::from(["cache-a".into(), "cache-b".into()]))
                .build(),
            build,
        )
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
        status.availability.caches.clear();
        builds.update_status("z-later", &later.metadata.resource_version, &status).await.expect("later built");
        status.availability.registry_ref = None;
        builds.update_status("build", &build.metadata.resource_version, &status).await.expect("unpublished");
        let job = tokio::spawn(async { Ok("wrong-repository@invalid".into()) });
        tokio::task::yield_now().await;
        delivery.publications.lock().await.insert("build".into(), job);
        delivery.refresh().await.expect("refresh continues");
        let status = builds.get("z-later").await.expect("later").status.expect("status");
        assert!(status.availability.caches.contains(&LocalImageCacheKey { host: "builder".into(), provider_instance: "cache-a".into() }));
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
        assert_eq!(
            published.status.as_ref().expect("status").availability.caches,
            BTreeSet::from([LocalImageCacheKey { host: "builder".into(), provider_instance: "cache-a".into() }])
        );
        assert!(published.status.as_ref().expect("status").availability.registry_ref.as_ref().expect("registry").contains('@'));
        delivery.io.held.lock().expect("inventory").clear();
        delivery.refresh().await.expect("evict");
        let evicted = builds.get("build").await.expect("build");
        assert!(evicted.status.as_ref().expect("status").availability.caches.is_empty());
        let mut forbidden = evicted.status.clone().expect("status");
        forbidden.identity.as_mut().expect("identity").local_image_id = format!("sha256:{}", "9".repeat(64));
        assert!(builds.update_status("build", &evicted.metadata.resource_version, &forbidden).await.is_err());
    }

    // #2972: refreshing one cache preserves another instance's inventory on
    // the same host; evicting from one must not erase the other's availability.
    #[tokio::test]
    async fn same_host_cache_refresh_preserves_other_provider_inventory() {
        let (mut delivery, build) = setup(false, true, false, 7).await;
        delivery.host = "builder".into();
        delivery.refresh().await.expect("cache a refresh");
        delivery.provider_instance = "cache-b".into();
        delivery.io.held.lock().expect("cache b inventory").clear();
        delivery.refresh().await.expect("empty cache b refresh");
        let hosts = delivery.backend.using::<Host>("test");
        let host = hosts.get("builder").await.expect("host");
        let inventory: LocalImageInventories =
            serde_json::from_value(host.status.expect("status").capabilities[IMAGE_DIGESTS_CAPABILITY].clone()).expect("inventory");
        let digest = &build.status.as_ref().expect("status").identity.as_ref().expect("identity").local_image_id;
        assert!(inventory.held(Some("cache-a")).expect("cache a").contains(digest));
        assert!(!inventory.held(Some("cache-b")).expect("cache b").contains(digest));
        let refreshed = delivery.backend.using::<ImageBuild>("test").get("build").await.expect("build");
        assert_eq!(
            refreshed.status.expect("status").availability.caches,
            BTreeSet::from([LocalImageCacheKey { host: "builder".into(), provider_instance: "cache-a".into() }])
        );
    }

    // #2972: renamed and removed instances cannot retain inventory or mutable
    // build availability, including a host with no cache/distributor at startup.
    #[tokio::test]
    async fn retired_instances_are_pruned_on_refresh_and_cacheless_startup() {
        let (mut delivery, _) = setup(false, true, false, 8).await;
        delivery.host = "builder".into();
        delivery.refresh().await.expect("observe old instance");
        delivery.registered_instances = BTreeSet::from(["cache-b".into()]);
        delivery.provider_instance = "cache-b".into();
        delivery.io.held.lock().expect("new empty cache").clear();
        delivery.refresh().await.expect("renamed instance refresh");
        let hosts = delivery.backend.using::<Host>("test");
        let inventory = hosts.get("builder").await.expect("host").status.expect("status").capabilities[IMAGE_DIGESTS_CAPABILITY].clone();
        assert_eq!(inventory, serde_json::json!({"cache-b": []}));
        assert!(delivery
            .backend
            .using::<ImageBuild>("test")
            .get("build")
            .await
            .expect("build")
            .status
            .expect("status")
            .availability
            .caches
            .is_empty());
        delivery.io.held.lock().expect("new cache image").insert(delivery.io.id.clone());
        delivery.refresh().await.expect("observe new cache");
        prune_unregistered_caches(&delivery.backend, "test", "builder", &BTreeSet::new()).await.expect("cacheless startup");
        let inventory = hosts.get("builder").await.expect("host").status.expect("status").capabilities[IMAGE_DIGESTS_CAPABILITY].clone();
        assert_eq!(inventory, serde_json::json!({}));
        assert!(delivery
            .backend
            .using::<ImageBuild>("test")
            .get("build")
            .await
            .expect("build")
            .status
            .expect("status")
            .availability
            .caches
            .is_empty());
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
