use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, CrewCommandContext};

use crate::{
    quote_value,
    resolved::{HostResolution, RepoContext},
    subject::SubjectInterpretation,
    Resolved, SubjectArgs,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser, bon::Builder)]
#[command(about = "Communicate with crew members")]
pub struct CrewNoun {
    #[command(flatten)]
    pub subjects: SubjectArgs,
    #[command(subcommand)]
    pub verb: Option<CrewVerb>,
    /// Explicit crew identity (normally read from FLOTILLA_CREW_ID)
    #[arg(long)]
    pub crew_id: Option<String>,
    #[arg(long)]
    pub namespace: Option<String>,
    #[arg(long)]
    pub convoy: Option<String>,
    /// Vessel resource name (e.g. `myconvoy-implement`)
    #[arg(long = "vessel-ref")]
    pub vessel_ref: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    /// Completion or failure message
    #[arg(long)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CrewVerb {
    /// Ensure the target is running and deliver it a message
    Handoff {
        #[arg(long)]
        message: String,
    },
}

impl CrewNoun {
    pub fn resolve_with_crew_id(self, ambient_crew_id: Option<String>) -> Result<Resolved, String> {
        let context = CrewCommandContext::builder()
            .maybe_crew_id(self.crew_id.or(ambient_crew_id))
            .maybe_namespace(self.namespace)
            .maybe_convoy(self.convoy)
            .maybe_vessel_ref(self.vessel_ref)
            .maybe_role(self.role)
            .build();
        let subject = self.subjects.resolve()?.ok_or_else(|| "crew command requires a command or target subject".to_string())?;
        let action = match (subject.value.as_str(), subject.interpretation, self.verb) {
            ("list", SubjectInterpretation::Ordinary, None) if self.message.is_none() => CommandAction::QueryCrewList { context },
            ("list", SubjectInterpretation::Ordinary, None) => {
                return Err("`flotilla crew list` does not accept --message".to_string());
            }
            ("complete", SubjectInterpretation::Ordinary, None) => CommandAction::CrewComplete { context, message: self.message },
            ("fail", SubjectInterpretation::Ordinary, None) => CommandAction::CrewFail {
                context,
                message: self.message.ok_or_else(|| "`flotilla crew fail` requires --message".to_string())?,
            },
            (reserved @ ("list" | "complete" | "fail"), SubjectInterpretation::Ordinary, Some(_)) => {
                return Err(format!("`{reserved}` is a crew command; use `@{reserved}` to address the crew role"));
            }
            (_, _, Some(CrewVerb::Handoff { message })) => CommandAction::CrewHandoff { context, target: subject.value, message },
            (_, _, None) => return Err("crew target requires a verb (for example: handoff)".to_string()),
        };
        Ok(Resolved::NeedsContext {
            command: Command { node_id: None, provisioning_target: None, context_repo: None, action },
            repo: RepoContext::None,
            host: HostResolution::Local,
        })
    }

    pub fn resolve(self) -> Result<Resolved, String> {
        self.resolve_with_crew_id(None)
    }
}

