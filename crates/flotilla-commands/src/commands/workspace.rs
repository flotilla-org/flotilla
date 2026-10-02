use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Workspaces", subcommand_precedence_over_arg = true)]
pub struct WorkspaceNoun {
    /// Workspace reference
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<WorkspaceVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WorkspaceVerb {
    /// Switch to a workspace
    Select,
}

impl WorkspaceNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (None, None) => Ok(Resolved::Ready(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Workspace },
            })),
            (Some(subject), Some(WorkspaceVerb::Select)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::SelectWorkspace { ws_ref: subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::Local,
            }),
            (None, Some(_)) => Err("workspace command requires a subject".into()),
            (Some(_), None) => Err("missing workspace verb".into()),
        }
    }
}

impl std::fmt::Display for WorkspaceNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workspace")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        match &self.verb {
            Some(WorkspaceVerb::Select) => write!(f, " select")?,
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::CommandAction;

    use super::WorkspaceNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
    };

    fn parse(args: &[&str]) -> WorkspaceNoun {
        WorkspaceNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn workspace_without_verb_lists_active_workspaces() {
        let resolved = parse(&["workspace"]).resolve().expect("default list");
        crate::test_utils::assert_ready(resolved, CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Workspace });
    }

    #[test]
    fn workspace_select() {
        let resolved = parse(&["workspace", "feat-ws", "select"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::SelectWorkspace { ws_ref: "feat-ws".into() },
            RepoContext::Inferred,
            HostResolution::Local,
        );
    }

    #[test]
    fn round_trip_select() {
        assert_round_trip::<WorkspaceNoun>(&["workspace", "feat-ws", "select"]);
    }
}
