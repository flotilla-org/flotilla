//! The pinned harness an access runs (#3909, #3956 ruling 2026-10-27): the artifact the
//! adapter's stdlib harness layer pins for the vessel's architecture, delivered into the
//! crew's home by digest. The planner chooses the mechanism from declared facts: a stdlib
//! pin names a public upstream download, so a vessel that reaches public sources fetches
//! it itself, and otherwise the controller fetches it and pushes it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_environment::environment::VesselAgent;
use flotilla_resources::{is_legacy_template, stdlib::stdlib_image_layers, ImageLayerSpec, PinUnpack};
use flotilla_vessel_delivery::{
    ContentDigest, FetchBy, Mechanism, RelativePath, StepFailure, StoreDelivery, StoreEntry, TreeSources, Visibility,
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Where the harness lands in the crew's home.
pub(super) const HARNESS_PATH: &str = ".flotilla/harness";
/// The most a controller downloads for one pinned harness: well above Codex's 427 MB
/// extracted package, so only a wrong or runaway response reaches it.
const DOWNLOAD_LIMIT: u64 = 2 << 30;
/// How long the controller's download may wait to connect, and between bytes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One adapter's pinned harness for one architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PinnedHarness {
    binary: &'static str,
    pub(super) sha256: String,
    url: String,
    unpack: PinUnpack,
}

impl PinnedHarness {
    /// The artifact pinned for `architecture` by the stdlib layer that provides
    /// `harness:<adapter>`, or `None` for an adapter without one.
    pub(super) fn resolve(adapter: &str, architecture: &str) -> Result<Option<Self>, String> {
        Self::resolve_in(stdlib_image_layers()?, adapter, architecture)
    }

    fn resolve_in(layers: &[ImageLayerSpec], adapter: &str, architecture: &str) -> Result<Option<Self>, String> {
        let Some(binary) = flotilla_agent::adapter::harness_binary(adapter) else { return Ok(None) };
        let provided = format!("harness:{adapter}");
        let mut providers = layers.iter().filter(|layer| layer.provides.contains(&provided));
        let layer = match (providers.next(), providers.next()) {
            (None, _) => return Ok(None),
            (Some(layer), None) => layer,
            (Some(first), Some(second)) => {
                return Err(format!("`{provided}` is provided by both `{}` and `{}`", first.name, second.name));
            }
        };
        let mut pins = layer.pins.iter();
        let (Some((name, pin)), None) = (pins.next(), pins.next()) else {
            return Err(format!("harness layer `{}` must pin exactly one artifact", layer.name));
        };
        let artifact =
            pin.artifact(architecture).ok_or_else(|| format!("harness pin `{name}` has no artifact for architecture `{architecture}`"))?;
        // A previous-shape template names no fetchable bytes (ADR 0047 shim).
        if is_legacy_template(&artifact.url) {
            return Err(format!("harness pin `{name}` still has an `{{arch}}` url template"));
        }
        Ok(Some(Self { binary, sha256: artifact.sha256.clone(), url: artifact.url.clone(), unpack: artifact.unpack }))
    }

    /// The pin's digest names public upstream bytes, which the controller can always fetch.
    pub(super) fn sources() -> TreeSources {
        TreeSources { controller: true, digest: Some(Visibility::Public), ..TreeSources::default() }
    }

    /// The executable once the harness has landed at `place`. Both shapes put it at
    /// `bin/<binary>`, as the image layers do.
    pub(super) fn executable(&self, place: &Path) -> PathBuf {
        place.join("bin").join(self.binary)
    }
}

#[async_trait]
impl StoreDelivery for PinnedHarness {
    async fn store(&self, mechanism: Mechanism, _: &Path, agent: &dyn VesselAgent) -> Result<StoreEntry, StepFailure> {
        match mechanism {
            Mechanism::AgentFetch { by: FetchBy::Digest } => {
                agent.fetch_artifact(&self.sha256, std::slice::from_ref(&self.url), self.unpack).await?
            }
            Mechanism::AgentPush => {
                let local = download(&self.url, &self.sha256, DOWNLOAD_LIMIT).await?;
                agent.push_artifact(&self.sha256, &local, self.unpack).await?
            }
            other => return Err(format!("a harness cannot be delivered by {other}").into()),
        }
        // A single file is the executable itself; an archive's entry is the harness tree.
        let link = match self.unpack {
            PinUnpack::File => Some(RelativePath::new(Path::new("bin").join(self.binary)).expect("a relative executable path")),
            PinUnpack::TarGz | PinUnpack::TarXz => None,
        };
        Ok(StoreEntry { sha256: ContentDigest::new(&self.sha256), link })
    }
}

