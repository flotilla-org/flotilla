use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Cloud agents", subcommand_precedence_over_arg = true)]
pub struct AgentNoun {
    /// Agent/session ID
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<AgentVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AgentVerb {
    /// Archive an agent session
    Archive,
}

impl AgentNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (None, None) => Ok(Resolved::Ready(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Agent },
            })),
            (Some(subject), Some(AgentVerb::Archive)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::ArchiveSession { session_id: subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (None, Some(_)) => Err("agent command requires a session subject".into()),
            (Some(_), None) => Err("missing agent verb".into()),
        }
    }
}

impl std::fmt::Display for AgentNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        match &self.verb {
            Some(AgentVerb::Archive) => write!(f, " archive")?,
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use clap::Parser;
    use flotilla_protocol::CommandAction;

    use super::AgentNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
    };

    fn parse(args: &[&str]) -> AgentNoun {
        AgentNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn agent_without_verb_lists_active_sessions() {
        let resolved = parse(&["agent"]).resolve().expect("default list");
        crate::test_utils::assert_ready(resolved, CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Agent });
    }

    #[test]
    fn agent_archive() {
        let resolved = parse(&["agent", "claude-1", "archive"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::ArchiveSession { session_id: "claude-1".into() },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn round_trip_archive() {
        assert_round_trip::<AgentNoun>(&["agent", "claude-1", "archive"]);
    }
}