impl std::fmt::Display for CrewNoun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "crew")?;
        self.subjects.write(f)?;
        for (flag, value) in [
            ("--crew-id", self.crew_id.as_ref()),
            ("--namespace", self.namespace.as_ref()),
            ("--convoy", self.convoy.as_ref()),
            ("--vessel-ref", self.vessel_ref.as_ref()),
            ("--role", self.role.as_ref()),
        ] {
            if let Some(value) = value {
                write!(f, " {flag} {}", quote_value(value))?;
            }
        }
        if let Some(message) = &self.message {
            write!(f, " --message {}", quote_value(message))?;
        }
        if let Some(CrewVerb::Handoff { message }) = &self.verb {
            write!(f, " handoff --message {}", quote_value(message))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{CommandAction, CrewCommandContext};

    use super::CrewNoun;
    use crate::{test_utils::assert_round_trip, Resolved};

    fn action(noun: CrewNoun, ambient_crew_id: Option<&str>) -> CommandAction {
        let resolved = noun.resolve_with_crew_id(ambient_crew_id.map(str::to_string)).expect("resolve crew command");
        let Resolved::NeedsContext { command, .. } = resolved else {
            panic!("crew command should resolve locally");
        };
        command.action
    }

    #[test]
    fn list_uses_ambient_crew_identity() {
        let noun = CrewNoun::try_parse_from(["crew", "list"]).expect("parse list");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::QueryCrewList {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() }
        });
    }

    #[test]
    fn handoff_preserves_target_and_message() {
        let noun = CrewNoun::try_parse_from(["crew", "reviewer", "handoff", "--message", "Review commit abc123"]).expect("parse handoff");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewHandoff {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            target: "reviewer".into(),
            message: "Review commit abc123".into(),
        });
    }

    #[test]
    fn address_marker_disambiguates_reserved_role_names() {
        for role in ["list", "complete", "fail"] {
            let marked = format!("@{role}");
            let noun = CrewNoun::try_parse_from(["crew", &marked, "handoff", "--message", "continue"]).expect("parse marked crew role");
            assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewHandoff {
                context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
                target: role.into(),
                message: "continue".into(),
            });
        }
    }

    #[test]
    fn unmarked_reserved_role_names_explain_the_address_marker() {
        for role in ["list", "complete", "fail"] {
            let noun = CrewNoun::try_parse_from(["crew", role, "handoff", "--message", "continue"]).expect("parse ambiguous crew role");
            let error = noun.resolve_with_crew_id(Some("crew-123".into())).expect_err("unmarked reserved role should fail");
            assert!(error.contains(&format!("@{role}")), "unexpected error: {error}");
        }
    }

    #[test]
    fn explicit_subject_preserves_literal_address_marker() {
        let noun = CrewNoun::try_parse_from(["crew", "--subject", "@reviewer", "handoff", "--message", "continue"])
            .expect("parse explicit crew subject");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewHandoff {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            target: "@reviewer".into(),
            message: "continue".into(),
        });
    }

    #[test]
    fn positional_and_explicit_subject_conflict() {
        let error = CrewNoun::try_parse_from(["crew", "reviewer", "--subject", "other", "handoff", "--message", "continue"])
            .expect_err("subjects should be mutually exclusive");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn complete_uses_ambient_crew_identity() {
        let noun = CrewNoun::try_parse_from(["crew", "complete", "--message", "ready for review"]).expect("parse complete");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewComplete {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            message: Some("ready for review".into()),
        });
    }

    #[test]
    fn fail_uses_ambient_crew_identity() {
        let noun = CrewNoun::try_parse_from(["crew", "fail", "--message", "cannot reproduce"]).expect("parse fail");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewFail {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            message: "cannot reproduce".into(),
        });
    }

    #[test]
    fn explicit_coordinates_are_a_human_fallback() {
        let noun = CrewNoun::try_parse_from([
            "crew",
            "list",
            "--namespace",
            "flotilla",
            "--convoy",
            "demo",
            "--vessel-ref",
            "demo-implement",
            "--role",
            "coder",
        ])
        .expect("parse fallback");
        assert_eq!(action(noun, None), CommandAction::QueryCrewList {
            context: CrewCommandContext {
                crew_id: None,
                namespace: Some("flotilla".into()),
                convoy: Some("demo".into()),
                vessel_ref: Some("demo-implement".into()),
                role: Some("coder".into()),
            }
        });
    }

    #[test]
    fn handoff_with_explicit_context_round_trips() {
        assert_round_trip::<CrewNoun>(&[
            "crew",
            "reviewer",
            "--namespace",
            "flotilla",
            "--convoy",
            "demo",
            "--vessel-ref",
            "demo-implement",
            "--role",
            "coder",
            "handoff",
            "--message",
            "review-abc123",
        ]);
    }

    #[test]
    fn marked_and_explicit_subjects_round_trip() {
        assert_round_trip::<CrewNoun>(&["crew", "@list", "handoff", "--message", "review"]);
        assert_round_trip::<CrewNoun>(&["crew", "--subject", "@reviewer", "handoff", "--message", "review"]);
    }
}
