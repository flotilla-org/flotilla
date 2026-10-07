//! Digest availability and delivery. Build evidence is immutable; inventory and
//! registry publication are observations which may change independently.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_core::{
    in_process::InProcessDaemon,
    providers::{ChannelLabel, CommandRunner},
};
use flotilla_resources::{
    is_image_digest, FleetDesignation, Host, HostImageAction, ImageBuild, ImageBuildPhase, ImageCacheBinding, PlacedImageIdentity,
    ResourceBackend, ResourceError, ResourceObject, FLEET_DESIGNATION_NAME, IMAGE_DIGESTS_CAPABILITY,
};
use tokio::io::AsyncWriteExt;

use crate::credential::CredentialStore;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The process/mesh seam. All methods operate on exact IDs, never mutable tags.
/// Returning from pull/transfer does not attest identity: the caller inspects it.
#[async_trait]
pub(crate) trait ImageDistributionIo: Send + Sync {
    async fn inventory(&self) -> Result<BTreeSet<String>, String>;
    async fn inspect(&self, reference: &str) -> Result<PlacedImageIdentity, String>;
    async fn push(&self, cache: &ImageCacheBinding, image_id: &str) -> Result<String, String>;
    async fn pull(&self, cache: &ImageCacheBinding, reference: &str) -> Result<(), String>;
    async fn transfer(&self, source: &str, image_id: &str) -> Result<(), String>;
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
    publications: tokio::sync::Mutex<BTreeMap<String, tokio::task::JoinHandle<Result<String, String>>>>,
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
            return jobs.remove(&name).expect("finished delivery").await.map_err(|error| error.to_string())?.map(Some);
        }
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

    /// Prefer the held exact ID, then a published registry digest, then mesh.
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
            let source = if status.availability.hosts.contains(&build.spec.host_ref) {
                &build.spec.host_ref
            } else {
                status.availability.hosts.iter().find(|host| *host != &self.host).unwrap_or(&build.spec.host_ref)
            };
            self.io.transfer(source, &expected.local_image_id).await?;
            self.verify(&expected.local_image_id, expected, None).await
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
    /// locations from their latest inventories rather than trusting old transfer
    /// successes forever.
    pub async fn refresh(&self) -> Result<(), String> {
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
            let Some(mut status) = build.status.clone().filter(|status| status.phase == ImageBuildPhase::Built) else {
                continue;
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
                                .map_err(|error| error.to_string())?;
                            match result {
                                Ok(reference) => {
                                    validate_registry_reference(cache, &reference)?;
                                    status.availability.registry_ref = Some(reference);
                                    status.availability.failure = None;
                                }
                                Err(reason) => status.availability.failure = Some(reason.chars().take(2048).collect()),
                            }
                        } else if !publications.contains_key(&build.metadata.name) {
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
        }
        Ok(())
    }
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
    pub daemon: Weak<InProcessDaemon>,
    pub host: String,
    pub namespace: String,
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
        Ok(output.split_whitespace().filter(|line| is_image_digest(line)).map(String::from).collect())
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

    async fn transfer(&self, source: &str, image_id: &str) -> Result<(), String> {
        if !is_image_digest(image_id) {
            return Err("mesh transfer requires an exact local image ID".into());
        }
        let daemon = self.daemon.upgrade().ok_or("image distribution daemon stopped")?;
        let hosts =
            daemon.resource_backend().including_replicas::<Host>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let peer = hosts
            .items
            .iter()
            .find(|item| item.object.metadata.name == source)
            .and_then(|item| item.object.status.as_ref())
            .and_then(|status| status.description.as_ref())
            .map(|description| description.node.node_id.as_str())
            .unwrap_or(source);
        let visited = vec![daemon.local_host_identity().node.node_id.to_string()];
        let mut response = open_mesh_archive(&daemon, peer, image_id, &visited).await?;
        let spool = tempfile::tempdir().map_err(|error| error.to_string())?;
        let archive = spool.path().join("image.tar");
        save_response(&mut response, &archive).await?;
        let config = spool.path().join("config");
        tokio::fs::create_dir(&config).await.map_err(|error| error.to_string())?;
        let directory = config.to_string_lossy();
        tokio::time::timeout(
            OPERATION_TIMEOUT,
            self.runner.run_from_file("docker", &["--config", &directory, "load"], Path::new("/"), &archive),
        )
        .await
        .map_err(|_| "Docker load timed out".to_string())??;
        Ok(())
    }
}

