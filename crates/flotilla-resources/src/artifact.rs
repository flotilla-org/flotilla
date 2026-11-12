use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use flotilla_protocol::LeafAddress;
use serde::{Deserialize, Serialize};

use crate::{ApiPaths, ReplicationClass, Resource, ResourceError, StatusPatch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact;

impl Resource for Artifact {
    type Spec = ArtifactSpec;
    type Status = ArtifactStatus;
    type StatusPatch = ArtifactStatusPatch;

    const API_PATHS: ApiPaths = ApiPaths { group: "flotilla.work", version: "v1", plural: "artifacts", kind: "Artifact" };
    const QUERYABLE_FIELDS: &'static [&'static str] = &["metadata.name", "spec.convoy"];
    const REPLICATION_CLASS: ReplicationClass = ReplicationClass::HomeBoundRuntime;

    fn validate_spec(_meta: &crate::InputMeta, spec: &ArtifactSpec) -> Result<(), ResourceError> {
        if !is_blob_digest(&spec.digest) {
            return Err(ResourceError::invalid(format!("artifact digest `{}` is not {BLOB_DIGEST_RULE}", spec.digest)));
        }
        Ok(())
    }

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

/// What a blob digest is, as a refusal names it.
pub const BLOB_DIGEST_RULE: &str = "64 lowercase SHA-256 hex digits";

/// The digest naming blob `bytes` by its content.
pub fn blob_digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Whether `value` names a blob by its content: [`BLOB_DIGEST_RULE`].
pub fn is_blob_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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

    #[test]
    fn an_artifact_names_its_body_by_a_blob_digest() {
        let spec = |digest: &str| {
            ArtifactSpec::builder()
                .convoy("demo".into())
                .producer("coder".into())
                .kind("decision-ledger".into())
                .subject("demo".into())
                .digest(digest.into())
                .size(1)
                .media_type("text/markdown".into())
                .expires_at(Utc::now())
                .build()
        };
        let meta = crate::InputMeta::builder().name("ledger".into()).build();
        Artifact::validate_spec(&meta, &spec(&"a1".repeat(32))).expect("a SHA-256 hex digest");
        for refused in ["dummy-script-finished", &"A1".repeat(32), &"a1".repeat(31), ""] {
            assert!(Artifact::validate_spec(&meta, &spec(refused)).is_err(), "{refused:?}");
        }
    }
}
