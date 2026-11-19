//! Cached key/version index of the old store's replica sets, by name bucket.
use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

// The first SHA-256 byte selects the bucket.
pub const DIGEST_FANOUT: usize = 256;

pub fn digest_bucket(name: &str) -> u8 {
    Sha256::digest(name.as_bytes())[0]
}

fn field(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
}

pub fn hash_entries(entries: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"flotilla-key-version-leaf-v1");
    for (name, version) in entries {
        field(&mut hash, name);
        field(&mut hash, version);
    }
    format!("{:x}", hash.finalize())
}

#[derive(Debug, Clone)]
pub struct DigestIndex {
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

#[cfg(test)]
mod tests {
    use super::*;

    // Length-framed key/version hashing cannot confuse field boundaries.
    #[test]
    fn key_version_boundaries_are_unambiguous() {
        assert_ne!(hash_entries(&BTreeMap::from([("ab".into(), "c".into())])), hash_entries(&BTreeMap::from([("a".into(), "bc".into())])));
    }
}