fn archive_url(base: &str, image: &str, source: &str, visited: &[String]) -> Result<url::Url, String> {
    let mut url = url::Url::parse(base).map_err(|error| error.to_string())?;
    url.set_path(&format!("/image-transfer/{image}"));
    url.query_pairs_mut().append_pair("source", source).append_pair("visited", &visited.join(","));
    Ok(url)
}

fn image_routes(routes: Vec<(String, PathBuf)>, source: &str, visited: &[String]) -> Result<Vec<(String, PathBuf)>, String> {
    if visited.len() > 8 {
        return Err("image mesh route exceeds eight hops".into());
    }
    let mut routes = routes.into_iter().filter(|(peer, _)| !visited.contains(peer)).collect::<Vec<_>>();
    routes.sort_by_key(|(peer, _)| peer != source);
    Ok(routes)
}

async fn open_mesh_archive(daemon: &InProcessDaemon, source: &str, image: &str, visited: &[String]) -> Result<reqwest::Response, String> {
    let routes = image_routes(daemon.image_peer_routes().await, source, visited)?;
    let url = archive_url("http://localhost", image, source, visited)?;
    for (_, path) in routes {
        let client = reqwest::Client::builder().unix_socket(path).timeout(OPERATION_TIMEOUT).build().map_err(|error| error.to_string())?;
        match client.get(url.clone()).send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(_) | Err(_) => continue,
        }
    }
    Err(format!("no resource mesh route can export the digest from {source}"))
}

/// Resource transport forwarding supports sparse meshes without growing
/// Plane-A routing. Visited nodes exclude cycles and the hop count is bounded.
/// Intermediaries copy chunks directly; only Docker's endpoints spool archives.
pub(crate) async fn relay_image<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    daemon: &InProcessDaemon,
    source: &str,
    image: &str,
    mut visited: Vec<String>,
) -> Result<(), String> {
    let local = daemon.local_host_identity().node.node_id.to_string();
    if visited.contains(&local) {
        return Err("cyclic image mesh route".into());
    }
    visited.push(local);
    let mut response = open_mesh_archive(daemon, source, image, &visited).await?;
    let length = response.content_length().map(|length| format!("Content-Length: {length}\r\n")).unwrap_or_default();
    stream
        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\n{length}Connection: close\r\n\r\n").as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        stream.write_all(&chunk).await.map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
async fn download_archive(client: &reqwest::Client, url: &str, archive: &Path) -> Result<(), String> {
    let mut response =
        client.get(url).send().await.map_err(|error| error.to_string())?.error_for_status().map_err(|error| error.to_string())?;
    save_response(&mut response, archive).await
}

async fn save_response(response: &mut reqwest::Response, archive: &Path) -> Result<(), String> {
    let mut file = tokio::fs::File::create(archive).await.map_err(|error| error.to_string())?;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        file.write_all(&chunk).await.map_err(|error| error.to_string())?;
    }
    file.flush().await.map_err(|error| error.to_string())
}

