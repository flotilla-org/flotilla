use std::fmt;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Workspaces")]
pub struct WorkspaceNoun {
    /// Workspace reference
    pub subject: String,

    #[command(subcommand)]
    pub verb: WorkspaceVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WorkspaceVerb {
    /// Switch to a workspace
    Select,
}

impl WorkspaceNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            WorkspaceVerb::Select => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::SelectWorkspace { ws_ref: self.subject },
            })),
        }
    }
}

impl fmt::Display for WorkspaceNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "workspace {}", self.subject)?;
        match &self.verb {
            WorkspaceVerb::Select => write!(f, " select")?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::WorkspaceNoun;
    use crate::Resolved;

    fn parse(args: &[&str]) -> WorkspaceNoun {
        WorkspaceNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn workspace_select() {
        let resolved = parse(&["workspace", "feat-ws", "select"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::SelectWorkspace { ws_ref: "feat-ws".into() },
            })
        );
    }
}
