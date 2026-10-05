use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ApiPaths, ReplicationClass, Resource, ResourceBackend, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact;

impl Resource for Artifact {
    type Spec = ArtifactSpec;
    type Status = ArtifactStatus;
    type StatusPatch = ArtifactStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "artifacts", kind: "Artifact" };
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec_update(current: &ArtifactSpec, requested: &ArtifactSpec) -> Result<(), ResourceError> {
        validate_artifact_update(current, requested)
    }

    fn validate_status_update(current: Option<&ArtifactStatus>, requested: &ArtifactStatus) -> Result<(), ResourceError> {
        if current.is_some_and(|status| {
            status.ledger_comment_creations.iter().any(|address| !requested.ledger_comment_creations.contains(address))
        }) {
            return Err(ResourceError::invalid("ledger comment creation reservations cannot be removed"));
        }
        Ok(())
    }
}

/// Durable, authority-owned reservations for non-idempotent forge creation.
/// Old artifacts stored a null status, which still decodes as no status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactStatus {
    // Accept empty statuses during the transition from unit status; remove
    // this default after the next fleet roll.
    #[serde(default)]
    pub ledger_comment_creations: Vec<LeafAddress>,
}

pub enum ArtifactStatusPatch {}

impl StatusPatch<ArtifactStatus> for ArtifactStatusPatch {
    fn apply(&self, _: &mut ArtifactStatus) {
        match *self {}
    }
}

/// Only the winner of the authority's compare-and-swap may attempt POST.
/// Reservations never expire: an absent comment cannot distinguish a refused
/// POST from an accepted request still in flight. Retry by looking up the marker,
/// never by granting another creation attempt. Revisions use PATCH instead.
pub async fn reserve_ledger_comment_creation(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    address: &LeafAddress,
) -> Result<bool, ResourceError> {
    if !matches!(address, LeafAddress::ChangeRequest { .. }) {
        return Err(ResourceError::invalid("ledger creation requires a change request"));
    }
    let resolver = backend.using::<Artifact>(namespace);
    for _ in 0..16 {
        let object = resolver.get(name).await?;
        if object.spec.kind != "decision-ledger" {
            return Err(ResourceError::invalid("ledger creation requires a decision-ledger artifact"));
        }
        let mut status = object.status.unwrap_or_default();
        if status.ledger_comment_creations.contains(address) {
            return Ok(false);
        }
        status.ledger_comment_creations.push(address.clone());
        match resolver.update_status(name, &object.metadata.resource_version, &status).await {
            Ok(_) => return Ok(true),
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ResourceError::conflict(name, "ledger creation reservation contention"))
}

fn validate_artifact_update(current: &ArtifactSpec, requested: &ArtifactSpec) -> Result<(), ResourceError> {
    if (current.convoy.as_str(), current.producer.as_str(), current.kind.as_str(), current.subject.as_str())
        != (requested.convoy.as_str(), requested.producer.as_str(), requested.kind.as_str(), requested.subject.as_str())
    {
        return Err(ResourceError::invalid("artifact address cannot change"));
    }
    if current.pinned && !requested.pinned {
        return Err(ResourceError::invalid("pinned artifact cannot be unpinned by a put"));
    }
    Ok(())
}

/// A small, replicated description of a body held by a BlobStore.
/// The address is stable; replacing it publishes the latest body for the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ArtifactSpec {
    pub convoy: String,
    pub producer: String,
    pub kind: String,
    pub subject: String,
    #[builder(default)]
    #[serde(default)]
    pub summary: BTreeMap<String, serde_json::Value>,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    // Decode pre-recorded_at artifacts for one generation. Remove the default
    // after the next fleet roll verifies those stored artifacts are gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    #[builder(default)]
    #[serde(default)]
    pub pinned: bool,
}

/// Hash the complete key so punctuation and long refs cannot escape resource
/// naming rules or collide after truncation.
pub fn artifact_record_name(convoy: &str, producer: &str, kind: &str, subject: &str) -> String {
    let mut hash = Sha256::new();
    for component in [convoy, producer, kind, subject] {
        hash.update((component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    format!("artifact-{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_generation_artifact_without_recorded_at_decodes() {
        let old = serde_json::json!({
            "convoy": "demo", "producer": "coder", "kind": "decision-ledger", "subject": "demo",
            "summary": {}, "digest": "digest", "size": 1, "media_type": "text/markdown",
            "expires_at": "2026-09-28T00:00:00Z"
        });
        let decoded: ArtifactSpec = serde_json::from_value(old).expect("previous artifact spec");
        assert!(decoded.recorded_at.is_none());
    }
}
