use std::path::PathBuf;

use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction};

use crate::{
    quote::quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Manage workflow templates")]
pub struct WorkflowTemplateNoun {
    /// Workflow template name (used as metadata.name on the resource)
    pub subject: String,

    #[command(subcommand)]
    pub verb: WorkflowTemplateVerb,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WorkflowTemplateVerb {
    /// Apply a workflow template spec from a YAML file
    Apply {
        /// Path to a YAML file containing the WorkflowTemplate spec body
        #[arg(long = "file", short = 'f')]
        file: PathBuf,
    },
}

impl WorkflowTemplateNoun {
    pub fn resolve(self) -> Result<Resolved, String> {
        match self.verb {
            WorkflowTemplateVerb::Apply { file } => {
                let spec_yaml = std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
                Ok(Resolved::NeedsContext {
                    command: Command {
                        node_id: None,
                        provisioning_target: None,
                        context_repo: None,
                        action: CommandAction::WorkflowTemplateApply { name: self.subject, spec_yaml },
                    },
                    repo: RepoContext::None,
                    host: HostResolution::Local,
                })
            }
        }
    }
}

impl std::fmt::Display for WorkflowTemplateNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "workflow-template {}", quote_value(&self.subject))?;
        match &self.verb {
            WorkflowTemplateVerb::Apply { file } => write!(f, " apply --file {}", quote_value(&file.display().to_string()))?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{Command, CommandAction};

    use super::WorkflowTemplateNoun;
    use crate::{
        resolved::{HostResolution, RepoContext},
        test_utils::assert_round_trip,
        Resolved,
    };

    fn parse(args: &[&str]) -> WorkflowTemplateNoun {
        WorkflowTemplateNoun::try_parse_from(args).expect("should parse")
    }

    #[test]
    fn apply_reads_file_into_spec_yaml() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "tasks: []\n").expect("write");
        let path = tmp.path().to_string_lossy().to_string();

        let resolved = parse(&["workflow-template", "scratch", "apply", "--file", &path]).resolve().expect("resolve");
        assert_eq!(resolved, Resolved::NeedsContext {
            command: Command {
                node_id: None,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::WorkflowTemplateApply { name: "scratch".into(), spec_yaml: "tasks: []\n".into() },
            },
            repo: RepoContext::None,
            host: HostResolution::Local,
        });
    }

    #[test]
    fn missing_file_returns_error() {
        let err = parse(&["workflow-template", "scratch", "apply", "--file", "/nonexistent/path/template.yaml"])
            .resolve()
            .expect_err("should fail");
        assert!(err.contains("read"));
    }

    #[test]
    fn round_trip_apply() {
        assert_round_trip::<WorkflowTemplateNoun>(&["workflow-template", "scratch", "apply", "--file", "/tmp/x.yaml"]);
    }

    #[test]
    fn apply_display_quotes_path_with_whitespace() {
        let parsed = parse(&["workflow-template", "scratch", "apply", "--file", "/tmp/my dir/template.yaml"]);
        let displayed = parsed.to_string();
        assert!(displayed.contains("--file \"/tmp/my dir/template.yaml\""), "expected quoted file path in {displayed:?}");
    }
}
