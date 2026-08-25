use std::{fmt, fs, path::Path, sync::Arc};

use object_store::{path::Path as ObjectPath, ObjectStore, PutPayload};
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

impl ReviewBundleStoreConfig {
    pub fn connect(&self, credential: &ReviewBundleWriteCredential) -> Result<ReviewBundleStore, ReviewBundleStoreError> {
        let mut builder = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(&self.endpoint)
            .with_bucket_name(&self.bucket)
            .with_region(&self.region)
            .with_access_key_id(&credential.access_key_id)
            .with_secret_access_key(&credential.secret_access_key)
            .with_allow_http(self.allow_http)
            .with_virtual_hosted_style_request(self.virtual_hosted_style);
        if let Some(token) = &credential.session_token {
            builder = builder.with_token(token);
        }
        let store = builder.build().map_err(|error| ReviewBundleStoreError::Configuration(error.to_string()))?;
        Ok(ReviewBundleStore::new(Arc::new(store), &self.public_base_url))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ReviewBundleLocation {
    pub project: String,
    pub convoy: String,
    pub claim_sequence: u64,
}

impl ReviewBundleLocation {
    pub fn key_prefix(&self) -> Result<String, ReviewBundleStoreError> {
        for (field, value) in [("project", &self.project), ("convoy", &self.convoy)] {
            if value.is_empty() || value == "." || value == ".." || value.contains('/') || value.contains('\\') {
                return Err(ReviewBundleStoreError::InvalidLocation(format!("{field} `{value}` is not a safe object-key component")));
            }
        }
        if self.claim_sequence == 0 {
            return Err(ReviewBundleStoreError::InvalidLocation("claim sequence must start at 1".to_string()));
        }
        Ok(format!("{REVIEW_BUNDLE_ROOT}/{}/{}/{}/", self.project, self.convoy, self.claim_sequence))
    }
}

#[derive(Debug)]
pub enum ReviewBundleStoreError {
    Configuration(String),
    InvalidLocation(String),
    InvalidArtifact(String),
    ReadArtifact { path: String, source: std::io::Error },
    Store(object_store::Error),
    DecodeIndex(serde_json::Error),
}

impl fmt::Display for ReviewBundleStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => write!(formatter, "configure review bundle store: {message}"),
            Self::InvalidLocation(message) | Self::InvalidArtifact(message) => formatter.write_str(message),
            Self::ReadArtifact { path, source } => write!(formatter, "read review bundle artifact {path}: {source}"),
            Self::Store(error) => write!(formatter, "review bundle object store: {error}"),
            Self::DecodeIndex(error) => write!(formatter, "decode uploaded review bundle index: {error}"),
        }
    }
}

impl std::error::Error for ReviewBundleStoreError {}

/// Provider-neutral review storage. `object_store`'s S3 implementation accepts
/// custom endpoints (including rustfs), while its memory and filesystem
/// implementations exercise the identical contract in tests.
#[derive(Clone)]
pub struct ReviewBundleStore {
    objects: Arc<dyn ObjectStore>,
    public_base_url: String,
}

impl ReviewBundleStore {
    pub fn new(objects: Arc<dyn ObjectStore>, public_base_url: impl Into<String>) -> Self {
        Self { objects, public_base_url: public_base_url.into().trim_end_matches('/').to_string() }
    }

    pub async fn upload(&self, location: &ReviewBundleLocation, bundle_directory: &Path) -> Result<String, ReviewBundleStoreError> {
        let prefix = location.key_prefix()?;
        let index_path = bundle_directory.join(REVIEW_BUNDLE_INDEX_FILE);
        let index_bytes = tokio::fs::read(&index_path)
            .await
            .map_err(|source| ReviewBundleStoreError::ReadArtifact { path: index_path.display().to_string(), source })?;
        let index: ReviewBundleIndex = serde_json::from_slice(&index_bytes).map_err(ReviewBundleStoreError::DecodeIndex)?;

        self.put(&prefix, REVIEW_BUNDLE_INDEX_FILE, index_bytes).await?;
        for relative in &index.artifacts {
            validate_relative_artifact(relative)?;
            let path = bundle_directory.join(relative);
            let contents = tokio::fs::read(&path)
                .await
                .map_err(|source| ReviewBundleStoreError::ReadArtifact { path: path.display().to_string(), source })?;
            self.put(&prefix, relative, contents).await?;
        }
        Ok(format!("{}/{prefix}{REVIEW_BUNDLE_INDEX_FILE}", self.public_base_url))
    }

