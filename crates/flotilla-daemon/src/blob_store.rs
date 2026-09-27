//! Content-addressed blob storage. Fleet stores are caches of the durable local
//! write path; no caller needs fleet credentials or network availability.
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::{
    config::BlobStoreConfig,
    providers::{ChannelLabel, HttpClient, ReqwestHttpClient},
};
use flotilla_protocol::BlobSyncStatus;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobDigest(String);

impl BlobDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
            return Err("blob digest must be 64 lowercase SHA-256 hex digits".into());
        }
        Ok(Self(value.to_string()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        if Self::of(bytes) == *self {
            Ok(())
        } else {
            Err(format!("blob digest mismatch for {}", self.0))
        }
    }
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String>;
    async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String>;
    async fn has(&self, digest: &BlobDigest) -> Result<bool, String>;
    async fn delete(&self, digest: &BlobDigest) -> Result<(), String>;

    async fn put_file(&self, path: &Path) -> Result<(BlobDigest, u64), String> {
        let bytes = tokio::fs::read(path).await.map_err(|error| error.to_string())?;
        let digest = self.put(&bytes).await?;
        Ok((digest, bytes.len() as u64))
    }

    async fn get_file(&self, digest: &BlobDigest, path: &Path) -> Result<Option<u64>, String> {
        let Some(bytes) = self.get(digest).await? else { return Ok(None) };
        tokio::fs::write(path, &bytes).await.map_err(|error| error.to_string())?;
        Ok(Some(bytes.len() as u64))
    }
}

#[derive(Default)]
pub struct MemoryBlobStore {
    blobs: Mutex<HashMap<BlobDigest, Vec<u8>>>,
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String> {
        let digest = BlobDigest::of(bytes);
        self.blobs.lock().await.insert(digest.clone(), bytes.to_vec());
        Ok(digest)
    }
    async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String> {
        let bytes = self.blobs.lock().await.get(digest).cloned();
        if let Some(bytes) = &bytes {
            digest.verify(bytes)?;
        }
        Ok(bytes)
    }
    async fn has(&self, digest: &BlobDigest) -> Result<bool, String> {
        Ok(self.blobs.lock().await.contains_key(digest))
    }
    async fn delete(&self, digest: &BlobDigest) -> Result<(), String> {
        self.blobs.lock().await.remove(digest);
        Ok(())
    }
}

pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(state_dir: &Path) -> Self {
        Self { root: state_dir.join("blobs/sha256") }
    }
    fn path(&self, digest: &BlobDigest) -> PathBuf {
        self.root.join(&digest.0[..2]).join(&digest.0[2..])
    }
    async fn write_digest(&self, digest: &BlobDigest, bytes: &[u8]) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let path = self.path(digest);
        let parent = path.parent().expect("blob path has parent");
        tokio::fs::create_dir_all(parent).await.map_err(|error| format!("create blob directory: {error}"))?;
        // Re-read an existing blob so a repeated put can repair corruption.
        // The read costs O(blob size), but a plain stat would preserve a bad copy.
        if self.get(digest).await.ok().flatten().is_some() {
            return Ok(());
        }
        let temp = parent.join(format!(".{}-{}.tmp", digest.0, uuid::Uuid::new_v4()));
        let result = async {
            let mut file =
                tokio::fs::OpenOptions::new().write(true).create_new(true).open(&temp).await.map_err(|error| error.to_string())?;
            file.write_all(bytes).await.map_err(|error| error.to_string())?;
            file.sync_all().await.map_err(|error| error.to_string())?;
            tokio::fs::rename(&temp, &path).await.map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }

    async fn verified_file_size(&self, digest: &BlobDigest) -> Result<Option<u64>, String> {
        use tokio::io::AsyncReadExt;
        let mut file = match tokio::fs::File::open(self.path(digest)).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        let mut chunk = [0_u8; 65536];
        loop {
            let read = file.read(&mut chunk).await.map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
            size += read as u64;
        }
        if format!("{:x}", hash.finalize()) == digest.as_str() {
            Ok(Some(size))
        } else {
            Err(format!("blob digest mismatch for {}", digest.as_str()))
        }
    }

    async fn write_file(&self, digest: &BlobDigest, source: &Path) -> Result<(), String> {
        let path = self.path(digest);
        let parent = path.parent().expect("blob path has parent");
        tokio::fs::create_dir_all(parent).await.map_err(|error| error.to_string())?;
        if self.verified_file_size(digest).await.ok().flatten().is_some() {
            return Ok(());
        }
        let temp = parent.join(format!(".{}-{}.tmp", digest.as_str(), uuid::Uuid::new_v4()));
        let result = async {
            tokio::fs::copy(source, &temp).await.map_err(|error| error.to_string())?;
            tokio::fs::File::open(&temp).await.map_err(|error| error.to_string())?.sync_all().await.map_err(|error| error.to_string())?;
            tokio::fs::rename(&temp, &path).await.map_err(|error| error.to_string())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }
    pub async fn digests(&self) -> Result<Vec<BlobDigest>, String> {
        let mut result = Vec::new();
        let mut prefixes = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(error) => return Err(format!("read blob directory: {error}")),
        };
        while let Some(prefix) = prefixes.next_entry().await.map_err(|error| error.to_string())? {
            if !prefix.file_type().await.map_err(|error| error.to_string())?.is_dir() {
                continue;
            }
            let Some(head) = prefix.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let mut entries = tokio::fs::read_dir(prefix.path()).await.map_err(|error| error.to_string())?;
            while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
                if !entry.file_type().await.map_err(|error| error.to_string())?.is_file() {
                    continue;
                }
                let Some(tail) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if let Ok(digest) = BlobDigest::parse(&format!("{head}{tail}")) {
                    result.push(digest);
                }
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
}

#[async_trait]
impl BlobStore for LocalBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String> {
        let digest = BlobDigest::of(bytes);
        self.write_digest(&digest, bytes).await?;
        Ok(digest)
    }
    async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String> {
        match tokio::fs::read(self.path(digest)).await {
            Ok(bytes) => {
                digest.verify(&bytes)?;
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read local blob {}: {error}", digest.0)),
        }
    }
    async fn has(&self, digest: &BlobDigest) -> Result<bool, String> {
        Ok(tokio::fs::try_exists(self.path(digest)).await.map_err(|error| error.to_string())?)
    }
    async fn delete(&self, digest: &BlobDigest) -> Result<(), String> {
        match tokio::fs::remove_file(self.path(digest)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[derive(Deserialize)]
pub struct S3Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
}

pub struct S3BlobStore {
    endpoint: Url,
    bucket: String,
    prefix: String,
    region: String,
    credentials: S3Credentials,
    client: reqwest::Client,
    http: Arc<dyn HttpClient>,
    signing_time: Option<DateTime<Utc>>,
}

impl S3BlobStore {
    pub fn from_config(config: &BlobStoreConfig) -> Result<Self, String> {
        validate_config_transport(config)?;
        let credentials: S3Credentials = serde_json::from_slice(
            &std::fs::read(&config.credential_file)
                .map_err(|error| format!("read S3 credential reference {}: {error}", config.credential_file.display()))?,
        )
        .map_err(|error| format!("parse S3 credential reference: {error}"))?;
        Self::new_with_region(
            config.endpoint.as_str(),
            &config.bucket,
            &config.prefix,
            &config.region,
            credentials,
            Arc::new(ReqwestHttpClient::new()),
        )
    }
    pub fn new(endpoint: &str, bucket: &str, prefix: &str, credentials: S3Credentials, http: Arc<dyn HttpClient>) -> Result<Self, String> {
        Self::new_with_region(endpoint, bucket, prefix, "us-east-1", credentials, http)
    }
    pub fn new_with_region(
        endpoint: &str,
        bucket: &str,
        prefix: &str,
        region: &str,
        credentials: S3Credentials,
        http: Arc<dyn HttpClient>,
    ) -> Result<Self, String> {
        let endpoint = Url::parse(endpoint).map_err(|error| format!("invalid S3 endpoint: {error}"))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err("S3 endpoint must be an HTTP URL without query or fragment".into());
        }
        if bucket.is_empty() || !bucket.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.') {
            return Err("invalid S3 bucket".into());
        }
        if !prefix.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'/')) {
            return Err("S3 prefix contains unsupported characters".into());
        }
        if region.is_empty() || !region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("invalid S3 signing region".into());
        }
        if credentials.access_key_id.is_empty() || credentials.secret_access_key.is_empty() {
            return Err("S3 credentials are empty".into());
        }
        let client = flotilla_core::tls::client_builder().build().map_err(|error| format!("build S3 request client: {error}"))?;
        Ok(Self {
            endpoint,
            bucket: bucket.into(),
            prefix: prefix.trim_matches('/').into(),
            region: region.into(),
            credentials,
            client,
            http,
            signing_time: None,
        })
    }
    #[cfg(test)]
    fn with_signing_time(mut self, time: DateTime<Utc>) -> Self {
        self.signing_time = Some(time);
        self
    }
    fn object_url(&self, digest: &BlobDigest) -> Result<Url, String> {
        let mut url = self.endpoint.clone();
        let path = format!(
            "{}/{}/{}{}",
            self.endpoint.path().trim_end_matches('/'),
            self.bucket,
            if self.prefix.is_empty() { String::new() } else { format!("{}/", self.prefix) },
            digest.0
        );
        url.set_path(&path);
        Ok(url)
    }
    async fn request(
        &self,
        method: reqwest::Method,
        digest: &BlobDigest,
        body: Option<&[u8]>,
    ) -> Result<http::Response<bytes::Bytes>, String> {
        let url = self.object_url(digest)?;
        let payload_hash = format!("{:x}", Sha256::digest(body.unwrap_or_default()));
        let now = self.signing_time.unwrap_or_else(Utc::now);
        let date = now.format("%Y%m%d").to_string();
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().expect("URL has host")),
            None => url.host_str().expect("URL has host").to_string(),
        };
        let mut headers = format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{timestamp}\n");
        let mut signed = "host;x-amz-content-sha256;x-amz-date".to_string();
        if let Some(token) = &self.credentials.session_token {
            headers.push_str(&format!("x-amz-security-token:{token}\n"));
            signed.push_str(";x-amz-security-token");
        }
        let canonical = format!("{}\n{}\n\n{}\n{}\n{}", method.as_str(), url.path(), headers, signed, payload_hash);
        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let to_sign = format!("AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{:x}", Sha256::digest(canonical.as_bytes()));
        let key = sign(format!("AWS4{}", self.credentials.secret_access_key).as_bytes(), date.as_bytes())?;
        let key = sign(&key, self.region.as_bytes())?;
        let key = sign(&key, b"s3")?;
        let key = sign(&key, b"aws4_request")?;
        let signature = hex_lower(&sign(&key, to_sign.as_bytes())?);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.credentials.access_key_id
        );
        let mut builder = self
            .client
            .request(method, url.clone())
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", timestamp)
            .header("authorization", authorization);
        if let Some(token) = &self.credentials.session_token {
            builder = builder.header("x-amz-security-token", token);
        }
        if let Some(body) = body {
            builder = builder.body(body.to_vec());
        }
        let request = builder.build().map_err(|error| error.to_string())?;
        self.http.execute(request, &ChannelLabel::Http("blob-store".into())).await
    }
    fn check(status: http::StatusCode, expected: &[http::StatusCode]) -> Result<(), String> {
        if expected.contains(&status) {
            Ok(())
        } else {
            Err(format!("S3 returned HTTP {status}"))
        }
    }
}

