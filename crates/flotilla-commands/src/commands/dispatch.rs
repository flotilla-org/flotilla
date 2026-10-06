use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{HostResolution, RepoContext, Resolved};

#[derive(Debug, Parser)]
#[command(name = "dispatch", about = "Inspect proposed dispatch work")]
pub struct DispatchNoun {
    #[command(subcommand)]
    pub verb: DispatchVerb,
}

#[derive(Debug, Subcommand)]
pub enum DispatchVerb {
    /// Daemon-owned tracker and readiness facts for board clients
    Board {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show ready, unblocked, undispatched issues proposed for dispatch
    #[command(name = "ready", alias = "queue")]
    Queue {
        /// Restrict the queue to one Project resource name
        #[arg(long)]
        project: Option<String>,
    },
}

impl DispatchNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let action = match self.verb {
            DispatchVerb::Queue { project } => CommandAction::QueryDispatchQueue { project },
            DispatchVerb::Board { project } => CommandAction::QueryDispatchBoard { project },
        };
        Ok(Resolved::NeedsContext {
            command: Command { node_id: None, provisioning_target: None, context_repo: None, action },
            repo: RepoContext::None,
            host: HostResolution::Local,
        })
    }
}

impl std::fmt::Display for DispatchNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.verb {
            DispatchVerb::Board { project } => {
                f.write_str("dispatch board")?;
                if let Some(project) = project {
                    write!(f, " --project {}", crate::quote_value(project))?;
                }
                Ok(())
            }
            DispatchVerb::Queue { project } => {
                f.write_str("dispatch ready")?;
                if let Some(project) = project {
                    write!(f, " --project {}", crate::quote_value(project))?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::CommandAction;

    use super::DispatchNoun;

    #[test]
    fn queue_resolves_to_a_read_only_query() {
        // Glue: both spellings resolve to the same read-only daemon command.
        for verb in ["ready", "queue"] {
            let noun = DispatchNoun::try_parse_from(["dispatch", verb, "--project", "widgets"]).expect("parse");
            let crate::Resolved::NeedsContext { command, .. } = noun.resolve().expect("resolve") else {
                panic!("expected context command")
            };
            assert_eq!(command.action, CommandAction::QueryDispatchQueue { project: Some("widgets".to_string()) });
        }
    }
}
