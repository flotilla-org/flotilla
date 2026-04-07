use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Code review", visible_alias = "pr")]
pub struct CrNoun {
    /// Change request ID
    pub subject: String,

    #[command(subcommand)]
    pub verb: CrVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CrVerb {
    /// Open change request in browser
    Open,
    /// Close a change request
    Close,
    /// Link issues to a change request
    LinkIssues { issue_ids: Vec<String> },
}

impl CrNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            CrVerb::Open => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::OpenChangeRequest { id: self.subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            CrVerb::Close => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::CloseChangeRequest { id: self.subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            CrVerb::LinkIssues { issue_ids } => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::LinkIssuesToChangeRequest { change_request_id: self.subject, issue_ids },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
        }
    }
}

impl std::fmt::Display for CrNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cr {}", self.subject)?;
        match &self.verb {
            CrVerb::Open => write!(f, " open")?,
            CrVerb::Close => write!(f, " close")?,
            CrVerb::LinkIssues { issue_ids } => {
                write!(f, " link-issues")?;
                for id in issue_ids {
                    write!(f, " {id}")?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::CrNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> CrNoun {
        CrNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn cr_open() {
        let resolved = parse(&["cr", "42", "open"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::OpenChangeRequest { id: "42".into() }
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProviderHost,
        });
    }

    #[test]
    fn cr_close() {
        let resolved = parse(&["cr", "42", "close"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::CloseChangeRequest { id: "42".into() }
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProviderHost,
        });
    }

    #[test]
    fn cr_link_issues() {
        let resolved = parse(&["cr", "42", "link-issues", "1", "5", "7"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::LinkIssuesToChangeRequest {
                    change_request_id: "42".into(),
                    issue_ids: vec!["1".into(), "5".into(), "7".into()],
                },
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProviderHost,
        });
    }

    #[test]
    fn pr_alias_open() {
        // The `pr` alias is registered at the CLI top level, not on the parser itself,
        // so we test the struct directly with the same args.
        let resolved = parse(&["pr", "42", "open"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::OpenChangeRequest { id: "42".into() }
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProviderHost,
        });
    }

    #[test]
    fn round_trip_open() {
        assert_round_trip::<CrNoun>(&["cr", "42", "open"]);
    }

    #[test]
    fn round_trip_close() {
        assert_round_trip::<CrNoun>(&["cr", "42", "close"]);
    }

    #[test]
    fn round_trip_link_issues() {
        assert_round_trip::<CrNoun>(&["cr", "42", "link-issues", "1", "5", "7"]);
    }
}
