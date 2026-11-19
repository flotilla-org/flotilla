//! What artifact retention reads, kept from one listing and each change after (#3108).
//!
//! One listing of every home's artifacts builds it: this home's unpinned artifacts by expiry, and
//! how many artifacts reference each blob. Each artifact change then updates only that artifact's
//! entry. Retention deletes this home's artifacts from the head of the expiry queue; a convoy's
//! retirement deletes none of them. A blob whose last reference goes is collected once its grace
//! has passed. Nothing is listed again to find them.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use flotilla_resources::{Artifact, ReadResourceObject, ReadWatchEvent, ResourceProvenance};

use crate::blob_store::BlobDigest;

/// An artifact by name and the home it is held at: none for this home's.
type Key = (String, Option<String>);

/// What retention reads of one artifact.
struct Entry {
    /// The blob it references.
    digest: BlobDigest,
    /// When it expires, for this home's unpinned artifacts: retention deletes only those.
    expires_at: Option<DateTime<Utc>>,
}

/// Every home's artifacts as retention reads them.
pub(crate) struct Retention {
    grace: chrono::Duration,
    artifacts: HashMap<Key, Entry>,
    /// This home's unpinned artifacts, by when they expire.
    expiry: BTreeSet<(DateTime<Utc>, String)>,
    /// How many artifacts reference each blob.
    references: HashMap<BlobDigest, usize>,
    /// The blobs no artifact references any more, by when they may be collected.
    unreferenced: BTreeSet<(DateTime<Utc>, String)>,
    collectable: HashMap<BlobDigest, DateTime<Utc>>,
}

impl Retention {
    /// Nothing held yet; a blob's last reference going makes it collectable `grace` later.
    pub(crate) fn new(grace: std::time::Duration) -> Self {
        Self {
            grace: chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX),
            artifacts: HashMap::new(),
            expiry: BTreeSet::new(),
            references: HashMap::new(),
            unreferenced: BTreeSet::new(),
            collectable: HashMap::new(),
        }
    }

    /// Takes in one change at `now`.
    pub(crate) fn apply(&mut self, event: ReadWatchEvent<Artifact>, now: DateTime<Utc>) {
        match event {
            ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) => self.insert(item, now),
            ReadWatchEvent::Deleted(item) => self.remove(&key(&item.object.metadata.name, &item.provenance), now),
            ReadWatchEvent::DeletedByName { tombstone, provenance } => self.remove(&key(&tombstone.name, &provenance), now),
        }
    }

    fn insert(&mut self, item: ReadResourceObject<Artifact>, now: DateTime<Utc>) {
        let key = key(&item.object.metadata.name, &item.provenance);
        self.remove(&key, now);
        let spec = item.object.spec;
        // Every artifact names a blob: a put and the import both refuse one that does not.
        let Ok(digest) = BlobDigest::parse(&spec.digest) else { return };
        let local = key.1.is_none() && !spec.pinned;
        *self.references.entry(digest.clone()).or_default() += 1;
        if let Some(at) = self.collectable.remove(&digest) {
            self.unreferenced.remove(&(at, digest.as_str().to_owned()));
        }
        let entry = Entry { digest, expires_at: local.then_some(spec.expires_at) };
        if let Some(expires_at) = entry.expires_at {
            self.expiry.insert((expires_at, key.0.clone()));
        }
        self.artifacts.insert(key, entry);
    }

    fn remove(&mut self, key: &Key, now: DateTime<Utc>) {
        let Some(entry) = self.artifacts.remove(key) else { return };
        if let Some(expires_at) = entry.expires_at {
            self.expiry.remove(&(expires_at, key.0.clone()));
        }
        let Some(count) = self.references.get_mut(&entry.digest) else { return };
        *count -= 1;
        if *count == 0 {
            self.references.remove(&entry.digest);
            self.defer(&entry.digest, now + self.grace);
        }
    }

    /// This home's artifact `name`, deleted at `now`.
    pub(crate) fn deleted(&mut self, name: &str, now: DateTime<Utc>) {
        self.remove(&(name.to_owned(), None), now);
    }

    /// `digest` was put at `now` for an artifact whose write then failed: unless an artifact
    /// references it, it is collectable a grace later.
    pub(crate) fn unwritten(&mut self, digest: &BlobDigest, now: DateTime<Utc>) {
        if !self.references.contains_key(digest) {
            self.defer(digest, now + self.grace);
        }
    }

    /// This home's unpinned artifacts expired at `now`.
    pub(crate) fn expired(&self, now: DateTime<Utc>) -> Vec<String> {
        self.expiry.iter().take_while(|(expires_at, _)| *expires_at <= now).map(|(_, name)| name.clone()).collect()
    }

    /// The blobs no artifact references whose grace has passed at `now`.
    pub(crate) fn collectable(&self, now: DateTime<Utc>) -> Vec<BlobDigest> {
        self.unreferenced.iter().take_while(|(at, _)| *at <= now).filter_map(|(_, digest)| BlobDigest::parse(digest).ok()).collect()
    }

    /// `digest` is collected, or referenced again.
    pub(crate) fn collected(&mut self, digest: &BlobDigest) {
        if let Some(at) = self.collectable.remove(digest) {
            self.unreferenced.remove(&(at, digest.as_str().to_owned()));
        }
    }

    /// `digest` is collectable at `at`, and not before.
    pub(crate) fn defer(&mut self, digest: &BlobDigest, at: DateTime<Utc>) {
        self.collected(digest);
        self.unreferenced.insert((at, digest.as_str().to_owned()));
        self.collectable.insert(digest.clone(), at);
    }

    /// The blobs some artifact references.
    pub(crate) fn referenced(&self) -> HashSet<BlobDigest> {
        self.references.keys().cloned().collect()
    }

    /// When the next unpinned artifact here expires or the next blob's grace passes.
    pub(crate) fn next_deadline(&self) -> Option<DateTime<Utc>> {
        let expiry = self.expiry.first().map(|(at, _)| *at);
        let grace = self.unreferenced.first().map(|(at, _)| *at);
        expiry.into_iter().chain(grace).min()
    }

    /// [`Self::next_deadline`] still ahead of `now`: what is already due and failing is skipped.
    pub(crate) fn next_deadline_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let expiry = self.expiry.iter().map(|(at, _)| *at).find(|at| *at > now);
        let grace = self.unreferenced.iter().map(|(at, _)| *at).find(|at| *at > now);
        expiry.into_iter().chain(grace).min()
    }
}

