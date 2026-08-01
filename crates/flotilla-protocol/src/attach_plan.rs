use serde::{Deserialize, Serialize};

use crate::arg::Arg;

/// Replaced with a unique value by the attach-plan executor. Keeping the
/// placeholder in resolved plans makes resolution deterministic and lets all
/// actions in a plan share one lifecycle lease.
pub const ATTACH_LEASE_PLACEHOLDER: &str = "__FLOTILLA_ATTACH_LEASE__";

/// An interactive attach plan in execution-stack order.
///
/// Consumers pop actions from the end: run the outer command first, then wait
/// for and type each successively inner command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAttachPlan(pub Vec<ResolvedAttachAction>);

impl ResolvedAttachPlan {
    pub fn command(args: Vec<Arg>) -> Self {
        Self(vec![ResolvedAttachAction::Command(args)])
    }

    pub fn shell_command(command: impl Into<String>) -> Self {
        Self::command(vec![Arg::Literal(command.into())])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResolvedAttachAction {
    Command(Vec<Arg>),
    /// A command that must run when the attach owner exits, including when it
    /// dies without an opportunity to unwind.
    Cleanup(Vec<Arg>),
    /// Steps are also in stack order and are popped from the end.
    SendKeys {
        hop: String,
        steps: Vec<SendKeyStep>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SendKeyStep {
    WaitForReady,
    Type { text: String },
}
