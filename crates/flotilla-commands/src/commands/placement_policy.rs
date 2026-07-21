use std::path::PathBuf;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    quote::quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage placement policies")]
pub struct PlacementPolicyNoun {
    /// Placement policy name (used as metadata.name on the resource)
    pub subject: String,

    #[command(subcommand)]
    pub verb: PlacementPolicyVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PlacementPolicyVerb {
    /// Apply a placement policy spec from a YAML file
    Apply {
        /// Path to a YAML file containing the PlacementPolicySpec body
        #[arg(long = "file", short = 'f')]
        file: PathBuf,
    },
}

impl PlacementPolicyNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            PlacementPolicyVerb::Apply { file } => {
                let spec_yaml = std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::PlacementPolicyApply { name: self.subject, spec_yaml },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
        }
    }
}

impl std::fmt::Display for PlacementPolicyNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "placement-policy {}", quote_value(&self.subject))?;
        match &self.verb {
            PlacementPolicyVerb::Apply { file } => write!(f, " apply --file {}", quote_value(&file.display().to_string()))?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::PlacementPolicyNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> PlacementPolicyNoun {
        PlacementPolicyNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn apply_reads_file_into_spec_yaml() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(
            tmp.path(),
            "pool: docker\ndocker_per_vessel:\n  host_ref: host-1\n  image: ghcr.io/flotilla/dev:latest\n  checkout:\n    fresh_clone_in_container:\n      clone_path: /workspace\n  agent_adapters:\n    - codex\n    - claude-code\n",
        )
        .expect("write");
        let path = tmp.path().to_string_lossy().to_string();

        let resolved = parse(&["placement-policy", "docker-worktree", "apply", "--file", &path]).resolve().expect("resolve");
        assert!(matches!(
            resolved,
            Resolved::NeedsContext {
                command: Command { action: CommandAction::PlacementPolicyApply { name, spec_yaml }, .. },
                ..
            } if name == "docker-worktree" && spec_yaml.contains("agent_adapters")
        ));
    }

    #[test]
    fn missing_file_returns_error() {
        let err = parse(&["placement-policy", "docker-worktree", "apply", "--file", "/nonexistent/path/policy.yaml"])
            .resolve()
            .expect_err("should fail");
        assert!(err.contains("read"));
    }

    #[test]
    fn round_trip_apply() {
        assert_round_trip::<PlacementPolicyNoun>(&["placement-policy", "docker-worktree", "apply", "--file", "/tmp/x.yaml"]);
    }

    #[test]
    fn apply_display_quotes_path_with_whitespace() {
        let parsed = parse(&["placement-policy", "docker-worktree", "apply", "--file", "/tmp/my dir/policy.yaml"]);
        let displayed = parsed.to_string();
        assert!(displayed.contains("--file \"/tmp/my dir/policy.yaml\""), "expected quoted file path in {displayed:?}");
    }
}
