use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};
use flotilla_protocol::{CheckoutTarget, Command, CommandAction, RepoSelector};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage repositories")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct RepoNoun {
    /// Repository slug (e.g. owner/repo)
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<RepoVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RepoVerb {
    /// Track a new repository by path
    Add { path: PathBuf },
    /// Stop tracking a repository
    Remove { repo: String },
    /// Refresh repository data (use subject for specific repo, or `repo all refresh` for all)
    Refresh,
    /// Check out a branch in a repository
    Checkout {
        branch: String,
        #[arg(long)]
        fresh: bool,
    },
    /// Prepare terminal for a checkout
    PrepareTerminal { path: PathBuf },
    /// Show providers for a repository
    Providers,
    /// Show work items for a repository
    Work,
}

impl RepoNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (_, Some(RepoVerb::Add { path })) => {
                Ok(Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::TrackRepoPath { path } }))
            }
            (_, Some(RepoVerb::Remove { repo })) => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::UntrackRepo { repo: RepoSelector::Query(repo) },
            })),
            (subject, Some(RepoVerb::Refresh)) => {
                // `repo myslug refresh` → refresh specific, `repo refresh` or `repo all refresh` → refresh all
                let resolved_repo = subject.filter(|s| s != "all").map(RepoSelector::Query);
                Ok(Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: resolved_repo } }))
            }
            (Some(subject), Some(RepoVerb::Checkout { branch, fresh })) => {
                let target = if fresh { CheckoutTarget::FreshBranch(branch) } else { CheckoutTarget::Branch(branch) };
                Ok(Resolved::Command(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::Checkout { repo: RepoSelector::Query(subject), target, issue_ids: vec![] },
                }))
            }
            (None, Some(RepoVerb::Checkout { .. })) => Err("checkout requires a repository subject".into()),
            (Some(subject), Some(RepoVerb::PrepareTerminal { path })) => Ok(Resolved::Command(Command {
                host: None,
                context_repo: Some(RepoSelector::Query(subject)),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path: path, commands: vec![] },
            })),
            (None, Some(RepoVerb::PrepareTerminal { .. })) => Err("prepare-terminal requires a repository subject".into()),
            (Some(subject), Some(RepoVerb::Providers)) => Ok(Resolved::RepoProviders { slug: subject }),
            (None, Some(RepoVerb::Providers)) => Err("providers requires a repository subject".into()),
            (Some(subject), Some(RepoVerb::Work)) => Ok(Resolved::RepoWork { slug: subject }),
            (None, Some(RepoVerb::Work)) => Err("work requires a repository subject".into()),
            (Some(slug), None) => Ok(Resolved::RepoDetail { slug }),
            (None, None) => Err("missing repo arguments".into()),
        }
    }
}

impl fmt::Display for RepoNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "repo")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        if let Some(verb) = &self.verb {
            match verb {
                RepoVerb::Add { path } => write!(f, " add {}", path.display())?,
                RepoVerb::Remove { repo } => write!(f, " remove {repo}")?,
                RepoVerb::Refresh => write!(f, " refresh")?,
                RepoVerb::Checkout { branch, fresh } => {
                    write!(f, " checkout")?;
                    if *fresh {
                        write!(f, " --fresh")?;
                    }
                    write!(f, " {branch}")?;
                }
                RepoVerb::PrepareTerminal { path } => write!(f, " prepare-terminal {}", path.display())?,
                RepoVerb::Providers => write!(f, " providers")?,
                RepoVerb::Work => write!(f, " work")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, path::PathBuf};

    use clap::Parser;
    use flotilla_protocol::{CheckoutTarget, Command, CommandAction, RepoSelector};

    use super::RepoNoun;
    use crate::Resolved;

    fn parse(args: &[&str]) -> RepoNoun {
        RepoNoun::try_parse_from(args).expect("should parse")
    }

    fn assert_round_trip(args: &[&str])
    where
        RepoNoun: fmt::Display + PartialEq + fmt::Debug,
    {
        let parsed = RepoNoun::try_parse_from(args).expect("initial parse");
        let displayed = parsed.to_string();
        let tokens: Vec<&str> = displayed.split_whitespace().collect();
        let reparsed = RepoNoun::try_parse_from(&tokens).expect("re-parse from display");
        assert_eq!(parsed, reparsed, "round-trip failed for: {displayed}");
    }