/// Serve a Docker save archive on the trusted resource mesh. Paths contain only
/// validated IDs, and all archive/config files disappear on cancellation.
pub(crate) async fn serve_image<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    runner: &dyn CommandRunner,
    image_id: &str,
) -> Result<(), String> {
    if !is_image_digest(image_id) {
        return Err("invalid image transfer digest".into());
    }
    let spool = tempfile::tempdir().map_err(|error| error.to_string())?;
    let archive: PathBuf = spool.path().join("image.tar");
    let config = spool.path().join("config");
    tokio::fs::create_dir(&config).await.map_err(|error| error.to_string())?;
    let config_arg = config.to_string_lossy();
    tokio::time::timeout(
        OPERATION_TIMEOUT,
        runner.run_to_file("docker", &["--config", &config_arg, "save", image_id], Path::new("/"), &archive),
    )
    .await
    .map_err(|_| "Docker save timed out".to_string())??;
    let mut file = tokio::fs::File::open(archive).await.map_err(|error| error.to_string())?;
    let size = file.metadata().await.map_err(|error| error.to_string())?.len();
    stream
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .map_err(|error| error.to_string())?;
    tokio::io::copy(&mut file, stream).await.map_err(|error| error.to_string())?;
    Ok(())
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

    // Stands in for the Docker process and authenticated mesh byte transport.
    // Inventories and inspect results model distinct Docker stores per host.
    struct Docker {
        held: Mutex<BTreeSet<String>>,
        calls: Mutex<Vec<String>>,
        id: String,
        manifest: String,
        corrupt: bool,
        corrupt_manifest: bool,
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
            Ok(format!("{}@{}", cache.repository, self.manifest))
        }
        async fn pull(&self, _cache: &ImageCacheBinding, reference: &str) -> Result<(), String> {
            assert!(reference.ends_with(&self.manifest));
            self.calls.lock().expect("calls").push("pull".into());
            self.held.lock().expect("inventory").insert(self.id.clone());
            Ok(())
        }
        async fn transfer(&self, source: &str, id: &str) -> Result<(), String> {
            assert_eq!(source, "builder");
            assert_eq!(id, self.id);
            self.calls.lock().expect("calls").push("transfer".into());
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
        });
        (ImageDistributor::builder().io(io).backend(backend).namespace("test".into()).host("destination".into()).build(), build)
    }

    // #2729: registry and registry-less paths deliver the same exact local
    // digest. Generate digest bytes, held/missing states and both transports;
    // each subsequent request must prefer the newly held digest.
    #[hegel::test]
    fn both_routes_deliver_the_immutable_digest(tc: hegel::TestCase) {
        let registry = tc.draw(hegel::generators::booleans());
        let held = tc.draw(hegel::generators::booleans());
        let byte = tc.draw(hegel::generators::integers::<u8>().min_value(0).max_value(254));
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let (delivery, build) = setup(registry, held, false, byte).await;
            let first = delivery.ensure(&build).await.expect("deliver");
            assert_eq!(first.local_image_id, build.status.as_ref().expect("status").identity.as_ref().expect("identity").local_image_id);
            let second = delivery.ensure(&build).await.expect("held");
            assert_eq!(second.local_image_id, first.local_image_id);
            let calls = delivery.io.calls.lock().expect("calls");
            assert_eq!(
                calls.as_slice(),
                if held {
                    vec![]
                } else if registry {
                    vec!["pull"]
                } else {
                    vec!["transfer"]
                }
            );
        });
    }

    // #2729: a successful transport is not proof of identity. Inspect must
    // reject a substituted local ID on both paths, including a held image.
    #[hegel::test]
    fn corrupt_delivery_never_becomes_ready(tc: hegel::TestCase) {
        let registry = tc.draw(hegel::generators::booleans());
        let held = tc.draw(hegel::generators::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let (delivery, build) = setup(registry, held, true, 3).await;
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
    // publication or wrong repository must not silently choose mesh or a tag.
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
mod stream_tests {
    use std::sync::Mutex;

    use flotilla_core::providers::CommandOutput;
    use tokio::io::AsyncReadExt;

    use super::*;

    // Stands in for Docker save's binary process stdout, including zero and
    // non-UTF8 bytes. The resource response uses real bounded Tokio streams.
    struct Save {
        config: Mutex<Option<PathBuf>>,
        bytes: Vec<u8>,
    }
    #[async_trait]
    impl CommandRunner for Save {
        async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
            Err("unexpected text command".into())
        }
        async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
            Err("unexpected text command".into())
        }
        async fn exists(&self, _: &str, _: &[&str]) -> bool {
            false
        }
        async fn run_to_file(&self, cmd: &str, args: &[&str], _: &Path, destination: &Path) -> Result<(), String> {
            assert_eq!(cmd, "docker");
            assert_eq!(args[0], "--config");
            assert_eq!(args[2], "save");
            assert!(is_image_digest(args[3]));
            assert!(Path::new(args[1]).is_dir());
            *self.config.lock().expect("config") = Some(args[1].into());
            tokio::fs::write(destination, &self.bytes).await.map_err(|error| error.to_string())
        }
    }

    // #2729: the save archive crosses the resource transport without text
    // conversion or loss, with accurate HTTP framing and operation cleanup.
    // Generate lengths across the 1KiB buffer boundary, including empty output.
    #[hegel::test]
    fn mesh_archive_preserves_binary_bytes_and_cleans_config(tc: hegel::TestCase) {
        let length = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(4096));
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let bytes = (0..length).map(|index| (index % 256) as u8).collect::<Vec<_>>();
            let save = Arc::new(Save { config: Mutex::new(None), bytes: bytes.clone() });
            let (mut reader, mut writer) = tokio::io::duplex(1024);
            let source = save.clone();
            let sending =
                tokio::spawn(async move { serve_image(&mut writer, source.as_ref(), &format!("sha256:{}", "3".repeat(64))).await });
            let mut received = Vec::new();
            reader.read_to_end(&mut received).await.expect("response");
            sending.await.expect("task").expect("save");
            let split = received.windows(4).position(|bytes| bytes == b"\r\n\r\n").expect("headers") + 4;
            assert_eq!(&received[split..], bytes.as_slice());
            assert!(std::str::from_utf8(&received[..split]).expect("headers").contains(&format!("Content-Length: {length}\r\n")));
            assert!(!save.config.lock().expect("config").as_ref().expect("path").exists());
        });
    }

    // A tag or malformed digest is rejected before invoking Docker save.
    #[tokio::test]
    async fn mesh_export_refuses_tags_without_a_process() {
        let save = Save { config: Mutex::new(None), bytes: vec![] };
        let mut sink = tokio::io::sink();
        for reference in ["image:latest", "sha256:123", "sha256:../../path", ""] {
            assert!(serve_image(&mut sink, &save, reference).await.is_err());
        }
        assert!(save.config.lock().expect("config").is_none());
    }
}

