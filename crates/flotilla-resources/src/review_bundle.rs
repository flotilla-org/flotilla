use serde::{Deserialize, Serialize};

pub const REVIEW_BUNDLE_INDEX_FILE: &str = "index.json";
pub const REVIEW_BUNDLE_ROOT: &str = "reviews";

/// Installation-wide S3-compatible endpoint configuration. Credentials stay
/// separate so this value is safe to persist in daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewBundleStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub public_base_url: String,
    #[builder(default)]
    #[serde(default)]
    pub allow_http: bool,
    /// Path-style requests are the interoperable default for custom endpoints.
    #[builder(default)]
    #[serde(default)]
    pub virtual_hosted_style: bool,
}

/// Contents of the scoped credential file staged into a vessel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewBundleWriteCredential {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

/// The immutable pair of refs reviewed by a settlement claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewRefPair {
    pub base: String,
    pub head: String,
}

/// Evidence attached to a settlement claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct SettlementClaimEvidence {
    pub refs: ReviewRefPair,
    pub bundle_url: String,
    pub claimed_head_digest: String,
}

/// Machine-readable entry point for a review bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewBundleIndex {
    pub refs: ReviewRefPair,
    pub head_digest: String,
    pub rounds: Vec<ReviewRound>,
    pub checks: Vec<ReviewCheck>,
    /// Human-facing files, relative to the bundle directory.
    pub artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewRound {
    pub number: u32,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewFinding {
    pub id: String,
    pub summary: String,
    pub resolution: FindingResolution,
}

/// A finding is either unanswered, fixed, or explicitly rejected with the
/// coder's rationale. The tagged representation prevents other terminal
/// states from entering the bundle protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum FindingResolution {
    Open,
    Addressed { fix_reference: String },
    RejectedWithRationale { rationale: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewCheck {
    pub name: String,
    pub outcome: ReviewCheckOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewCheckOutcome {
    Passed,
    Failed,
}
