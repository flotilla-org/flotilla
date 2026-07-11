use clap::{Parser, Subcommand};
use flotilla_protocol::{Command, CommandAction, CrewCommandContext};

use crate::{
    quote_value,
    resolved::{HostResolution, RepoContext},
    Resolved,
};

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(about = "Communicate with crew members")]
pub struct CrewNoun {
    /// Target crew role, or `list`
    pub subject: String,
    #[command(subcommand)]
    pub verb: Option<CrewVerb>,
    /// Explicit crew identity (normally read from FLOTILLA_CREW_ID)
    #[arg(long)]
    pub crew_id: Option<String>,
    #[arg(long)]
    pub namespace: Option<String>,
    #[arg(long)]
    pub convoy: Option<String>,
    #[arg(long)]
    pub vessel: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
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
            .maybe_vessel(self.vessel)
            .maybe_role(self.role)
            .build();
        let action = match (self.subject.as_str(), self.verb) {
            ("list", None) => CommandAction::QueryCrewList { context },
            ("list", Some(_)) => return Err("`flotilla crew list` does not accept a verb".to_string()),
            (_, Some(CrewVerb::Handoff { message })) => CommandAction::CrewHandoff { context, target: self.subject, message },
            (_, None) => return Err("crew target requires a verb (for example: handoff)".to_string()),
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
        write!(f, "crew {}", self.subject)?;
        for (flag, value) in [
            ("--crew-id", self.crew_id.as_ref()),
            ("--namespace", self.namespace.as_ref()),
            ("--convoy", self.convoy.as_ref()),
            ("--vessel", self.vessel.as_ref()),
            ("--role", self.role.as_ref()),
        ] {
            if let Some(value) = value {
                write!(f, " {flag} {}", quote_value(value))?;
            }
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
    fn explicit_coordinates_are_a_human_fallback() {
        let noun = CrewNoun::try_parse_from([
            "crew",
            "list",
            "--namespace",
            "flotilla",
            "--convoy",
            "demo",
            "--vessel",
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
                vessel: Some("demo-implement".into()),
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
            "--vessel",
            "demo-implement",
            "--role",
            "coder",
            "handoff",
            "--message",
            "review-abc123",
        ]);
    }
}
