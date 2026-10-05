//! Cached key/version anti-entropy. Bodies are read only for differing buckets.
use std::collections::BTreeMap;

use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Resource, ResourceError};

pub const DIGEST_FANOUT: usize = 256;

pub use flotilla_protocol::ResourceDigestQuery as DigestQuery;

/// Hashing and validation behavior around the protocol's typed data envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartitionDigest(flotilla_protocol::ResourceDigest);

impl std::ops::Deref for PartitionDigest {
    type Target = flotilla_protocol::ResourceDigest;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for PartitionDigest {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl From<flotilla_protocol::ResourceDigest> for PartitionDigest {
    fn from(value: flotilla_protocol::ResourceDigest) -> Self {
        Self(value)
    }
}
impl From<PartitionDigest> for flotilla_protocol::ResourceDigest {
    fn from(value: PartitionDigest) -> Self {
        value.0
    }
}

pub fn digest_bucket(name: &str) -> u8 {
    Sha256::digest(name.as_bytes())[0]
}

fn field(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
}

pub(crate) fn hash_entries(entries: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"flotilla-key-version-leaf-v1");
    for (name, version) in entries {
        field(&mut hash, name);
        field(&mut hash, version);
    }
    format!("{:x}", hash.finalize())
}

#[derive(Debug, Clone)]
pub(crate) struct DigestIndex {
    entries: Vec<BTreeMap<String, String>>,
    hashes: Vec<String>,
}

impl Default for DigestIndex {
    fn default() -> Self {
        Self { entries: vec![BTreeMap::new(); DIGEST_FANOUT], hashes: vec![hash_entries(&BTreeMap::new()); DIGEST_FANOUT] }
    }
}

impl DigestIndex {
    pub fn set(&mut self, name: &str, version: Option<&str>) {
        let bucket = digest_bucket(name) as usize;
        match version {
            Some(version) => {
                self.entries[bucket].insert(name.to_owned(), version.to_owned());
            }
            None => {
                self.entries[bucket].remove(name);
            }
        }
        self.hashes[bucket] = hash_entries(&self.entries[bucket]);
    }
    pub fn hashes(&self) -> Vec<String> {
        self.hashes.clone()
    }
    pub fn names(&self, bucket: u8) -> impl Iterator<Item = &String> {
        self.entries[bucket as usize].keys()
    }
}

impl PartitionDigest {
    /// Validate a complete hierarchy before using any of its absence proofs.
    pub fn validate_tree<T: Resource>(&self) -> Result<(), ResourceError> {
        let hashes = self
            .children
            .as_ref()
            .filter(|hashes| hashes.len() == DIGEST_FANOUT)
            .ok_or_else(|| ResourceError::invalid("incomplete digest tree"))?;
        let computed =
            Self::new::<T>(self.origin.clone(), &self.namespace, self.generation.clone(), self.resource_version.clone(), hashes.clone());
        if computed.root != self.root || self.kind != T::API_PATHS.kind {
            return Err(ResourceError::invalid("invalid digest hierarchy"));
        }
        Ok(())
    }

    /// A complete bucket must hash to the authority's advertised leaf. A
    /// truncated response, duplicate key or wrong identity never proves absence.
    pub fn snapshot<T: Resource>(&self, expected: &Self, bucket: u8) -> Result<crate::ResourceList<T>, ResourceError> {
        if self.origin != expected.origin
            || self.kind != T::API_PATHS.kind
            || self.namespace != expected.namespace
            || self.generation != expected.generation
            || self.root != expected.root
            || self.bucket != Some(bucket)
        {
            return Err(ResourceError::invalid("digest snapshot identity mismatch"));
        }
        let mut entries = BTreeMap::new();
        let mut items = Vec::new();
        for value in self.items.as_ref().ok_or_else(|| ResourceError::invalid("incomplete digest snapshot"))? {
            let object = crate::ResourceObject::<T>::from_k8s_object(
                serde_json::from_value(value.clone()).map_err(|error| ResourceError::decode(error.to_string()))?,
            )?;
            if object.metadata.namespace != self.namespace
                || digest_bucket(&object.metadata.name) != bucket
                || entries.insert(object.metadata.name.clone(), object.metadata.resource_version.clone()).is_some()
            {
                return Err(ResourceError::invalid("digest snapshot contains invalid keys"));
            }
            items.push(object);
        }
        let hash = expected
            .children
            .as_ref()
            .and_then(|children| children.get(bucket as usize))
            .ok_or_else(|| ResourceError::invalid("incomplete digest tree"))?;
        if hash_entries(&entries) != *hash {
            return Err(ResourceError::invalid("incomplete digest bucket"));
        }
        Ok(crate::ResourceList { items, generation: self.generation.clone(), resource_version: self.resource_version.clone() })
    }

