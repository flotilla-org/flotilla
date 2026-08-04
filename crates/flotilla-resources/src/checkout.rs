use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch, ReplicationClass, RepositoryCheckoutKind, RepositoryKey};

define_resource!(
    Checkout,
    "checkouts",
    CheckoutSpec,
    CheckoutStatus,
    CheckoutStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckoutSpec {
    Worktree(CheckoutWorktreeSpec),
    FreshClone(FreshCloneCheckoutSpec),
    Observed(ObservedCheckoutSpec),
}

impl CheckoutSpec {
    pub fn env_ref(&self) -> Option<&str> {
        match self {
            Self::Worktree(spec) => Some(&spec.env_ref),
            Self::FreshClone(spec) => Some(&spec.env_ref),
            Self::Observed(_) => None,
        }
    }

    pub fn target_path(&self) -> Option<&str> {
        match self {
            Self::Worktree(spec) => Some(&spec.target_path),
            Self::FreshClone(spec) => Some(&spec.target_path),
            Self::Observed(_) => None,
        }
    }

    pub fn repo_ref(&self) -> &RepositoryKey {
        match self {
            Self::Worktree(spec) => &spec.repo_ref,
            Self::FreshClone(spec) => &spec.repo_ref,
            Self::Observed(spec) => &spec.repo_ref,
        }
    }

    pub fn repository_checkout_kind(&self) -> RepositoryCheckoutKind {
        match self {
            Self::Worktree(_) => RepositoryCheckoutKind::Worktree,
            Self::FreshClone(_) => RepositoryCheckoutKind::FreshClone,
            Self::Observed(_) => RepositoryCheckoutKind::Observed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CheckoutWorktreeSpec {
    pub repo_ref: RepositoryKey,
    pub env_ref: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    pub target_path: String,
    pub clone_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct FreshCloneCheckoutSpec {
    pub repo_ref: RepositoryKey,
    pub env_ref: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    pub target_path: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ObservedCheckoutSpec {
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub path: String,
    pub repo_ref: RepositoryKey,
    pub host_ref: String,
    pub is_main: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutPhase {
    #[default]
    Pending,
    Preparing,
    Ready,
    Terminating,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutBranchProvenance {
    #[default]
    PreExisting,
    CreatedForConvoy,
}

impl CheckoutBranchProvenance {
    fn is_pre_existing(&self) -> bool {
        *self == Self::PreExisting
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CheckoutStatus {
    pub phase: CheckoutPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "CheckoutBranchProvenance::is_pre_existing")]
    #[builder(default)]
    pub branch_provenance: CheckoutBranchProvenance,
    #[builder(default)]
    #[serde(default)]
    pub integration: CheckoutIntegrationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CheckoutIntegrationStatus {
    #[builder(default)]
    #[serde(default)]
    pub clean: IntegrationCondition,
    #[builder(default)]
    #[serde(default)]
    pub pushed: IntegrationCondition,
    #[builder(default)]
    #[serde(default)]
    pub landed: IntegrationCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_evidence: Option<LandedEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<ChangeRequestObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct IntegrationCondition {
    pub value: ConditionValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub details: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

impl Default for IntegrationCondition {
    fn default() -> Self {
        Self { value: ConditionValue::Unknown, details: Vec::new(), observed_at: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionValue {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct LandedEvidence {
    pub change_request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ChangeRequestObservation {
    pub id: String,
    pub state: ChangeRequestState,
    pub mergeability: ChangeRequestMergeability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeRequestMergeability {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutStatusPatch {
    MarkPreparing,
    MarkReady { path: String, commit: Option<String>, branch_provenance: CheckoutBranchProvenance },
    MarkTerminating,
    MarkFailed { message: String },
    UpdateIntegration { integration: Box<CheckoutIntegrationStatus> },
}

pub fn latch_evidence_backed_integration(existing: &CheckoutIntegrationStatus, observed: &mut CheckoutIntegrationStatus) {
    if existing.landed.value != ConditionValue::True
        || existing.landed_evidence.is_none()
        || observed.landed.value != ConditionValue::Unknown
    {
        return;
    }
    observed.landed.value = ConditionValue::True;
    if observed.landed_evidence.is_none() {
        observed.landed_evidence = existing.landed_evidence.clone();
    }
    if observed.change_request.is_none() {
        observed.change_request = existing.change_request.clone().filter(|change_request| change_request.state != ChangeRequestState::Open);
    }
}

impl StatusPatch<CheckoutStatus> for CheckoutStatusPatch {
    fn apply(&self, status: &mut CheckoutStatus) {
        match self {
            Self::MarkPreparing => {
                status.phase = CheckoutPhase::Preparing;
                status.message = None;
            }
            Self::MarkReady { path, commit, branch_provenance } => {
                status.phase = CheckoutPhase::Ready;
                status.path = Some(path.clone());
                status.commit = commit.clone();
                status.branch_provenance = *branch_provenance;
                status.message = None;
            }
            Self::MarkTerminating => {
                status.phase = CheckoutPhase::Terminating;
            }
            Self::MarkFailed { message } => {
                status.phase = CheckoutPhase::Failed;
                status.message = Some(message.clone());
            }
            Self::UpdateIntegration { integration } => {
                // The shared patch is the authority-side latch point for every
                // integration writer: only Unknown yields to prior evidence.
                let mut observed = integration.as_ref().clone();
                latch_evidence_backed_integration(&status.integration, &mut observed);
                status.integration = observed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn condition(value: ConditionValue) -> IntegrationCondition {
        IntegrationCondition::builder().value(value).observed_at("2026-07-27T00:00:00Z".to_string()).build()
    }

    fn integration(landed: ConditionValue, landed_evidence: Option<LandedEvidence>) -> CheckoutIntegrationStatus {
        CheckoutIntegrationStatus {
            clean: condition(ConditionValue::True),
            pushed: condition(ConditionValue::True),
            landed: condition(landed),
            landed_evidence,
            change_request: None,
        }
    }

    #[test]
    fn vacuous_landed_true_yields_to_fresh_false_observation() {
        // A True recorded with no landed evidence ("nothing to land yet" on an
        // untouched branch) is not a landing fact; a later observation that
        // finds an open change request must lower it (#1163).
        let mut status = CheckoutStatus { integration: integration(ConditionValue::True, None), ..Default::default() };
        CheckoutStatusPatch::UpdateIntegration { integration: Box::new(integration(ConditionValue::False, None)) }.apply(&mut status);
        assert_eq!(status.integration.landed.value, ConditionValue::False);
    }

    #[test]
    fn evidence_backed_landed_true_yields_to_fresh_false_observation() {
        let evidence = LandedEvidence::builder().change_request_id("1162".to_string()).build();
        let mut status = CheckoutStatus { integration: integration(ConditionValue::True, Some(evidence)), ..Default::default() };
        CheckoutStatusPatch::UpdateIntegration { integration: Box::new(integration(ConditionValue::False, None)) }.apply(&mut status);
        assert_eq!(status.integration.landed.value, ConditionValue::False);
        assert_eq!(status.integration.landed_evidence, None);
    }
}
