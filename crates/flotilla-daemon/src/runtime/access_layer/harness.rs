//! The pinned harness an access runs (#3909, #3956 ruling 2026-10-27): the artifact the
//! adapter's stdlib harness layer pins for the vessel's architecture, delivered into the
//! crew's home by digest. The planner chooses the mechanism from declared facts: a stdlib
//! pin names a public upstream download, so a vessel that reaches public sources fetches
//! it itself, and otherwise the controller fetches it and pushes it.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use flotilla_environment::environment::VesselAgent;
use flotilla_resources::{is_legacy_template, stdlib::stdlib_image_layers, PinUnpack};
use flotilla_vessel_delivery::{FetchBy, Mechanism, TreeDelivery, TreeSources, Visibility};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Where the harness lands in the crew's home.
pub(super) const HARNESS_PATH: &str = ".flotilla/harness";

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
        let Some(binary) = flotilla_agent::adapter::harness_binary(adapter) else { return Ok(None) };
        let provided = format!("harness:{adapter}");
        let layers = stdlib_image_layers()?;
        let Some(layer) = layers.iter().find(|layer| layer.provides.contains(&provided)) else { return Ok(None) };
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
impl TreeDelivery for PinnedHarness {
    async fn deliver(&self, mechanism: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        match mechanism {
            Mechanism::AgentFetch { by: FetchBy::Digest } => {
                agent.fetch_artifact(&self.sha256, std::slice::from_ref(&self.url), self.unpack).await?
            }
            Mechanism::AgentPush => {
                let local = download(&self.url, &self.sha256).await?;
                agent.push_artifact(&self.sha256, &local, self.unpack).await?
            }
            other => return Err(format!("a harness cannot be delivered by {other}")),
        }
        // A single file is the executable itself; an archive's entry is the harness tree.
        match self.unpack {
            PinUnpack::File => agent.link_artifact(&self.executable(target), &self.sha256, Path::new("")).await,
            PinUnpack::TarGz | PinUnpack::TarXz => agent.link_artifact(target, &self.sha256, Path::new("")).await,
        }
    }
}

/// Stream `url` to a private temporary file on the controller, kept only when its bytes
/// hash to `sha256`. The file is removed when the returned path is dropped.
async fn download(url: &str, sha256: &str) -> Result<tempfile::TempPath, String> {
    let failed = |error: String| format!("fetch {url} on the controller: {error}");
    let client = rigging_tls::client_builder().build().map_err(|error| failed(error.to_string()))?;
    let mut response =
        client.get(url).send().await.and_then(reqwest::Response::error_for_status).map_err(|error| failed(error.to_string()))?;
    let (file, path) = tempfile::NamedTempFile::new().map_err(|error| failed(error.to_string()))?.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut hasher = Sha256::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| failed(error.to_string()))? {
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
    // artifact for the vessel's architecture, and an architecture without one is refused.
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
    }

    // Behaviour (#3909 push fallback): the controller's copy of a pinned artifact is kept
    // only when its bytes hash to the pin, following a release host's redirect, and an
    // HTTP refusal is reported with its status. The stand-in answers GET only, as a
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
        let kept = download(&format!("{origin}/download/harness"), &pinned).await.expect("matching bytes are kept");
        assert_eq!(std::fs::read(&kept).expect("kept copy"), b"pinned harness");
        let mismatch = download(&format!("{origin}/download/harness"), &"0".repeat(64)).await.expect_err("other bytes");
        assert!(mismatch.contains("not the pinned"), "{mismatch}");
        let missing = download(&format!("{origin}/download/absent"), &pinned).await.expect_err("absent");
        assert!(missing.contains(&StatusCode::NOT_FOUND.as_u16().to_string()), "{missing}");
    }
}
