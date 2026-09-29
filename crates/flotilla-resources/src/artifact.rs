use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{resource::define_resource, NoStatusPatch, ReplicationClass, ResourceError};

define_resource!(
    Artifact,
    "artifacts",
    ArtifactSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    validate_spec_update = validate_artifact_update
);

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