fn validate_config_transport(config: &BlobStoreConfig) -> Result<(), String> {
    let endpoint = Url::parse(&config.endpoint).map_err(|error| format!("invalid S3 endpoint for store {}: {error}", config.bucket))?;
    if endpoint.scheme() != "http" {
        return Ok(());
    }
    let loopback = match endpoint.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !loopback && !config.allow_insecure_http {
        return Err(format!(
            "blob store {} uses non-loopback HTTP endpoint {}; set allow_insecure_http = true to opt in",
            config.bucket, config.endpoint
        ));
    }
    tracing::warn!(store = %config.bucket, endpoint = %config.endpoint, "blob store uses insecure HTTP transport");
    Ok(())
}

fn sign(key: &[u8], message: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|error| error.to_string())?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String> {
        let digest = BlobDigest::of(bytes);
        let response = self.request(reqwest::Method::PUT, &digest, Some(bytes)).await?;
        Self::check(response.status(), &[http::StatusCode::OK, http::StatusCode::CREATED])?;
        Ok(digest)
    }
    async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String> {
        let response = self.request(reqwest::Method::GET, digest, None).await?;
        if response.status() == http::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::check(response.status(), &[http::StatusCode::OK])?;
        let bytes = response.into_body().to_vec();
        digest.verify(&bytes)?;
        Ok(Some(bytes))
    }
    async fn has(&self, digest: &BlobDigest) -> Result<bool, String> {
        let response = self.request(reqwest::Method::HEAD, digest, None).await?;
        if response.status() == http::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        Self::check(response.status(), &[http::StatusCode::OK])?;
        Ok(true)
    }
    async fn delete(&self, digest: &BlobDigest) -> Result<(), String> {
        let response = self.request(reqwest::Method::DELETE, digest, None).await?;
        Self::check(response.status(), &[http::StatusCode::NO_CONTENT, http::StatusCode::OK])
    }
}

/// A local write returns as soon as the atomic local put succeeds. Markers are
/// per fleet target, so a restart can continue uploading only missing copies.
pub struct TieredBlobStore {
    local: Arc<LocalBlobStore>,
    fleet: Vec<FleetTarget>,
    markers: PathBuf,
    wake: Notify,
    inventory: Mutex<SyncInventory>,
    sync_lock: Mutex<()>,
}

#[derive(Default)]
struct SyncInventory {
    initialized: bool,
    known: HashSet<BlobDigest>,
    pending: HashMap<BlobDigest, HashSet<String>>,
    last_error: Option<String>,
}

impl SyncInventory {
    fn status(&self) -> BlobSyncStatus {
        BlobSyncStatus { pending_count: self.pending.values().map(HashSet::len).sum(), last_error: self.last_error.clone() }
    }
}

struct FleetTarget {
    id: String,
    store: Arc<dyn BlobStore>,
}

