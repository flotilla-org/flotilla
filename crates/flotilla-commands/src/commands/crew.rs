use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, CrewCommandContext, CrewSupervisionAction, StallReason};

use crate::{
    quote_value,
    resolved::{HostResolution, RepoContext},
    subject::{is_crew_command_subject, SubjectInterpretation},
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
    /// Work vessel name for `crew supervise`
    #[arg(long)]
    pub vessel: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    /// Completion or failure message
    #[arg(long)]
    pub message: Option<String>,
    /// Why crew work is blocked while it remains wanted
    #[arg(long)]
    pub reason: Option<String>,
    /// Machine-readable settlement answer declared by the brief
    #[arg(long)]
    pub disposition: Option<String>,
    /// URL of the PR comment containing this claim's decision ledger
    #[arg(long = "decision-ledger-ref")]
    pub decision_ledger_ref: Option<String>,
    /// Admit a ledger-less completion as the connected operator principal
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CrewVerb {
    /// Ensure the target is running and deliver it a message
    Handoff {
        #[arg(long)]
        message: String,
    },
    /// Resume stalled crew work with guidance
    Resume {
        #[arg(long)]
        message: String,
    },
    /// Convert stalled crew work to failed
    ConvertToFailed {
        #[arg(long)]
        message: String,
    },
    /// Escalate stalled crew work to the next supervisor
    Escalate {
        #[arg(long)]
        message: String,
    },
}

impl CrewNoun {
    pub fn resolve_with_crew_id(self, ambient_crew_id: Option<String>) -> Result<Resolved, String> {
        let supervision_target = (
            self.namespace.clone(),
            self.convoy.clone(),
            self.vessel.clone(),
            self.role.clone(),
            self.crew_id.clone().or(ambient_crew_id.clone()),
        );
        let context = CrewCommandContext::builder()
            .maybe_crew_id(self.crew_id.or(ambient_crew_id))
            .maybe_namespace(self.namespace)
            .maybe_convoy(self.convoy)
            .maybe_vessel_ref(self.vessel_ref)
            .maybe_role(self.role)
            .build();
        let subject = self.subjects.resolve()?.ok_or_else(|| "crew command requires a command or target subject".to_string())?;
        let action = match (subject.value.as_str(), subject.interpretation, self.verb) {
            ("list", SubjectInterpretation::Ordinary, None)
                if self.message.is_none()
                    && self.reason.is_none()
                    && self.disposition.is_none()
                    && self.decision_ledger_ref.is_none()
                    && !self.force =>
            {
                CommandAction::QueryCrewList { context }
            }
            ("list", SubjectInterpretation::Ordinary, None) => {
                return Err("`flotilla crew list` does not accept completion options".to_string());
            }
            ("complete", SubjectInterpretation::Ordinary, None) => {
                if self.reason.is_some() {
                    return Err("--reason is only valid with `flotilla crew stall`".to_string());
                }
                if self
                    .decision_ledger_ref
                    .as_deref()
                    .is_some_and(|reference| !(reference.starts_with("https://") || reference.starts_with("http://")))
                {
                    return Err("--decision-ledger-ref must use an HTTP(S) URL".to_string());
                }
                CommandAction::CrewComplete {
                    context,
                    message: self.message,
                    disposition: self.disposition,
                    decision_ledger_ref: self.decision_ledger_ref,
                    force: self.force,
                }
            }
            ("fail", SubjectInterpretation::Ordinary, None)
                if self.reason.is_some() || self.disposition.is_some() || self.decision_ledger_ref.is_some() || self.force =>
            {
                return Err("`flotilla crew fail` does not accept completion options".to_string());
            }
            ("fail", SubjectInterpretation::Ordinary, None) => CommandAction::CrewFail {
                context,
                message: self.message.ok_or_else(|| "`flotilla crew fail` requires --message".to_string())?,
            },
            ("stall", SubjectInterpretation::Ordinary, None) => {
                if self.disposition.is_some() || self.decision_ledger_ref.is_some() || self.force {
                    return Err("`flotilla crew stall` does not accept completion options".to_string());
                }
                let reason = self.reason.ok_or_else(|| "`flotilla crew stall` requires --reason".to_string())?;
                let reason = match reason.as_str() {
                    "infra" => StallReason::Infra,
                    "scope" => StallReason::Scope,
                    "decision" => StallReason::Decision,
                    "access" => StallReason::Access,
                    "other" => StallReason::Other,
                    _ => return Err(format!("invalid stall reason `{reason}`; expected infra, scope, decision, access, or other")),
                };
                CommandAction::CrewStall {
                    context,
                    reason,
                    message: self.message.ok_or_else(|| "`flotilla crew stall` requires --message".to_string())?,
                }
            }
            ("supervise", SubjectInterpretation::Ordinary, Some(verb)) => {
                let (namespace, convoy, vessel, role, actor_crew_id) = supervision_target;
                let (action, message) = match verb {
                    CrewVerb::Resume { message } => (CrewSupervisionAction::Resume, message),
                    CrewVerb::ConvertToFailed { message } => (CrewSupervisionAction::Fail, message),
                    CrewVerb::Escalate { message } => (CrewSupervisionAction::Escalate, message),
                    CrewVerb::Handoff { .. } => return Err("`flotilla crew supervise` requires resume, fail, or escalate".to_string()),
                };
                CommandAction::CrewSupervise {
                    namespace,
                    convoy: convoy.ok_or_else(|| "crew supervise requires --convoy".to_string())?,
                    vessel: vessel.ok_or_else(|| "crew supervise requires --vessel".to_string())?,
                    role: role.ok_or_else(|| "crew supervise requires --role".to_string())?,
                    operation: action,
                    message,
                    actor_crew_id,
                }
            }
            (reserved, SubjectInterpretation::Ordinary, Some(_)) if is_crew_command_subject(reserved) => {
                return Err(format!("`{reserved}` is a crew command; use `@{reserved}` to address the crew role"));
            }
            (_, _, Some(_)) if self.force => return Err("--force is only valid with `flotilla crew complete`".to_string()),
            (_, _, Some(CrewVerb::Handoff { message })) => CommandAction::CrewHandoff { context, target: subject.value, message },
            (_, _, Some(_)) => return Err("resume, fail, and escalate require `flotilla crew supervise`".to_string()),
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
            ("--vessel", self.vessel.as_ref()),
            ("--role", self.role.as_ref()),
        ] {
            if let Some(value) = value {
                write!(f, " {flag} {}", quote_value(value))?;
            }
        }
        if let Some(message) = &self.message {
            write!(f, " --message {}", quote_value(message))?;
        }
        if let Some(reason) = &self.reason {
            write!(f, " --reason {}", quote_value(reason))?;
        }
        if let Some(disposition) = &self.disposition {
            write!(f, " --disposition {}", quote_value(disposition))?;
        }
        if let Some(reference) = &self.decision_ledger_ref {
            write!(f, " --decision-ledger-ref {}", quote_value(reference))?;
        }
        if self.force {
            write!(f, " --force")?;
        }
        if let Some(verb) = &self.verb {
            let (name, message) = match verb {
                CrewVerb::Handoff { message } => ("handoff", message),
                CrewVerb::Resume { message } => ("resume", message),
                CrewVerb::ConvertToFailed { message } => ("convert-to-failed", message),
                CrewVerb::Escalate { message } => ("escalate", message),
            };
            write!(f, " {name} --message {}", quote_value(message))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use flotilla_protocol::{CommandAction, CrewCommandContext, StallReason};

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
    fn stall_requires_closed_reason_and_message() {
        let noun = CrewNoun::try_parse_from(["crew", "stall", "--reason", "access", "--message", "repo denied"]).expect("parse stall");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewStall {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            reason: StallReason::Access,
            message: "repo denied".into(),
        });
        let invalid =
            CrewNoun::try_parse_from(["crew", "stall", "--reason", "maybe", "--message", "blocked"]).expect("parse invalid reason");
        assert!(invalid.resolve_with_crew_id(None).expect_err("closed reason").contains("invalid stall reason"));
    }

    #[test]
    fn supervisor_resume_names_source_and_actor() {
        let noun = CrewNoun::try_parse_from([
            "crew",
            "supervise",
            "--convoy",
            "work",
            "--vessel",
            "implement",
            "--role",
            "coder",
            "resume",
            "--message",
            "try again",
        ])
        .expect("parse supervisor resume");
        assert_eq!(action(noun, Some("governor-crew")), CommandAction::CrewSupervise {
            namespace: None,
            convoy: "work".into(),
            vessel: "implement".into(),
            role: "coder".into(),
            operation: flotilla_protocol::CrewSupervisionAction::Resume,
            message: "try again".into(),
            actor_crew_id: Some("governor-crew".into()),
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
            disposition: None,
            decision_ledger_ref: None,
            force: false,
        });
    }

    #[test]
    fn complete_preserves_declared_disposition() {
        let noun = CrewNoun::try_parse_from(["crew", "complete", "--message", "ready for review", "--disposition", "changes-pushed"])
            .expect("parse complete disposition");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewComplete {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            message: Some("ready for review".into()),
            disposition: Some("changes-pushed".into()),
            decision_ledger_ref: None,
            force: false,
        });
    }

    #[test]
    fn complete_preserves_decision_ledger_pointer() {
        let url = "https://github.com/flotilla-org/flotilla/pull/1#issuecomment-2";
        let noun = CrewNoun::try_parse_from(["crew", "complete", "--decision-ledger-ref", url]).expect("parse ledger pointer");
        assert_eq!(action(noun, Some("crew-123")), CommandAction::CrewComplete {
            context: CrewCommandContext { crew_id: Some("crew-123".into()), ..Default::default() },
            message: None,
            disposition: None,
            decision_ledger_ref: Some(url.into()),
            force: false,
        });
    }

    #[test]
    fn decision_ledger_pointer_must_be_a_comment_url() {
        let noun =
            CrewNoun::try_parse_from(["crew", "complete", "--decision-ledger-ref", "not-a-url"]).expect("parse invalid ledger pointer");
        assert_eq!(
            noun.resolve_with_crew_id(Some("crew-123".into())).expect_err("invalid pointer should fail"),
            "--decision-ledger-ref must use an HTTP(S) URL"
        );
    }

    #[test]
    fn unsupported_verb_error_precedes_decision_ledger_url_validation() {
        let noun =
            CrewNoun::try_parse_from(["crew", "list", "--decision-ledger-ref", "not-a-url"]).expect("parse unsupported ledger pointer");
        assert_eq!(
            noun.resolve_with_crew_id(Some("crew-123".into())).expect_err("list should reject pointer"),
            "`flotilla crew list` does not accept completion options"
        );
    }

    #[test]
    fn complete_preserves_operator_force() {
        let noun = CrewNoun::try_parse_from(["crew", "complete", "--force"]).expect("parse forced completion");
        assert!(matches!(action(noun, Some("crew-123")), CommandAction::CrewComplete { force: true, .. }));
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
