use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};
use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, RepoSelector};

use crate::Resolved;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage checkouts")]
#[command(subcommand_precedence_over_arg = true, subcommand_negates_reqs = true)]
pub struct CheckoutNoun {
    /// Branch name or checkout path
    pub subject: Option<String>,

    #[command(subcommand)]
    pub verb: Option<CheckoutVerb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CheckoutVerb {
    /// Create a new checkout
    Create {
        #[arg(long)]
        branch: String,
        #[arg(long)]
        fresh: bool,
    },
    /// Remove a checkout
    Remove,
    /// Show checkout status
    Status {
        #[arg(long)]
        checkout_path: Option<PathBuf>,
        #[arg(long)]
        cr_id: Option<String>,
    },
}

impl CheckoutNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match (self.subject, self.verb) {
            (_, Some(CheckoutVerb::Create { branch, fresh })) => {
                let target = if fresh { CheckoutTarget::FreshBranch(branch) } else { CheckoutTarget::Branch(branch) };
                // SENTINEL: repo is empty — `inject_repo_context` in main.rs must fill it
                // from --repo flag or FLOTILLA_REPO env before dispatch to the daemon.
                Ok(Resolved::Command(Command {
                    host: None,
                    context_repo: None,
                    action: CommandAction::Checkout { repo: RepoSelector::Query("".into()), target, issue_ids: vec![] },
                }))
            }
            (Some(subject), Some(CheckoutVerb::Remove)) => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(subject) },
            })),
            (None, Some(CheckoutVerb::Remove)) => Err("remove requires a checkout subject".into()),
            (Some(subject), Some(CheckoutVerb::Status { checkout_path, cr_id })) => Ok(Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus { branch: subject, checkout_path, change_request_id: cr_id },
            })),
            (None, Some(CheckoutVerb::Status { .. })) => Err("status requires a checkout subject".into()),
            (_, None) => Err("missing checkout verb".into()),
        }
    }
}

impl fmt::Display for CheckoutNoun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "checkout")?;
        if let Some(subject) = &self.subject {
            write!(f, " {subject}")?;
        }
        if let Some(verb) = &self.verb {
            match verb {
                CheckoutVerb::Create { branch, fresh } => {
                    write!(f, " create --branch {branch}")?;
                    if *fresh {
                        write!(f, " --fresh")?;
                    }
                }
                CheckoutVerb::Remove => write!(f, " remove")?,
                CheckoutVerb::Status { checkout_path, cr_id } => {
                    write!(f, " status")?;
                    if let Some(p) = checkout_path {
                        write!(f, " --checkout-path {}", p.display())?;
                    }
                    if let Some(id) = cr_id {
                        write!(f, " --cr-id {id}")?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, path::PathBuf};

    use clap::Parser;
    use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, RepoSelector};

    use super::CheckoutNoun;
    use crate::Resolved;

    fn parse(args: &[&str]) -> CheckoutNoun {
        CheckoutNoun::try_parse_from(args).expect("should parse")
    }

    fn assert_round_trip(args: &[&str])
    where
        CheckoutNoun: fmt::Display + PartialEq + fmt::Debug,
    {
        let parsed = CheckoutNoun::try_parse_from(args).expect("initial parse");
        let displayed = parsed.to_string();
        let tokens: Vec<&str> = displayed.split_whitespace().collect();
        let reparsed = CheckoutNoun::try_parse_from(&tokens).expect("re-parse from display");
        assert_eq!(parsed, reparsed, "round-trip failed for: {displayed}");
    }

    #[test]
    fn checkout_create_branch() {
        let resolved = parse(&["checkout", "create", "--branch", "feat-x"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("".into()),
                    target: CheckoutTarget::Branch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn checkout_create_fresh_branch() {
        let resolved = parse(&["checkout", "create", "--branch", "feat-x", "--fresh"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("".into()),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![],
                },
            })
        );
    }

    #[test]
    fn checkout_remove() {
        let resolved = parse(&["checkout", "my-feature", "remove"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("my-feature".into()) },
            })
        );
    }

    #[test]
    fn checkout_status_subject_only() {
        let resolved = parse(&["checkout", "my-feature", "status"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus { branch: "my-feature".into(), checkout_path: None, change_request_id: None },
            })
        );
    }

    #[test]
    fn checkout_status_with_all_flags() {
        let resolved = parse(&["checkout", "my-feature", "status", "--checkout-path", "/tmp/wt", "--cr-id", "42"]).resolve().unwrap();
        assert_eq!(
            resolved,
            Resolved::Command(Command {
                host: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus {
                    branch: "my-feature".into(),
                    checkout_path: Some(PathBuf::from("/tmp/wt")),
                    change_request_id: Some("42".into()),
                },
            })
        );
    }

    #[test]
    fn checkout_remove_no_subject_errors() {
        let noun = CheckoutNoun { subject: None, verb: Some(super::CheckoutVerb::Remove) };
        assert!(noun.resolve().is_err());
    }

    #[test]
    fn round_trip_remove() {
        assert_round_trip(&["checkout", "my-feature", "remove"]);
    }

    #[test]
    fn round_trip_create_branch() {
        assert_round_trip(&["checkout", "create", "--branch", "feat-x"]);
    }

    #[test]
    fn round_trip_create_branch_fresh() {
        assert_round_trip(&["checkout", "create", "--branch", "feat-x", "--fresh"]);
    }

    #[test]
    fn round_trip_status() {
        assert_round_trip(&["checkout", "my-feature", "status"]);
    }
}
