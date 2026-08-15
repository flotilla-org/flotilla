use serde::{Deserialize, Serialize};

use crate::arg::Arg;

/// One executable attach command. Every remote or environment level resolves
/// only its own next hop and replaces itself with this command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAttachPlan(pub Vec<ResolvedAttachAction>);

impl ResolvedAttachPlan {
    pub fn command(args: Vec<Arg>) -> Self {
        Self(vec![ResolvedAttachAction::Command(args)])
    }

    pub fn shell_command(command: impl Into<String>) -> Self {
        Self::command(vec![Arg::Literal("sh".into()), Arg::Literal("-lc".into()), Arg::Quoted(command.into())])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResolvedAttachAction {
    Command(Vec<Arg>),
}
