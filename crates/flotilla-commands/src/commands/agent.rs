use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Cloud agents")]
pub struct AgentNoun {
    /// Agent/session ID
    pub subject: String,

    #[command(subcommand)]
    pub verb: AgentVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AgentVerb {
    /// Connect to a remote agent session
    Teleport {
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Archive an agent session
    Archive,
}

impl AgentNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            AgentVerb::Teleport { branch, checkout } => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TeleportSession { session_id: self.subject, branch, checkout_key: checkout },
            })),
            AgentVerb::Archive => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::ArchiveSession { session_id: self.subject },
            })),
        }
    }
}

impl fmt::Display for AgentNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agent {}", self.subject)?;
        match &self.verb {
            AgentVerb::Teleport { branch, checkout } => {
                write!(f, " teleport")?;
                if let Some(b) = branch {
                    write!(f, " --branch {b}")?;
                }
                if let Some(c) = checkout {
                    write!(f, " --checkout {}", c.display())?;
                }
            }
            AgentVerb::Archive => write!(f, " archive")?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::AgentNoun;
    use crate::Resolved;

    fn parse(args: &[&str]) -> AgentNoun {
        AgentNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn agent_teleport_no_flags() {
        let resolved = parse(&["agent", "claude-1", "teleport"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TeleportSession { session_id: "claude-1".into(), branch: None, checkout_key: None },
            })
        );
    }

    #[test]
    fn agent_teleport_with_branch() {
        let resolved = parse(&["agent", "claude-1", "teleport", "--branch", "feat"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TeleportSession { session_id: "claude-1".into(), branch: Some("feat".into()), checkout_key: None },
            })
        );
    }

    #[test]
    fn agent_teleport_with_branch_and_checkout() {
        let resolved = parse(&["agent", "claude-1", "teleport", "--branch", "feat", "--checkout", "/tmp/wt"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TeleportSession {
                    session_id: "claude-1".into(),
                    branch: Some("feat".into()),
                    checkout_key: Some(PathBuf::from("/tmp/wt")),
                },
            })
        );
    }

    #[test]
    fn agent_archive() {
        let resolved = parse(&["agent", "claude-1", "archive"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::ArchiveSession { session_id: "claude-1".into() },
            })
        );
    }
}