    pub(crate) fn new<T: Resource>(
        origin: NodeId,
        namespace: &str,
        generation: Option<String>,
        resource_version: String,
        hashes: Vec<String>,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"flotilla-partition-root-v1");
        for identity in [origin.as_str(), T::API_PATHS.group, T::API_PATHS.version, T::API_PATHS.kind, namespace] {
            field(&mut hash, identity);
        }
        // Distinguish an absent generation from an empty generation.
        hash.update([u8::from(generation.is_some())]);
        field(&mut hash, generation.as_deref().unwrap_or_default());
        for (bucket, child) in hashes.iter().enumerate() {
            hash.update([bucket as u8]);
            field(&mut hash, child);
        }
        Self(flotilla_protocol::ResourceDigest {
            origin,
            kind: T::API_PATHS.kind.into(),
            namespace: namespace.into(),
            generation,
            resource_version,
            root: format!("{:x}", hash.finalize()),
            children: Some(hashes),
            bucket: None,
            items: None,
        })
    }
    pub(crate) fn select(mut self, query: &DigestQuery) -> Result<Self, ResourceError> {
        let expected = match query {
            DigestQuery::Root => None,
            DigestQuery::Children { expected_root } | DigestQuery::Snapshot { expected_root, .. } => Some(expected_root),
        };
        if expected.is_some_and(|root| root != &self.root) {
            return Err(ResourceError::conflict("digest", "partition changed during digest drill-down"));
        }
        if !matches!(query, DigestQuery::Children { .. }) {
            self.children = None;
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Convoy, ConvoySpec, InMemoryBackend, InputMeta, ResourceBackend};

    // Length-framed key/version hashing cannot confuse field boundaries.
    #[test]
    fn key_version_boundaries_are_unambiguous() {
        assert_ne!(hash_entries(&BTreeMap::from([("ab".into(), "c".into())])), hash_entries(&BTreeMap::from([("a".into(), "bc".into())])));
    }

    // Incomplete snapshots and trees, including a missing live key, never
    // provide the authority's proof of absence.
    #[tokio::test]
    async fn truncated_bucket_and_tree_are_rejected() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let local = backend.using::<Convoy>("ns");
        local
            .create(&InputMeta::builder().name("retained".into()).build(), &ConvoySpec::builder().workflow_ref("workflow".into()).build())
            .await
            .expect("create");
        let root = local.digest(&DigestQuery::Root).await.expect("root");
        let children = local.digest(&DigestQuery::Children { expected_root: root.root.clone() }).await.expect("tree");
        let bucket = digest_bucket("retained");
        let mut snapshot = local.digest(&DigestQuery::Snapshot { expected_root: root.root.clone(), bucket }).await.expect("snapshot");
        snapshot.snapshot::<Convoy>(&children, bucket).expect("complete snapshot");
        let mut duplicate = snapshot.clone();
        duplicate.items.as_mut().expect("items").push(snapshot.items.as_ref().expect("items")[0].clone());
        assert!(duplicate.snapshot::<Convoy>(&children, bucket).is_err(), "duplicate keys are invalid");
        snapshot.items.as_mut().expect("items").clear();
        assert!(snapshot.snapshot::<Convoy>(&children, bucket).is_err(), "truncated live set is invalid");
        let mut truncated = children;
        truncated.children = Some(vec![]);
        assert!(snapshot.snapshot::<Convoy>(&truncated, bucket).is_err(), "an incomplete tree cannot prove absence");
    }

    // Manual sizing benchmark: cached key/version index only; no resources or
    // bodies are serialized. Bootstrap is O(N), match work is O(fanout), and a
    // mismatch transfers only leaves containing divergent keys.
    #[test]
    #[ignore = "manual 1k/100k/1m sizing benchmark"]
    fn benchmark_digest_index() {
        use std::time::Instant;
        for size in [1_000usize, 100_000, 1_000_000] {
            for fanout in [64usize, 256, 1024] {
                let started = Instant::now();
                let mut entries = vec![BTreeMap::new(); fanout];
                for key in 0..size {
                    let name = format!("key-{key:08}");
                    let digest = Sha256::digest(name.as_bytes());
                    let bucket = u16::from_be_bytes([digest[0], digest[1]]) as usize % fanout;
                    entries[bucket].insert(name, (key + 1).to_string());
                }
                let hashes = entries.iter().map(hash_entries).collect::<Vec<_>>();
                let bootstrap = started.elapsed();
                let started = Instant::now();
                for _ in 0..100 {
                    let mut hash = Sha256::new();
                    for child in &hashes {
                        field(&mut hash, child);
                    }
                    std::hint::black_box(hash.finalize());
                }
                let root = started.elapsed() / 100;
                for changes in [1usize, size / 5] {
                    let mut touched = std::collections::BTreeSet::new();
                    let started = Instant::now();
                    for key in 0..changes {
                        let name = format!("key-{key:08}");
                        let digest = Sha256::digest(name.as_bytes());
                        let bucket = u16::from_be_bytes([digest[0], digest[1]]) as usize % fanout;
                        entries[bucket].insert(name, format!("changed-{key}"));
                        touched.insert(bucket);
                    }
                    // A dense batch is measured with one leaf refresh per touched
                    // bucket; individual committed writes also pay a leaf hash.
                    for bucket in &touched {
                        std::hint::black_box(hash_entries(&entries[*bucket]));
                    }
                    let repair_keys = touched.iter().map(|bucket| entries[*bucket].len()).sum::<usize>();
                    println!("keys={size} fanout={fanout} changes={changes} bootstrap_us={} root_us={} refresh_us={} buckets={} snapshot_keys={repair_keys} child_hash_bytes={}",bootstrap.as_micros(),root.as_micros(),started.elapsed().as_micros(),touched.len(),fanout*64);
                }
            }
        }
    }
}