impl TieredBlobStore {
    pub fn new(state_dir: &Path, fleet: Vec<(String, Arc<dyn BlobStore>)>) -> Self {
        let fleet = fleet.into_iter().map(|(id, store)| FleetTarget { id, store }).collect();
        Self::with_targets(state_dir, fleet)
    }
    fn with_targets(state_dir: &Path, fleet: Vec<FleetTarget>) -> Self {
        Self {
            local: Arc::new(LocalBlobStore::new(state_dir)),
            fleet,
            markers: state_dir.join("blob-sync"),
            wake: Notify::new(),
            inventory: Mutex::new(SyncInventory::default()),
            sync_lock: Mutex::new(()),
        }
    }
    pub fn from_config(state_dir: &Path, fleet: &[BlobStoreConfig]) -> Result<Self, String> {
        let stores = fleet
            .iter()
            .map(|config| {
                let identity = format!("{}\n{}\n{}\n{}", config.endpoint, config.bucket, config.prefix, config.region);
                let id = format!("{:x}", Sha256::digest(identity.as_bytes()));
                S3BlobStore::from_config(config).map(|store| FleetTarget { id, store: Arc::new(store) })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::with_targets(state_dir, stores))
    }
    fn marker(&self, id: &str, digest: &BlobDigest) -> PathBuf {
        self.markers.join(id).join(digest.as_str())
    }
    pub async fn status(&self) -> BlobSyncStatus {
        self.inventory.lock().await.status()
    }
    async fn recover_inventory(&self) -> Result<(), String> {
        let mut inventory = self.inventory.lock().await;
        if inventory.initialized {
            return Ok(());
        }
        for digest in self.local.digests().await? {
            let mut missing = HashSet::new();
            for target in &self.fleet {
                if !tokio::fs::try_exists(self.marker(&target.id, &digest)).await.map_err(|error| error.to_string())? {
                    missing.insert(target.id.clone());
                }
            }
            inventory.known.insert(digest.clone());
            if missing.is_empty() {
                inventory.pending.remove(&digest);
            } else {
                inventory.pending.insert(digest, missing);
            }
        }
        inventory.initialized = true;
        Ok(())
    }
    async fn queue_new(&self, digest: &BlobDigest, was_present: bool, completed_target: Option<&str>) {
        let mut inventory = self.inventory.lock().await;
        if !was_present && inventory.known.insert(digest.clone()) {
            let missing: HashSet<_> =
                self.fleet.iter().filter(|target| Some(target.id.as_str()) != completed_target).map(|target| target.id.clone()).collect();
            if !missing.is_empty() {
                inventory.pending.insert(digest.clone(), missing);
                self.wake.notify_one();
            }
        }
    }

    /// Reap each tier independently, using local files and sync markers as
    /// that tier's inventory. Failed fleet deletions retain their marker and
    /// are retried on the next sweep.
    pub async fn gc_unreferenced(&self, referenced: &HashSet<BlobDigest>, grace: Duration) -> Result<(), String> {
        let mut failures = Vec::new();
        for digest in self.local.digests().await? {
            if !referenced.contains(&digest) && older_than(&self.local.path(&digest), grace).await? {
                if let Err(error) = self.local.delete(&digest).await {
                    failures.push(format!("local {}: {error}", digest.as_str()));
                }
            }
        }
        for target in &self.fleet {
            let directory = self.markers.join(&target.id);
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("read blob sync inventory: {error}")),
            };
            while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let Ok(digest) = BlobDigest::parse(&name) else { continue };
                if !referenced.contains(&digest) && older_than(&entry.path(), grace).await? {
                    match target.store.delete(&digest).await {
                        Ok(()) => {
                            if let Err(error) = tokio::fs::remove_file(entry.path()).await {
                                failures.push(format!("{} marker {}: {error}", target.id, digest.as_str()));
                            }
                        }
                        Err(error) => failures.push(format!("{} {}: {error}", target.id, digest.as_str())),
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
    pub async fn sync_once(&self) -> Result<BlobSyncStatus, String> {
        let _sync = self.sync_lock.lock().await;
        self.recover_inventory().await?;
        let mut digests: Vec<_> = self.inventory.lock().await.pending.keys().cloned().collect();
        digests.sort_by(|a, b| a.0.cmp(&b.0));
        let mut last_error = None;
        let mut unavailable = HashSet::new();
        for digest in digests {
            for target in &self.fleet {
                if !self.inventory.lock().await.pending.get(&digest).is_some_and(|ids| ids.contains(&target.id)) {
                    continue;
                }
                if unavailable.contains(&target.id) {
                    continue;
                }
                let marker = self.marker(&target.id, &digest);
                if tokio::fs::try_exists(&marker).await.map_err(|error| error.to_string())? {
                    self.mark_synced(&digest, &target.id).await;
                    continue;
                }
                let result = async {
                    let bytes = self.local.get(&digest).await?.ok_or_else(|| format!("local blob {} disappeared", digest.0))?;
                    if !target.store.has(&digest).await? {
                        let remote_digest = target.store.put(&bytes).await?;
                        if remote_digest != digest {
                            return Err("fleet store returned wrong digest".into());
                        }
                    }
                    tokio::fs::create_dir_all(marker.parent().expect("marker has parent")).await.map_err(|error| error.to_string())?;
                    tokio::fs::write(&marker, b"").await.map_err(|error| error.to_string())
                }
                .await;
                if let Err(error) = result {
                    last_error = Some(error);
                    unavailable.insert(&target.id);
                } else {
                    self.mark_synced(&digest, &target.id).await;
                }
            }
        }
        let mut inventory = self.inventory.lock().await;
        inventory.last_error = last_error;
        Ok(inventory.status())
    }
    async fn mark_synced(&self, digest: &BlobDigest, target: &str) {
        let mut inventory = self.inventory.lock().await;
        if let Some(missing) = inventory.pending.get_mut(digest) {
            missing.remove(target);
            if missing.is_empty() {
                inventory.pending.remove(digest);
            }
        }
    }
    pub async fn run_sync(self: Arc<Self>) {
        let mut delay = Duration::from_secs(1);
        loop {
            let failed = match self.sync_once().await {
                Ok(status) if status.last_error.is_none() => false,
                Ok(_) => true,
                Err(error) => {
                    self.inventory.lock().await.last_error = Some(error);
                    true
                }
            };
            if failed {
                delay = (delay * 2).min(Duration::from_secs(300));
                tokio::time::sleep(delay).await;
            } else {
                delay = Duration::from_secs(1);
                tokio::select! { () = self.wake.notified() => {}, () = tokio::time::sleep(Duration::from_secs(30)) => {} }
            }
        }
    }
}

async fn older_than(path: &Path, grace: Duration) -> Result<bool, String> {
    let modified = tokio::fs::metadata(path).await.map_err(|error| error.to_string())?.modified().map_err(|error| error.to_string())?;
    Ok(std::time::SystemTime::now().duration_since(modified).is_ok_and(|age| age >= grace))
}

#[async_trait]
impl BlobStore for TieredBlobStore {
    async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String> {
        let digest = BlobDigest::of(bytes);
        let was_present = self.local.has(&digest).await?;
        self.local.write_digest(&digest, bytes).await?;
        self.queue_new(&digest, was_present, None).await;
        Ok(digest)
    }

    async fn put_file(&self, path: &Path) -> Result<(BlobDigest, u64), String> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(path).await.map_err(|error| error.to_string())?;
        let mut hash = Sha256::new();
        let mut size = 0_u64;
        let mut chunk = [0_u8; 65536];
        loop {
            let read = file.read(&mut chunk).await.map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
            size += read as u64;
        }
        let digest = BlobDigest(format!("{:x}", hash.finalize()));
        let was_present = self.local.verified_file_size(&digest).await.ok().flatten().is_some();
        self.local.write_file(&digest, path).await?;
        if !was_present && !self.fleet.is_empty() {
            self.status.lock().await.pending_count += self.fleet.len();
            self.wake.notify_one();
        }
        Ok((digest, size))
    }

    async fn get_file(&self, digest: &BlobDigest, path: &Path) -> Result<Option<u64>, String> {
        if let Ok(Some(size)) = self.local.verified_file_size(digest).await {
            tokio::fs::copy(self.local.path(digest), path).await.map_err(|error| error.to_string())?;
            return Ok(Some(size));
        }
        let Some(bytes) = self.get(digest).await? else { return Ok(None) };
        tokio::fs::write(path, &bytes).await.map_err(|error| error.to_string())?;
        Ok(Some(bytes.len() as u64))
    }
    async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String> {
        let mut error = match self.local.get(digest).await {
            Ok(Some(bytes)) => return Ok(Some(bytes)),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        for target in &self.fleet {
            match target.store.get(digest).await {
                Ok(Some(bytes)) => {
                    if let Err(failure) = digest.verify(&bytes) {
                        error = Some(failure);
                        continue;
                    }
                    let was_present = self.local.has(digest).await?;
                    self.local.write_digest(digest, &bytes).await?;
                    let marker = self.marker(&target.id, digest);
                    tokio::fs::create_dir_all(marker.parent().expect("marker has parent")).await.map_err(|error| error.to_string())?;
                    tokio::fs::write(marker, b"").await.map_err(|error| error.to_string())?;
                    self.queue_new(digest, was_present, Some(&target.id)).await;
                    return Ok(Some(bytes));
                }
                Ok(None) => {}
                Err(failure) => error = Some(failure),
            }
        }
        match error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }
    async fn has(&self, digest: &BlobDigest) -> Result<bool, String> {
        if self.local.has(digest).await? {
            return Ok(true);
        }
        for target in &self.fleet {
            if target.store.has(digest).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    async fn delete(&self, digest: &BlobDigest) -> Result<(), String> {
        self.local.delete(digest).await?;
        {
            let mut inventory = self.inventory.lock().await;
            inventory.known.remove(digest);
            inventory.pending.remove(digest);
        }
        for target in &self.fleet {
            target.store.delete(digest).await?;
            let _ = tokio::fs::remove_file(self.marker(&target.id, digest)).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use flotilla_core::providers::replay::{self, Masks};

    use super::*;

    #[test]
    fn configured_http_requires_loopback_or_explicit_opt_in() {
        let config = |endpoint: &str, allow_insecure_http| BlobStoreConfig {
            endpoint: endpoint.into(),
            bucket: "test-bucket".into(),
            region: "us-east-1".into(),
            prefix: String::new(),
            credential_file: PathBuf::from("missing-credentials.json"),
            allow_insecure_http,
        };
        for endpoint in ["http://localhost:9000", "http://127.25.1.2:9000", "http://[::1]:9000"] {
            assert!(validate_config_transport(&config(endpoint, false)).is_ok(), "{endpoint}");
        }
        assert!(validate_config_transport(&config("https://storage.example.com", false)).is_ok());
        assert!(validate_config_transport(&config("http://storage.example.com", true)).is_ok());
        for endpoint in ["http://storage.example.com", "http://192.168.1.2", "http://[::2]"] {
            assert!(validate_config_transport(&config(endpoint, false)).expect_err(endpoint).contains("allow_insecure_http"));
            assert!(S3BlobStore::from_config(&config(endpoint, false)).err().expect(endpoint).contains("allow_insecure_http"));
        }
    }

    #[tokio::test]
    async fn concurrent_duplicate_puts_count_one_pending_copy() {
        let state = tempfile::tempdir().expect("state dir");
        let tiered = Arc::new(TieredBlobStore::new(state.path(), vec![("fleet".into(), Arc::new(MemoryBlobStore::default()))]));
        let puts: Vec<_> = (0..32)
            .map(|_| {
                let tiered = Arc::clone(&tiered);
                tokio::spawn(async move { tiered.put(b"concurrent duplicate").await.expect("put") })
            })
            .collect();
        for put in puts {
            put.await.expect("join put");
        }
        assert_eq!(tiered.status().await.pending_count, 1);
        assert_eq!(tiered.sync_once().await.expect("sync").pending_count, 0);
        tiered.put(b"concurrent duplicate").await.expect("repeat put");
        assert_eq!(tiered.status().await.pending_count, 0);
    }

    #[tokio::test]
    async fn restart_recovers_unsynced_blobs_and_later_passes_use_pending_inventory() {
        let state = tempfile::tempdir().expect("state dir");
        let fleet = Arc::new(MemoryBlobStore::default());
        let first = TieredBlobStore::new(state.path(), vec![("fleet".into(), fleet.clone())]);
        let digest = first.put(b"restart recovery").await.expect("put");
        drop(first);
        let restarted = TieredBlobStore::new(state.path(), vec![("fleet".into(), fleet.clone())]);
        assert_eq!(restarted.sync_once().await.expect("recover and sync").pending_count, 0);
        assert!(fleet.has(&digest).await.expect("fleet copy"));
        tokio::fs::rename(&restarted.local.root, state.path().join("hidden-blobs")).await.expect("hide local directory");
        assert_eq!(restarted.sync_once().await.expect("empty pending pass needs no directory scan").pending_count, 0);
    }

    #[tokio::test]
    async fn fleet_cache_fill_queues_other_targets() {
        let state = tempfile::tempdir().expect("state dir");
        let source = Arc::new(MemoryBlobStore::default());
        let mirror = Arc::new(MemoryBlobStore::default());
        let digest = source.put(b"cached from source").await.expect("source put");
        let tiered = TieredBlobStore::new(state.path(), vec![("source".into(), source), ("mirror".into(), mirror.clone())]);
        assert_eq!(tiered.get(&digest).await.expect("fleet read"), Some(b"cached from source".to_vec()));
        assert_eq!(tiered.status().await.pending_count, 1);
        assert_eq!(tiered.sync_once().await.expect("sync mirror").pending_count, 0);
        assert!(mirror.has(&digest).await.expect("mirror copy"));
    }

    async fn contract(store: &dyn BlobStore) {
        let missing = BlobDigest::of(b"missing blob");
        assert!(!store.has(&missing).await.expect("has missing"));
        assert_eq!(store.get(&missing).await.expect("get missing"), None);
        store.delete(&missing).await.expect("delete missing is idempotent");
        for bytes in [b"".as_slice(), b"blob contract payload".as_slice()] {
            let digest = BlobDigest::of(bytes);
            assert_eq!(store.put(bytes).await.expect("put"), digest);
            assert_eq!(store.put(bytes).await.expect("duplicate put"), digest);
            assert!(store.has(&digest).await.expect("has present"));
            assert_eq!(store.get(&digest).await.expect("get present"), Some(bytes.to_vec()));
            store.delete(&digest).await.expect("delete present");
            assert!(!store.has(&digest).await.expect("has deleted"));
            assert_eq!(store.get(&digest).await.expect("get deleted"), None);
        }
    }

    #[tokio::test]
    async fn memory_contract() {
        contract(&MemoryBlobStore::default()).await;
    }

    #[tokio::test]
    async fn local_contract() {
        let state = tempfile::tempdir().expect("state dir");
        contract(&LocalBlobStore::new(state.path())).await;
    }

    #[tokio::test]
    async fn local_read_verifies_digest_and_ignores_partial_writes() {
        let state = tempfile::tempdir().expect("state dir");
        let local = LocalBlobStore::new(state.path());
        let digest = local.put(b"complete").await.expect("put");
        let path = local.path(&digest);
        tokio::fs::write(path.parent().expect("parent").join(format!(".{}-partial.tmp", digest.as_str())), b"partial")
            .await
            .expect("write partial");
        assert_eq!(local.digests().await.expect("inventory"), vec![digest.clone()]);
        tokio::fs::write(path, b"corrupted").await.expect("corrupt local blob");
        assert!(local.get(&digest).await.expect_err("verify digest").contains("digest mismatch"));
        local.put(b"complete").await.expect("repair corrupt local blob");
        assert_eq!(local.get(&digest).await.expect("read repair"), Some(b"complete".to_vec()));
    }

    #[tokio::test]
    async fn s3_contract() {
        let fixture = format!("{}/src/fixtures/s3_blob_contract.yaml", env!("CARGO_MANIFEST_DIR"));
        let endpoint = std::env::var("BLOB_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        let bucket = "flotilla-blob-contract";
        let credentials = S3Credentials {
            access_key_id: "TESTACCESSKEY".into(),
            secret_access_key: "TestSecretKeyForLocalReplayOnly".into(),
            session_token: None,
        };
        let mut masks = Masks::new();
        masks.add(&endpoint, "{s3_endpoint}");
        masks.add(bucket, "{s3_bucket}");
        masks.add(&credentials.access_key_id, "{s3_access_key}");
        masks.add(&credentials.secret_access_key, "{s3_secret_key}");
        let signing_time = if replay::is_live() {
            Utc::now()
        } else {
            let yaml: serde_yml::Value =
                serde_yml::from_slice(&std::fs::read(&fixture).expect("read S3 fixture")).expect("parse S3 fixture");
            let timestamp = yaml["rounds"][0]["interactions"]
                .as_sequence()
                .expect("fixture interactions")
                .iter()
                .find_map(|interaction| interaction["request_headers"]["x-amz-date"].as_str())
                .expect("recorded signing date");
            chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ").expect("signing date").and_utc()
        };
        let session = replay::test_session(&fixture, masks);
        let store = S3BlobStore::new(&endpoint, bucket, "contracts", credentials, replay::test_http_client(&session))
            .expect("S3 store")
            .with_signing_time(signing_time);
        contract(&store).await;
        session.finish();
    }

    struct SwitchableFleet {
        available: AtomicBool,
        inner: MemoryBlobStore,
    }
    impl SwitchableFleet {
        fn new() -> Self {
            Self { available: AtomicBool::new(false), inner: MemoryBlobStore::default() }
        }
    }
    #[async_trait]
    impl BlobStore for SwitchableFleet {
        async fn put(&self, bytes: &[u8]) -> Result<BlobDigest, String> {
            if !self.available.load(Ordering::SeqCst) {
                return Err("fleet unavailable".into());
            }
            self.inner.put(bytes).await
        }
        async fn get(&self, digest: &BlobDigest) -> Result<Option<Vec<u8>>, String> {
            if !self.available.load(Ordering::SeqCst) {
                return Err("fleet unavailable".into());
            }
            self.inner.get(digest).await
        }
        async fn has(&self, digest: &BlobDigest) -> Result<bool, String> {
            if !self.available.load(Ordering::SeqCst) {
                return Err("fleet unavailable".into());
            }
            self.inner.has(digest).await
        }
        async fn delete(&self, digest: &BlobDigest) -> Result<(), String> {
            if !self.available.load(Ordering::SeqCst) {
                return Err("fleet unavailable".into());
            }
            self.inner.delete(digest).await
        }
    }

    #[tokio::test]
    async fn local_first_sync_recovers_and_remote_read_caches() {
        let state = tempfile::tempdir().expect("state dir");
        let fleet = Arc::new(SwitchableFleet::new());
        let tiered = TieredBlobStore::new(state.path(), vec![("test-fleet".into(), fleet.clone())]);
        let digest = tiered.put(b"offline write").await.expect("local-first put");
        assert_eq!(tiered.get(&digest).await.expect("local read"), Some(b"offline write".to_vec()));
        let failed = tiered.sync_once().await.expect("sync inventory");
        assert_eq!(failed.pending_count, 1);
        assert!(failed.last_error.expect("sync error").contains("unavailable"));
        fleet.available.store(true, Ordering::SeqCst);
        let recovered = tiered.sync_once().await.expect("retry sync");
        assert_eq!(recovered, BlobSyncStatus::default());
        assert!(fleet.has(&digest).await.expect("remote has blob"));
        tokio::fs::write(tiered.local.path(&digest), b"corrupt local copy").await.expect("corrupt local copy");
        assert_eq!(tiered.get(&digest).await.expect("repair local from fleet"), Some(b"offline write".to_vec()));
        tiered.local.delete(&digest).await.expect("drop local copy");
        assert_eq!(tiered.get(&digest).await.expect("fleet fallback"), Some(b"offline write".to_vec()));
        fleet.available.store(false, Ordering::SeqCst);
        assert_eq!(tiered.get(&digest).await.expect("cached read"), Some(b"offline write".to_vec()));
    }

    #[tokio::test]
    async fn sync_markers_follow_store_identity_across_reorder() {
        let state = tempfile::tempdir().expect("state dir");
        let first = Arc::new(MemoryBlobStore::default());
        let second = Arc::new(MemoryBlobStore::default());
        let tiered = TieredBlobStore::new(state.path(), vec![("first".into(), first.clone()), ("second".into(), second.clone())]);
        let digest = tiered.put(b"persisted inventory").await.expect("put");
        assert_eq!(tiered.sync_once().await.expect("sync").pending_count, 0);
        let reordered = TieredBlobStore::new(state.path(), vec![("second".into(), second.clone()), ("first".into(), first.clone())]);
        assert_eq!(reordered.sync_once().await.expect("sync after reorder").pending_count, 0);
        assert!(first.has(&digest).await.expect("first copy"));
        assert!(second.has(&digest).await.expect("second copy"));
    }

    #[tokio::test]
    async fn corrupt_fleet_blob_is_rejected() {
        let state = tempfile::tempdir().expect("state dir");
        let fleet = Arc::new(MemoryBlobStore::default());
        let digest = fleet.put(b"valid bytes").await.expect("put fleet blob");
        fleet.blobs.lock().await.insert(digest.clone(), b"wrong bytes".to_vec());
        let tiered = TieredBlobStore::new(state.path(), vec![("fleet".into(), fleet)]);
        assert!(tiered.get(&digest).await.expect_err("reject corrupt fleet blob").contains("digest mismatch"));
        assert!(!tiered.local.has(&digest).await.expect("no corrupt cache"));
    }

    #[tokio::test]
    async fn corrupt_fleet_mirror_falls_back_to_healthy_mirror() {
        let state = tempfile::tempdir().expect("state dir");
        let corrupt = Arc::new(MemoryBlobStore::default());
        let healthy = Arc::new(MemoryBlobStore::default());
        let digest = corrupt.put(b"valid bytes").await.expect("put corrupt mirror blob");
        healthy.put(b"valid bytes").await.expect("put healthy mirror blob");
        corrupt.blobs.lock().await.insert(digest.clone(), b"wrong bytes".to_vec());
        let tiered = TieredBlobStore::new(state.path(), vec![("corrupt".into(), corrupt), ("healthy".into(), healthy)]);

        assert_eq!(tiered.get(&digest).await.expect("read healthy mirror"), Some(b"valid bytes".to_vec()));
        assert_eq!(tiered.local.get(&digest).await.expect("cache valid blob"), Some(b"valid bytes".to_vec()));
    }

    #[tokio::test]
    async fn gc_reaps_unreferenced_copies_per_backend_and_retries_unavailable_fleet() {
        let state = tempfile::tempdir().expect("state dir");
        let unavailable = Arc::new(SwitchableFleet::new());
        unavailable.available.store(true, Ordering::SeqCst);
        let healthy = Arc::new(MemoryBlobStore::default());
        let tiered =
            TieredBlobStore::new(state.path(), vec![("unavailable".into(), unavailable.clone()), ("healthy".into(), healthy.clone())]);
        let stale = tiered.put(b"stale").await.expect("put stale");
        let keep = tiered.put(b"keep").await.expect("put referenced");
        tiered.sync_once().await.expect("sync both tiers");
        unavailable.available.store(false, Ordering::SeqCst);
        let referenced = HashSet::from([keep.clone()]);
        assert!(tiered.gc_unreferenced(&referenced, Duration::ZERO).await.is_err());
        assert!(!tiered.local.has(&stale).await.expect("local stale removed"));
        assert!(!healthy.has(&stale).await.expect("healthy stale removed"));
        assert!(healthy.has(&keep).await.expect("referenced copy kept"));
        unavailable.available.store(true, Ordering::SeqCst);
        tiered.gc_unreferenced(&referenced, Duration::ZERO).await.expect("retry unavailable backend");
        assert!(!unavailable.has(&stale).await.expect("stale copy removed on retry"));
    }
}
