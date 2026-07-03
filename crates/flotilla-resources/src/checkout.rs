use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(Checkout, "checkouts", CheckoutSpec, CheckoutStatus, CheckoutStatusPatch);

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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutWorktreeSpec {
    pub env_ref: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub target_path: String,
    pub clone_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshCloneCheckoutSpec {
    pub env_ref: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub target_path: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCheckoutSpec {
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub path: String,
    pub repo_ref: String,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub phase: CheckoutPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutStatusPatch {
    MarkPreparing,
    MarkReady { path: String, commit: Option<String> },
    MarkTerminating,
    MarkFailed { message: String },
}

impl StatusPatch<CheckoutStatus> for CheckoutStatusPatch {
    fn apply(&self, status: &mut CheckoutStatus) {
        match self {
            Self::MarkPreparing => {
                status.phase = CheckoutPhase::Preparing;
                status.message = None;
            }
            Self::MarkReady { path, commit } => {
                status.phase = CheckoutPhase::Ready;
                status.path = Some(path.clone());
                status.commit = commit.clone();
                status.message = None;
            }
            Self::MarkTerminating => {
                status.phase = CheckoutPhase::Terminating;
            }
            Self::MarkFailed { message } => {
                status.phase = CheckoutPhase::Failed;
                status.message = Some(message.clone());
            }
        }
    }
}