fn key(name: &str, provenance: &ResourceProvenance) -> Key {
    match provenance {
        ResourceProvenance::Local => (name.to_owned(), None),
        ResourceProvenance::Replica { origin_root, .. } => (name.to_owned(), Some(origin_root.as_str().to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{ArtifactSpec, ObjectMeta, ResourceObject};

    use super::*;

    fn artifact(name: &str, digest: &str, provenance: ResourceProvenance) -> ReadResourceObject<Artifact> {
        let spec = ArtifactSpec::builder()
            .convoy("demo".into())
            .producer("coder".into())
            .kind("review-round".into())
            .subject(name.into())
            .digest(digest.into())
            .size(4)
            .media_type("text/plain".into())
            .expires_at(Utc::now() + chrono::Duration::days(1))
            .build();
        let metadata = ObjectMeta {
            name: name.into(),
            namespace: "flotilla".into(),
            resource_version: "1".into(),
            labels: Default::default(),
            annotations: Default::default(),
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: Utc::now(),
            merge: None,
        };
        ReadResourceObject { object: ResourceObject { metadata, spec, status: None }, provenance }
    }

    // Behaviour (#3108): a blob becomes collectable a grace after its last reference goes, at any
    // home, and not while another artifact references it; a new reference takes it back.
    #[test]
    fn a_blob_is_collectable_a_grace_after_its_last_reference_goes() {
        let digest = BlobDigest::parse(&flotilla_resources::artifact::blob_digest_of(b"body")).expect("digest");
        let elsewhere = ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("there"), last_synced_at: Utc::now() };
        let grace = chrono::Duration::hours(1);
        let now = Utc::now();
        let mut retention = Retention::new(grace.to_std().expect("grace"));
        retention.apply(ReadWatchEvent::Added(artifact("here", digest.as_str(), ResourceProvenance::Local)), now);
        retention.apply(ReadWatchEvent::Added(artifact("there", digest.as_str(), elsewhere.clone())), now);

        retention.deleted("here", now);
        assert!(retention.referenced().contains(&digest), "another home's artifact still references it");
        assert_eq!(retention.next_deadline(), None, "nothing here expires and nothing is collectable");

        retention.apply(ReadWatchEvent::Deleted(artifact("there", digest.as_str(), elsewhere)), now);
        assert!(retention.referenced().is_empty());
        assert!(retention.collectable(now + grace - chrono::Duration::seconds(1)).is_empty(), "within its grace");
        assert_eq!(retention.collectable(now + grace), vec![digest.clone()]);
        assert_eq!(retention.next_deadline(), Some(now + grace));

        retention.apply(ReadWatchEvent::Added(artifact("again", digest.as_str(), ResourceProvenance::Local)), now);
        assert!(retention.collectable(now + grace).is_empty(), "referenced again");
    }
}
