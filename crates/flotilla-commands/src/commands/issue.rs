use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, RepoSelector};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Issues")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct IssueNoun {
    /// Issue ID or comma-separated IDs (e.g. "#1,#5,#7")
    pub subject: Option<String>,

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
        match (self.subject, self.verb) {
            (Some(subject), Some(IssueVerb::Open)) => {
                Ok(Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::OpenIssue { id: subject } }))
            }
            (None, Some(IssueVerb::Open)) => Err("open requires an issue subject".into()),
            (Some(subject), Some(IssueVerb::SuggestBranch)) => {
                let issue_keys = subject.split(',').map(|s| s.trim().to_string()).collect();
                Ok(Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::GenerateBranchName { issue_keys } }))
            }
            (None, Some(IssueVerb::SuggestBranch)) => Err("suggest-branch requires an issue subject".into()),
            (_, Some(IssueVerb::Search { query })) => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                // SENTINEL: repo is empty — `inject_repo_context` in main.rs must fill it
                // from --repo flag or FLOTILLA_REPO env before dispatch to the daemon.
                action: CommandAction::SearchIssues { repo: RepoSelector::Query("".into()), query: query.join(" ") },
            })),
            (_, None) => Err("missing issue verb".into()),
        }
    }
}

impl std::fmt::Display for IssueNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "issue")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
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
    use crate::{test_utils::assert_round_trip, Resolved};

    fn parse(args: &[&str]) -> IssueNoun {
        IssueNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn issue_open() {
        let resolved = parse(&["issue", "1", "open"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::OpenIssue { id: "1".into() } })
        );
    }

    #[test]
    fn issue_suggest_branch_multiple() {
        let resolved = parse(&["issue", "1,5,7", "suggest-branch"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::GenerateBranchName { issue_keys: vec!["1".into(), "5".into(), "7".into()] },
            })
        );
    }

    #[test]
    fn issue_search() {
        let resolved = parse(&["issue", "search", "my", "query"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::SearchIssues { repo: RepoSelector::Query("".into()), query: "my query".into() },
            })
        );
    }

    #[test]
    fn issue_open_no_subject_errors() {
        let noun = IssueNoun { subject: None, verb: Some(super::IssueVerb::Open) };
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
