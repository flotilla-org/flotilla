use serde::{Deserialize, Serialize};

use crate::{
    path_context::ExecutionEnvironmentPath, AttachableSetId, CommandValue, HostName, HostPath, PreparedTerminalCommand, ResolvedPaneCommand,
};

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
    Host(HostName),
    /// Run on a host daemon but resolve against an environment's providers.
    Environment(HostName, crate::EnvironmentId),
}

impl StepExecutionContext {
    /// The daemon host that will execute this step (determines transport routing).
    pub fn host_name(&self) -> &HostName {
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
        deleted_checkout_paths: Vec<HostPath>,
    },

    // Teleport
    ResolveAttachCommand {
        session_id: String,
    },
    EnsureCheckoutForTeleport {
        branch: Option<String>,
        checkout_key: Option<ExecutionEnvironmentPath>,
        initial_path: Option<ExecutionEnvironmentPath>,
    },
    CreateTeleportWorkspace {
        session_id: String,
        branch: Option<String>,
    },

    // Session
    ArchiveSession {
        session_id: String,
    },
    GenerateBranchName {
        issue_keys: Vec<String>,
    },

    // Workspace lifecycle (new)
    CreateWorkspaceFromPreparedTerminal {
        target_host: HostName,
        branch: String,
        checkout_path: ExecutionEnvironmentPath,
        attachable_set_id: Option<AttachableSetId>,
        commands: Vec<ResolvedPaneCommand>,
    },
    PrepareWorkspace {
        checkout_path: Option<ExecutionEnvironmentPath>,
        label: String,
    },
    AttachWorkspace,
    SelectWorkspace {
        ws_ref: String,
    },
    PrepareTerminalForCheckout {
        checkout_path: ExecutionEnvironmentPath,
        commands: Vec<PreparedTerminalCommand>,
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
    EnsureEnvironmentImage {
        /// The environment provider to use (e.g. "docker").
        provider: String,
    },
    CreateEnvironment {
        env_id: crate::EnvironmentId,
        /// The environment provider to use (e.g. "docker").
        provider: String,
        /// `None` means resolve from prior `EnsureEnvironmentImage` outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<crate::ImageId>,
    },
    DiscoverEnvironmentProviders {
        env_id: crate::EnvironmentId,
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