/// Stream `url` to a private temporary file on the controller, kept only when its bytes
/// hash to `sha256`. A response past `limit` bytes, or one that stalls, is abandoned. The
/// file is removed when the returned path is dropped.
async fn download(url: &str, sha256: &str, limit: u64) -> Result<tempfile::TempPath, String> {
    let failed = |error: String| format!("fetch {url} on the controller: {error}");
    let client = rigging_tls::client_builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(STALL_TIMEOUT)
        .build()
        .map_err(|error| failed(error.to_string()))?;
    let mut response =
        client.get(url).send().await.and_then(reqwest::Response::error_for_status).map_err(|error| failed(error.to_string()))?;
    let (file, path) = tempfile::NamedTempFile::new().map_err(|error| failed(error.to_string()))?.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|error| failed(error.to_string()))? {
        length += chunk.len() as u64;
        if length > limit {
            return Err(failed(format!("the response exceeds {limit} bytes")));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| failed(error.to_string()))?;
    }
    file.flush().await.map_err(|error| failed(error.to_string()))?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != sha256 {
        return Err(failed(format!("downloaded bytes hash to {actual}, not the pinned {sha256}")));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3909): each built-in adapter's harness resolves to its stdlib pin's
    // artifact for the vessel's architecture; an architecture without one, or a harness
    // two layers provide, is refused.
    #[test]
    fn adapters_resolve_their_pinned_harness_per_architecture() {
        for adapter in ["claude-code", "codex"] {
            let layers = stdlib_image_layers().expect("stdlib");
            let layer = layers.iter().find(|layer| layer.provides.contains(&format!("harness:{adapter}"))).expect("harness layer");
            let pin = layer.pins.values().next().expect("pin");
            for architecture in ["amd64", "arm64"] {
                let harness = PinnedHarness::resolve(adapter, architecture).expect("resolves").expect("pinned");
                let artifact = pin.artifact(architecture).expect("artifact");
                assert_eq!(
                    (harness.sha256.as_str(), harness.url.as_str(), harness.unpack),
                    (artifact.sha256.as_str(), artifact.url.as_str(), artifact.unpack)
                );
            }
            assert!(PinnedHarness::resolve(adapter, "riscv64").is_err());
        }
        assert_eq!(PinnedHarness::resolve("fleet-canary", "amd64"), Ok(None));
        let mut layers = stdlib_image_layers().expect("stdlib").to_vec();
        let mut twin = layers.iter().find(|layer| layer.provides.contains("harness:codex")).expect("codex layer").clone();
        twin.name = "harness-codex-twin".into();
        layers.push(twin);
        let error = PinnedHarness::resolve_in(&layers, "codex", "amd64").expect_err("two providers");
        assert!(error.contains("provided by both"), "{error}");
    }

    // Behaviour (#3909 push fallback): the controller's copy of a pinned artifact is kept
    // only when its bytes hash to the pin, following a release host's redirect; a response
    // past the limit is abandoned, and an HTTP refusal is reported with its status. The stand-in answers GET only, as a
    // release download does.
    #[tokio::test]
    async fn the_controller_keeps_only_bytes_matching_the_pin() {
        use axum::{http::StatusCode, response::Redirect, routing::get, Router};
        let app = Router::new()
            .route("/download/harness", get(|| async { Redirect::temporary("/objects/harness") }))
            .route("/objects/harness", get(|| async { b"pinned harness".to_vec() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let pinned = format!("{:x}", Sha256::digest(b"pinned harness"));
        let url = format!("{origin}/download/harness");
        let kept = download(&url, &pinned, DOWNLOAD_LIMIT).await.expect("matching bytes are kept");
        assert_eq!(std::fs::read(&kept).expect("kept copy"), b"pinned harness");
        let mismatch = download(&url, &"0".repeat(64), DOWNLOAD_LIMIT).await.expect_err("other bytes");
        assert!(mismatch.contains("not the pinned"), "{mismatch}");
        let oversized = download(&url, &pinned, 4).await.expect_err("past the limit");
        assert!(oversized.contains("exceeds 4 bytes"), "{oversized}");
        let missing = download(&format!("{origin}/download/absent"), &pinned, DOWNLOAD_LIMIT).await.expect_err("absent");
        assert!(missing.contains(&StatusCode::NOT_FOUND.as_u16().to_string()), "{missing}");
    }

    /// A vessel agent whose store records what arrived; storing uses nothing else.
    #[derive(Default)]
    struct Store(std::sync::Mutex<Vec<String>>);

    #[async_trait]
    impl VesselAgent for Store {
        async fn ready(&self) -> Result<(), String> {
            unreachable!("delivery uses only the store")
        }
        async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
            unreachable!("delivery uses only the store")
        }
        async fn push_tools(&self, _: &[flotilla_environment::environment::EnvironmentTool]) -> Result<(), String> {
            unreachable!("delivery uses only the store")
        }
        fn commands(&self) -> std::sync::Arc<dyn flotilla_provider_runtime::CommandRunner> {
            unreachable!("delivery uses only the store")
        }
        async fn writable_base(&self) -> Result<PathBuf, String> {
            unreachable!("delivery uses only the store")
        }
        async fn stat(&self, _: &Path) -> Result<Option<flotilla_environment::environment::EntryKind>, String> {
            unreachable!("delivery uses only the store")
        }
        async fn create_dir(&self, _: &Path, _: u32) -> Result<(), String> {
            unreachable!("delivery uses only the store")
        }
        async fn read(&self, _: &Path, _: u64) -> Result<Option<Vec<u8>>, String> {
            unreachable!("delivery uses only the store")
        }
        async fn write(&self, _: &Path, _: Vec<u8>, _: u32) -> Result<(), String> {
            unreachable!("delivery uses only the store")
        }
        async fn symlink(&self, _: &Path, _: &Path) -> Result<(), String> {
            unreachable!("delivery uses only the store")
        }
        async fn fetch_artifact(&self, sha256: &str, urls: &[String], _: PinUnpack) -> Result<(), String> {
            self.0.lock().expect("store").push(format!("fetched {sha256} from {}", urls.join(" ")));
            Ok(())
        }
        async fn push_artifact(&self, sha256: &str, file: &Path, _: PinUnpack) -> Result<(), String> {
            let bytes = std::fs::read(file).map_err(|error| error.to_string())?;
            self.0.lock().expect("store").push(format!("pushed {sha256} ({} bytes)", bytes.len()));
            Ok(())
        }
    }

    // Behaviour (#3909): the harness lands as the image layers lay it out, its executable
    // at `bin/<binary>` under its place, whether the vessel fetched it or the controller
    // pushed it: a single file's entry is linked there itself, an archive's as the place.
    #[tokio::test]
    async fn the_harness_executable_lands_at_bin_binary_by_fetch_or_push() {
        use axum::{routing::get, Router};
        let app = Router::new().route("/harness", get(|| async { b"pinned harness".to_vec() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/harness", listener.local_addr().expect("address"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let sha256 = format!("{:x}", Sha256::digest(b"pinned harness"));
        let place = Path::new("/home/.flotilla/harness");
        let fetch = Mechanism::AgentFetch { by: FetchBy::Digest };
        for (unpack, mechanism, arrived, link) in [
            (PinUnpack::File, fetch, format!("fetched {sha256} from {url}"), "/home/.flotilla/harness/bin/claude"),
            (PinUnpack::File, Mechanism::AgentPush, format!("pushed {sha256} (14 bytes)"), "/home/.flotilla/harness/bin/claude"),
            (PinUnpack::TarGz, fetch, format!("fetched {sha256} from {url}"), "/home/.flotilla/harness"),
            (PinUnpack::TarGz, Mechanism::AgentPush, format!("pushed {sha256} (14 bytes)"), "/home/.flotilla/harness"),
        ] {
            let harness = PinnedHarness { binary: "claude", sha256: sha256.clone(), url: url.clone(), unpack };
            let store = Store::default();
            let entry = harness.store(mechanism, place, &store).await.expect("stored");
            assert_eq!(*store.0.lock().expect("store"), [arrived], "{unpack:?} by {mechanism}");
            assert_eq!(entry.sha256.as_str(), sha256);
            assert_eq!(entry.link.map_or_else(|| place.to_path_buf(), |entry| entry.under(place)), Path::new(link), "{unpack:?}");
            assert_eq!(harness.executable(place), Path::new("/home/.flotilla/harness/bin/claude"));
        }
        let refused = PinnedHarness { binary: "claude", sha256, url, unpack: PinUnpack::File };
        let error = refused.store(Mechanism::AgentFetch { by: FetchBy::Url }, place, &Store::default()).await.unwrap_err();
        assert!(error.error.contains("cannot be delivered by agent-fetch(url)"), "{error:?}");
    }
}