    async fn put(&self, prefix: &str, relative: &str, contents: Vec<u8>) -> Result<(), ReviewBundleStoreError> {
        self.objects
            .put(&ObjectPath::from(format!("{prefix}{relative}")), PutPayload::from_bytes(contents.into()))
            .await
            .map_err(ReviewBundleStoreError::Store)?;
        Ok(())
    }

    pub async fn read_index(&self, location: &ReviewBundleLocation) -> Result<ReviewBundleIndex, ReviewBundleStoreError> {
        let key = ObjectPath::from(format!("{}{REVIEW_BUNDLE_INDEX_FILE}", location.key_prefix()?));
        let bytes =
            self.objects.get(&key).await.map_err(ReviewBundleStoreError::Store)?.bytes().await.map_err(ReviewBundleStoreError::Store)?;
        serde_json::from_slice(&bytes).map_err(ReviewBundleStoreError::DecodeIndex)
    }
}

fn validate_relative_artifact(relative: &str) -> Result<(), ReviewBundleStoreError> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path.components().any(|component| !matches!(component, std::path::Component::Normal(_)))
        || relative == REVIEW_BUNDLE_INDEX_FILE
    {
        return Err(ReviewBundleStoreError::InvalidArtifact(format!("review bundle artifact `{relative}` is not a safe relative name")));
    }
    Ok(())
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

#[derive(Debug)]
pub enum ClaimAdmissibilityError {
    ReadIndex(std::io::Error),
    ReadUploadedIndex(ReviewBundleStoreError),
    DecodeIndex(serde_json::Error),
    RefPairMismatch { claimed: ReviewRefPair, bundled: ReviewRefPair },
    UnansweredFinding { round: u32, finding_id: String },
    HeadDigestMismatch { claimed: String, bundled: String },
}

impl fmt::Display for ClaimAdmissibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadIndex(error) => write!(formatter, "read review bundle index: {error}"),
            Self::ReadUploadedIndex(error) => write!(formatter, "read uploaded review bundle index: {error}"),
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

    validate_claim_index(claim, index)
}

fn validate_claim_index(claim: &SettlementClaimEvidence, index: ReviewBundleIndex) -> Result<ReviewBundleIndex, ClaimAdmissibilityError> {
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

/// Validate against the immutable index in object storage. This is the claim
/// admission path; the local-directory function remains for isolated tests.
pub async fn validate_uploaded_settlement_claim(
    claim: &SettlementClaimEvidence,
    location: &ReviewBundleLocation,
    store: &ReviewBundleStore,
) -> Result<ReviewBundleIndex, ClaimAdmissibilityError> {
    let index = store.read_index(location).await.map_err(ClaimAdmissibilityError::ReadUploadedIndex)?;
    validate_claim_index(claim, index)
}

/// Claim-time publication fence: artifacts are uploaded first, the evidence
/// URL is the uploaded well-known index, and admissibility is checked by
/// reading that object back rather than trusting the local directory.
pub async fn publish_settlement_claim(
    refs: ReviewRefPair,
    claimed_head_digest: String,
    location: &ReviewBundleLocation,
    bundle_directory: &Path,
    store: &ReviewBundleStore,
) -> Result<SettlementClaimEvidence, ClaimPublicationError> {
    let bundle_url = store.upload(location, bundle_directory).await.map_err(ClaimPublicationError::Upload)?;
    let claim = SettlementClaimEvidence { refs, bundle_url, claimed_head_digest };
    validate_uploaded_settlement_claim(&claim, location, store).await.map_err(ClaimPublicationError::Admissibility)?;
    Ok(claim)
}

#[derive(Debug)]
pub enum ClaimPublicationError {
    Upload(ReviewBundleStoreError),
    Admissibility(ClaimAdmissibilityError),
}

impl fmt::Display for ClaimPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upload(error) => write!(formatter, "upload settlement claim review bundle: {error}"),
            Self::Admissibility(error) => write!(formatter, "uploaded settlement claim is inadmissible: {error}"),
        }
    }
}

impl std::error::Error for ClaimPublicationError {}