    #[test]
    fn repo_add() {
        let resolved = parse(&["repo", "add", "/tmp/test"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::TrackRepoPath { path: PathBuf::from("/tmp/test") },
            })
        );
    }

    #[test]
    fn repo_remove() {
        let resolved = parse(&["repo", "remove", "owner/repo"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::UntrackRepo { repo: RepoSelector::Query("owner/repo".into()) },
            })
        );
    }

    #[test]
    fn repo_refresh_all() {
        let resolved = parse(&["repo", "refresh"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } }));
    }

    #[test]
    fn repo_refresh_specific() {
        // noun-subject-verb form: `repo owner/repo refresh`
        let resolved = parse(&["repo", "owner/repo", "refresh"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("owner/repo".into())) },
            })
        );
    }

    #[test]
    fn repo_all_refresh() {
        // `repo all refresh` is the explicit "refresh everything" form
        let resolved = parse(&["repo", "all", "refresh"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } }));
    }

    #[test]
    fn repo_query_detail() {
        let resolved = parse(&["repo", "myslug"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoDetail { slug: "myslug".into() });
    }

    #[test]
    fn repo_query_providers() {
        let resolved = parse(&["repo", "myslug", "providers"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoProviders { slug: "myslug".into() });
    }

    #[test]
    fn repo_query_work() {
        let resolved = parse(&["repo", "myslug", "work"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::RepoWork { slug: "myslug".into() });
    }

    #[test]
    fn repo_checkout_existing_branch() {
        let resolved = parse(&["repo", "myslug", "checkout", "feat-x"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("myslug".into()),
                    target: CheckoutTarget::Branch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn repo_checkout_fresh_branch() {
        let resolved = parse(&["repo", "myslug", "checkout", "--fresh", "feat-x"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("myslug".into()),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn repo_prepare_terminal() {
        let resolved = parse(&["repo", "myslug", "prepare-terminal", "/tmp/path"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: Some(RepoSelector::Query("myslug".into())),
                action: CommandAction::PrepareTerminalForCheckout { checkout_path: PathBuf::from("/tmp/path"), commands: vec![] },
            })
        );
    }

    #[test]
    fn repo_subject_form_refresh() {
        // `repo myslug refresh` — subject used as repo (noun-subject-verb canonical form)
        let resolved = parse(&["repo", "myslug", "refresh"]).resolve().unwrap();
        assert!(matches!(resolved, Resolved::Command(Command { action: CommandAction::Refresh { repo: Some(_) }, .. })));
    }

    #[test]
    fn repo_refresh_no_subject_is_all() {
        // `repo refresh` with no subject means refresh all (shorthand for `repo all refresh`)
        let resolved = parse(&["repo", "refresh"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::Command(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } }));
    }

    #[test]
    fn round_trip_checkout_main() {
        assert_round_trip(&["repo", "myslug", "checkout", "main"]);
    }

    #[test]
    fn round_trip_checkout_fresh() {
        assert_round_trip(&["repo", "myslug", "checkout", "--fresh", "main"]);
    }

    #[test]
    fn round_trip_add() {
        assert_round_trip(&["repo", "add", "/tmp/test"]);
    }

    #[test]
    fn round_trip_providers() {
        assert_round_trip(&["repo", "myslug", "providers"]);
    }

    #[test]
    fn round_trip_work() {
        assert_round_trip(&["repo", "myslug", "work"]);
    }

    #[test]
    fn round_trip_remove() {
        assert_round_trip(&["repo", "remove", "org/repo"]);
    }

    #[test]
    fn round_trip_refresh_all() {
        assert_round_trip(&["repo", "refresh"]);
    }

    #[test]
    fn round_trip_refresh_specific() {
        assert_round_trip(&["repo", "org/repo", "refresh"]);
    }

    #[test]
    fn round_trip_all_refresh() {
        assert_round_trip(&["repo", "all", "refresh"]);
    }

    #[test]
    fn round_trip_prepare_terminal() {
        assert_round_trip(&["repo", "myslug", "prepare-terminal", "/tmp/path"]);
    }
}
