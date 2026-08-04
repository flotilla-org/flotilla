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
    /// Show ready, unblocked, undispatched issues proposed for dispatch
    Queue {
        /// Restrict the queue to one Project resource name
        #[arg(long)]
        project: Option<String>,
    },
}

impl DispatchNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let DispatchVerb::Queue { project } = self.verb;
        Ok(Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryDispatchQueue { project },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        })
    }
}

impl std::fmt::Display for DispatchNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.verb {
            DispatchVerb::Queue { project } => {
                f.write_str("dispatch queue")?;
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
    use flotilla_protocol::{Command, CommandAction};

    use super::DispatchNoun;

    #[test]
    fn queue_resolves_to_a_read_only_query() {
        let noun = DispatchNoun::try_parse_from(["dispatch", "queue", "--project", "widgets"]).expect("parse");
        let crate::Resolved::NeedsContext { command, .. } = noun.resolve().expect("resolve") else { panic!("expected context command") };
        assert_eq!(command, Command {
            node_id: None,
            provisioning_target: None,
            context_repo: None,
            action: CommandAction::QueryDispatchQueue { project: Some("widgets".to_string()) },
        });
    }
}
