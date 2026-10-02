use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Code review", visible_alias = "pr")]
#[command(subcommand_precedence_over_arg = true)]
pub struct CrNoun {
    /// Change request ID
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<CrVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CrVerb {
    /// Open change request in browser
    Open,
    /// Close a change request
    Close,
    /// Merge a change request using the repository's squash convention
    Merge {
        /// Confirm the merge without prompting
        #[arg(long)]
        yes: bool,
    },
    /// Link issues to a change request
    LinkIssues { issue_ids: Vec<String> },
}

impl CrNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (None, None) => Ok(Resolved::Ready(Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Cr },
            })),
            (Some(subject), Some(CrVerb::Open)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::OpenChangeRequest { id: subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (Some(subject), Some(CrVerb::Close)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::CloseChangeRequest { id: subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (Some(subject), Some(CrVerb::Merge { yes })) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::MergeChangeRequest { id: subject, confirmed: yes },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (Some(subject), Some(CrVerb::LinkIssues { issue_ids })) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::LinkIssuesToChangeRequest { change_request_id: subject, issue_ids },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (None, Some(_)) => Err("change request command requires a subject".into()),
            (Some(_), None) => Err("missing cr verb".into()),
        }
    }
}

impl std::fmt::Display for CrNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cr")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        match &self.verb {
            Some(CrVerb::Open) => write!(f, " open")?,
            Some(CrVerb::Close) => write!(f, " close")?,
            Some(CrVerb::Merge { yes }) => {
                write!(f, " merge")?;
                if *yes {
                    write!(f, " --yes")?;
                }
            }
            Some(CrVerb::LinkIssues { issue_ids }) => {
                write!(f, " link-issues")?;
                for id in issue_ids {
                    write!(f, " {id}")?;
                }
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::CommandAction;

    use super::CrNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
    };

    fn parse(args: &[&str]) -> CrNoun {
        CrNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn cr_without_verb_lists_open_change_requests() {
        let resolved = parse(&["cr"]).resolve().expect("default list");
        crate::test_utils::assert_ready(resolved, CommandAction::QueryCliList { kind: flotilla_protocol::CliListKind::Cr });
    }

    #[test]
    fn cr_open() {
        let resolved = parse(&["cr", "42", "open"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::OpenChangeRequest { id: "42".into() },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn cr_close() {
        let resolved = parse(&["cr", "42", "close"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::CloseChangeRequest { id: "42".into() },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn cr_merge_requires_surface_confirmation() {
        let resolved = parse(&["cr", "42", "merge"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::MergeChangeRequest { id: "42".into(), confirmed: false },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn cr_merge_yes_records_explicit_confirmation() {
        let resolved = parse(&["cr", "42", "merge", "--yes"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::MergeChangeRequest { id: "42".into(), confirmed: true },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn cr_link_issues() {
        let resolved = parse(&["cr", "42", "link-issues", "1", "5", "7"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::LinkIssuesToChangeRequest {
                change_request_id: "42".into(),
                issue_ids: vec!["1".into(), "5".into(), "7".into()],
            },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
    }

    #[test]
    fn pr_alias_open() {
        // The `pr` alias is registered at the CLI top level, not on the parser itself,
        // so we test the struct directly with the same args.
        let resolved = parse(&["pr", "42", "open"]).resolve().unwrap();
        crate::test_utils::assert_needs_context(
            resolved,
            CommandAction::OpenChangeRequest { id: "42".into() },
            RepoContext::Inferred,
            HostResolution::ProviderHost,
        );
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
    fn round_trip_merge() {
        assert_round_trip::<CrNoun>(&["cr", "42", "merge"]);
        assert_round_trip::<CrNoun>(&["cr", "42", "merge", "--yes"]);
    }

    #[test]
    fn round_trip_link_issues() {
        assert_round_trip::<CrNoun>(&["cr", "42", "link-issues", "1", "5", "7"]);
    }
}
