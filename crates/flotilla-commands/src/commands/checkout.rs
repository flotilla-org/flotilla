use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};
use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, RepoSelector};

use crate::{
    resolved::{HostResolution, RepoContext},
    Resolved,
};

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
            (Some(_), Some(CheckoutVerb::Create { .. })) => {
                Err("checkout create does not take a subject (repo comes from --repo or FLOTILLA_REPO)".into())
            }
            (None, Some(CheckoutVerb::Create { branch, fresh })) => {
                let target = if fresh { CheckoutTarget::FreshBranch(branch) } else { CheckoutTarget::Branch(branch) };
                // SENTINEL: repo is empty — dispatch must fill it from --repo or FLOTILLA_REPO.
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::Checkout { repo: RepoSelector::Query("".into()), target, issue_ids: vec![] },
                    },
                    repo: RepoContext::Required,
                    host: HostResolution::ProvisioningTarget,
                })
            }
            (Some(subject), Some(CheckoutVerb::Remove)) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(subject) },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::SubjectHost,
            }),
            (None, Some(CheckoutVerb::Remove)) => Err("remove requires a checkout subject".into()),
            (Some(subject), Some(CheckoutVerb::Status { checkout_path, cr_id })) => Ok(Resolved::NeedsContext {
                command: Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: None,
                    action: CommandAction::FetchCheckoutStatus { branch: subject, checkout_path, change_request_id: cr_id },
                },
                repo: RepoContext::Inferred,
                host: HostResolution::SubjectHost,
            }),
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
    use std::path::PathBuf;

    use clap::Parser;
    use flotilla_protocol::{CheckoutSelector, CheckoutTarget, Command, CommandAction, RepoSelector};

    use super::CheckoutNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> CheckoutNoun {
        CheckoutNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn checkout_create_branch() {
        let resolved = parse(&["checkout", "create", "--branch", "feat-x"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("".into()),
                    target: CheckoutTarget::Branch("feat-x".into()),
                    issue_ids: vec![],
                },
            },
            repo: RepoContext::Required,
            host: HostResolution::ProvisioningTarget,
        });
    }

    #[test]
    fn checkout_create_fresh_branch() {
        let resolved = parse(&["checkout", "create", "--branch", "feat-x", "--fresh"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("".into()),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![],
                },
            },
            repo: RepoContext::Required,
            host: HostResolution::ProvisioningTarget,
        });
    }

    #[test]
    fn checkout_remove() {
        let resolved = parse(&["checkout", "my-feature", "remove"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query("my-feature".into()) },
            },
            repo: RepoContext::Inferred,
            host: HostResolution::SubjectHost,
        });
    }

    #[test]
    fn checkout_status_subject_only() {
        let resolved = parse(&["checkout", "my-feature", "status"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus { branch: "my-feature".into(), checkout_path: None, change_request_id: None },
            },
            repo: RepoContext::Inferred,
            host: HostResolution::SubjectHost,
        });
    }

    #[test]
    fn checkout_status_with_all_flags() {
        let resolved = parse(&["checkout", "my-feature", "status", "--checkout-path", "/tmp/wt", "--cr-id", "42"]).resolve().unwrap();
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus {
                    branch: "my-feature".into(),
                    checkout_path: Some(PathBuf::from("/tmp/wt")),
                    change_request_id: Some("42".into()),
                },
            },
            repo: RepoContext::Inferred,
            host: HostResolution::SubjectHost,
        });
    }

    #[test]
    fn checkout_remove_no_subject_errors() {
        let noun = CheckoutNoun { subject: None, verb: Some(super::CheckoutVerb::Remove) };
        assert!(noun.resolve().is_err());
    }

    #[test]
    fn round_trip_remove() {
        assert_round_trip::<CheckoutNoun>(&["checkout", "my-feature", "remove"]);
    }

    #[test]
    fn round_trip_create_branch() {
        assert_round_trip::<CheckoutNoun>(&["checkout", "create", "--branch", "feat-x"]);
    }

    #[test]
    fn round_trip_create_branch_fresh() {
        assert_round_trip::<CheckoutNoun>(&["checkout", "create", "--branch", "feat-x", "--fresh"]);
    }

    #[test]
    fn round_trip_status() {
        assert_round_trip::<CheckoutNoun>(&["checkout", "my-feature", "status"]);
    }

    #[test]
    fn checkout_create_with_subject_errors() {
        // `checkout myslug create --branch feat` should error — create doesn't take a subject
        let noun = parse(&["checkout", "myslug", "create", "--branch", "feat"]);
        assert!(noun.resolve().is_err());
    }
}
