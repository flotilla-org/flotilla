use std::{fmt, fs, path::Path};

use serde::{Deserialize, Serialize};

pub const REVIEW_BUNDLE_INDEX_FILE: &str = "index.json";

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

#[derive(Debug)]
pub enum ClaimAdmissibilityError {
    ReadIndex(std::io::Error),
    DecodeIndex(serde_json::Error),
    RefPairMismatch { claimed: ReviewRefPair, bundled: ReviewRefPair },
    UnansweredFinding { round: u32, finding_id: String },
    HeadDigestMismatch { claimed: String, bundled: String },
}

impl fmt::Display for ClaimAdmissibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadIndex(error) => write!(formatter, "read review bundle index: {error}"),
            Self::DecodeIndex(error) => write!(formatter, "decode review bundle index: {error}"),
            Self::RefPairMismatch { claimed, bundled } => write!(
                formatter,
                "claim ref pair {}..{} does not match bundle ref pair {}..{}",
                claimed.base, claimed.head, bundled.base, bundled.head
            ),
            Self::UnansweredFinding { round, finding_id } => {
                write!(formatter, "review round {round} finding {finding_id} is unanswered")
            }
            Self::HeadDigestMismatch { claimed, bundled } => {
                write!(formatter, "claimed head digest {claimed} does not match bundle digest {bundled}")
            }
        }
    }
}

impl std::error::Error for ClaimAdmissibilityError {}

/// Validate a claim against a review bundle already present on local disk.
pub fn validate_settlement_claim(
    claim: &SettlementClaimEvidence,
    bundle_directory: &Path,
) -> Result<ReviewBundleIndex, ClaimAdmissibilityError> {
    let contents = fs::read(bundle_directory.join(REVIEW_BUNDLE_INDEX_FILE)).map_err(ClaimAdmissibilityError::ReadIndex)?;
    let index: ReviewBundleIndex = serde_json::from_slice(&contents).map_err(ClaimAdmissibilityError::DecodeIndex)?;

    if claim.refs != index.refs {
        return Err(ClaimAdmissibilityError::RefPairMismatch { claimed: claim.refs.clone(), bundled: index.refs.clone() });
    }

    for round in &index.rounds {
        if let Some(finding) = round.findings.iter().find(|finding| matches!(finding.resolution, FindingResolution::Open)) {
            return Err(ClaimAdmissibilityError::UnansweredFinding { round: round.number, finding_id: finding.id.clone() });
        }
    }

    if claim.claimed_head_digest != index.head_digest {
        return Err(ClaimAdmissibilityError::HeadDigestMismatch {
            claimed: claim.claimed_head_digest.clone(),
            bundled: index.head_digest.clone(),
        });
    }

    Ok(index)
}
