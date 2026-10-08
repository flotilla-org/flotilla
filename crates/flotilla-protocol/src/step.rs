use serde::{Deserialize, Serialize};

use crate::{path_context::ExecutionEnvironmentPath, CommandValue, NodeId};

/// Whether a checkout command targets an existing branch or creates a fresh one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutIntent {
    ExistingBranch,
    FreshBranch,
}

/// Execution context for a step: which daemon (transport) and which provider scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepExecutionContext {
    /// Run on a host daemon using the host's own providers.
    Host(NodeId),
    /// Run on a host daemon but resolve against an environment's providers.
    Environment(NodeId, crate::EnvironmentId),
}

impl StepExecutionContext {
    /// The daemon host that will execute this step (determines transport routing).
    pub fn node_id(&self) -> &NodeId {
        match self {
            Self::Host(h) | Self::Environment(h, _) => h,
        }
    }
}

/// Outcome of a single step execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub enum StepOutcome {
    Completed,
    CompletedWith(CommandValue),
    Produced(CommandValue),
    Skipped,
}

/// A symbolic action that the step runner resolves at execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepAction {
    // Checkout lifecycle
    CreateCheckout {
        branch: String,
        create_branch: bool,
        intent: CheckoutIntent,
        issue_ids: Vec<(String, String)>,
    },
    LinkIssuesToBranch {
        branch: String,
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        branch: String,
    },

    // Session
    ArchiveSession {
        session_id: String,
    },
    GenerateBranchName {
        issue_keys: Vec<String>,
    },

    // Query
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<ExecutionEnvironmentPath>,
        change_request_id: Option<String>,
    },

    // External interactions
    OpenChangeRequest {
        id: String,
    },
    CloseChangeRequest {
        id: String,
    },
    MergeChangeRequest {
        id: String,
    },
    OpenIssue {
        id: String,
    },
    LinkIssuesToChangeRequest {
        change_request_id: String,
        issue_ids: Vec<String>,
    },

    /// No-op action — resolvers return `Completed` without side effects.
    Noop,

    // Environment lifecycle
    CreateEnvironment {
        env_id: crate::EnvironmentId,
        /// The environment provider to use (e.g. "docker").
        provider: String,
    },
    DestroyEnvironment {
        env_id: crate::EnvironmentId,
    },
    /// Read `.flotilla/environment.yaml` from the repo root known to the step resolver at runtime.
    ReadEnvironmentSpec,
}

/// A single step in a multi-step command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub description: String,
    pub host: StepExecutionContext,
    pub action: StepAction,
}