#[cfg(test)]
mod http_contract {
    use axum::{
        extract::Path as RequestPath,
        http::{Method, StatusCode, Uri},
        routing::get,
        Router,
    };

    use super::*;

    // #2729 HTTP glue: one GET endpoint is sufficient. This in-process stand-in
    // enforces the digest path/method and supplies binary bytes, not JSON/text.
    #[tokio::test]
    async fn mesh_download_obeys_http_contract_and_refuses_non_success() {
        let expected = format!("sha256:{}", "3".repeat(64));
        let bytes = (0..65536).map(|index| (index % 256) as u8).collect::<Vec<_>>();
        let reply = bytes.clone();
        let accepted = expected.clone();
        let app = Router::new().route(
            "/image-transfer/{digest}",
            get(move |RequestPath(digest): RequestPath<String>, uri: Uri, method: Method| {
                let accepted = accepted.clone();
                let reply = reply.clone();
                async move {
                    assert_eq!(method, Method::GET);
                    let query =
                        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned().collect::<BTreeMap<_, _>>();
                    assert_eq!(query.get("source").map(String::as_str), Some("builder"));
                    assert_eq!(query.get("visited").map(String::as_str), Some("consumer,relay"));
                    if digest != accepted {
                        return (StatusCode::NOT_FOUND, Vec::new());
                    }
                    (StatusCode::OK, reply)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("HTTP listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let directory = tempfile::tempdir().expect("archive");
        let archive = directory.path().join("image.tar");
        let client = reqwest::Client::new();
        let url = archive_url(&format!("http://{address}"), &expected, "builder", &["consumer".into(), "relay".into()]).expect("URL");
        download_archive(&client, url.as_str(), &archive).await.expect("download");
        assert_eq!(tokio::fs::read(&archive).await.expect("bytes"), bytes);
        let missing = directory.path().join("missing.tar");
        let url = archive_url(&format!("http://{address}"), &format!("sha256:{}", "4".repeat(64)), "builder", &[
            "consumer".into(),
            "relay".into(),
        ])
        .expect("URL");
        assert!(download_archive(&client, url.as_str(), &missing).await.is_err());
        assert!(!missing.exists(), "HTTP refusal must not create an archive to load");
        server.abort();
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    // Sparse resource meshes may relay through an intermediate host. Generate
    // visited paths across the eight-hop boundary, both direct and indirect
    // targets, and visited direct/relay neighbors. No node is revisited.
    #[hegel::test]
    fn resource_image_routes_prefer_direct_and_exclude_cycles(tc: hegel::TestCase) {
        let length = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(9));
        let direct = tc.draw(hegel::generators::booleans());
        let seen_target = tc.draw(hegel::generators::booleans());
        let seen_relay = tc.draw(hegel::generators::booleans());
        let mut visited = (0..length).map(|index| format!("node-{index}")).collect::<Vec<_>>();
        if seen_target {
            visited.push("builder".into());
        }
        if seen_relay {
            visited.push("relay".into());
        }
        let source = if direct { "builder" } else { "other" };
        let routes = vec![("relay".into(), PathBuf::from("/relay")), ("builder".into(), PathBuf::from("/builder"))];
        let result = image_routes(routes, source, &visited);
        if visited.len() > 8 {
            assert!(result.is_err());
            return;
        }
        let routes = result.expect("bounded route");
        assert!(routes.iter().all(|(peer, _)| !visited.contains(peer)));
        assert_eq!(routes.len(), usize::from(!seen_target) + usize::from(!seen_relay));
        if direct && !seen_target {
            assert_eq!(routes.first().expect("direct").0, "builder");
        }
    }
}
