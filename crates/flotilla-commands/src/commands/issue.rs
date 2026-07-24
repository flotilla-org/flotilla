use clap::{Parser, Subcommand};
use flotilla_protocol::{issue_query::IssueQuery, Command, CommandAction, RepoSelector};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved, SubjectArgs,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Issues")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct IssueNoun {
    #[command(flatten)]
    pub subjects: SubjectArgs,

    #[command(subcommand)]
    pub verb: Option<IssueVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum IssueVerb {
    /// Open issue in browser
    Open,
    /// Generate branch name from issues
    SuggestBranch,
    /// Search issues
    Search { query: Vec<String> },
}

impl IssueNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        let subject = self.subjects.resolve()?.map(|subject| subject.value);
        match (subject, self.verb) {
            (Some(subject), Some(IssueVerb::Open)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::OpenIssue { id: subject },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::ProviderHost,
            }),
            (None, Some(IssueVerb::Open)) => Err("open requires an issue subject".into()),
            (Some(subject), Some(IssueVerb::SuggestBranch)) => {
                let issue_keys = subject.split(',').map(|s| s.trim().to_string()).collect();
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::GenerateBranchName { issue_keys },
                    },
                    repo: RepoContext::Inferred,
                    host: HostResolution::ProvisioningTarget,
                })
            }
            (None, Some(IssueVerb::SuggestBranch)) => Err("suggest-branch requires an issue subject".into()),
            (_, Some(IssueVerb::Search { query })) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    // SENTINEL: repo is empty — dispatch must fill it from --repo or FLOTILLA_REPO.
                    action: CommandAction::QueryIssues {
                        repo: RepoSelector::Query("".into()),
                        params: IssueQuery { search: Some(query.join(" ")), label: None },
                        page: 1,
                        count: 50,
                    },
                },
                repo: RepoContext::Required,
                host: HostResolution::Local,
            }),
            (_, None) => Err("missing issue verb".into()),
        }
    }
}

impl std::fmt::Display for IssueNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "issue")?;
        self.subjects.write(f)?;
        if let Some(verb) = &self.verb {
            match verb {
                IssueVerb::Open => write!(f, " open")?,
                IssueVerb::SuggestBranch => write!(f, " suggest-branch")?,
                IssueVerb::Search { query } => {
                    write!(f, " search")?;
                    for word in query {
                        write!(f, " {word}")?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction, RepoSelector};

    use super::IssueNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved, SubjectArgs,
    };

    fn parse(args: &[&str]) -> IssueNoun {
        IssueNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn issue_open() {
        let resolved = parse(&["issue", "1", "open"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::OpenIssue { id: "1".into() }
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProviderHost,
        });
    }

    #[test]
    fn marked_subject_disambiguates_issue_named_open() {
        let resolved = parse(&["issue", "@open", "open"]).resolve().expect("resolve marked issue subject");
        assert!(matches!(resolved, Resolved::NeedsContext { command: Command {
            action: CommandAction::OpenIssue { id }, ..
        }, .. } if id == "open"));
    }

    #[test]
    fn explicit_subject_preserves_issue_beginning_with_marker() {
        let resolved = parse(&["issue", "--subject", "@open", "open"]).resolve().expect("resolve explicit issue subject");
        assert!(matches!(resolved, Resolved::NeedsContext { command: Command {
            action: CommandAction::OpenIssue { id }, ..
        }, .. } if id == "@open"));
    }

    #[test]
    fn issue_suggest_branch_multiple() {
        let resolved = parse(&["issue", "1,5,7", "suggest-branch"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::GenerateBranchName { issue_keys: vec!["1".into(), "5".into(), "7".into()] },
            },
            repo: RepoContext::Inferred,
            host: HostResolution::ProvisioningTarget,
        });
    }

    #[test]
    fn issue_search() {
        let resolved = parse(&["issue", "search", "my", "query"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryIssues {
                    repo: RepoSelector::Query("".into()),
                    params: flotilla_protocol::issue_query::IssueQuery { search: Some("my query".into()), label: None },
                    page: 1,
                    count: 50,
                },
            },
            repo: RepoContext::Required,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn issue_open_no_subject_errors() {
        let noun = IssueNoun { subjects: SubjectArgs::default(), verb: Some(super::IssueVerb::Open) };
        assert!(noun.resolve().is_err());
    }

    #[test]
    fn round_trip_open() {
        assert_round_trip::<IssueNoun>(&["issue", "1", "open"]);
    }

    #[test]
    fn round_trip_suggest_branch() {
        assert_round_trip::<IssueNoun>(&["issue", "1,5,7", "suggest-branch"]);
    }
}
