use std::sync::OnceLock;

use clap::{
    error::{ContextKind, ContextValue, ErrorKind},
    Args, CommandFactory, Subcommand,
};

use crate::{commands::host::HostNounPartial, noun::NounCommand, quote_value};

pub const ADDRESS_MARKER: char = '@';
const CREW_COMMAND_SUBJECTS: &[&str] = &["list", "complete", "fail"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectNoun {
    Checkout,
    Crew,
    Environment,
    Host,
    Issue,
    Repo,
}

impl SubjectNoun {
    pub fn from_command_name(name: &str) -> Option<Self> {
        match name {
            "checkout" => Some(Self::Checkout),
            "crew" => Some(Self::Crew),
            "environment" | "env" => Some(Self::Environment),
            "host" => Some(Self::Host),
            "issue" => Some(Self::Issue),
            "repo" => Some(Self::Repo),
            _ => None,
        }
    }

    fn command_name(self) -> &'static str {
        match self {
            Self::Checkout => "checkout",
            Self::Crew => "crew",
            Self::Environment => "environment",
            Self::Host => "host",
            Self::Issue => "issue",
            Self::Repo => "repo",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
pub struct SubjectArgs {
    /// Subject name; prefix with `@` when it matches a command
    #[arg(value_name = "SUBJECT", conflicts_with = "explicit_subject")]
    pub subject: Option<String>,
    /// Literal subject; use for external names beginning with `@`
    #[arg(long = "subject", value_name = "SUBJECT", conflicts_with = "subject")]
    pub explicit_subject: Option<String>,
}

impl SubjectArgs {
    pub fn positional(subject: String) -> Self {
        Self { subject: Some(subject), explicit_subject: None }
    }

    pub(crate) fn resolve(self) -> Result<Option<ResolvedSubject>, String> {
        match (self.subject, self.explicit_subject) {
            (Some(_), Some(_)) => Err("subject may be supplied either positionally or with --subject, not both".to_string()),
            (None, Some(value)) => Ok(Some(ResolvedSubject { value, interpretation: SubjectInterpretation::Forced })),
            (Some(value), None) => {
                if let Some(value) = value.strip_prefix(ADDRESS_MARKER) {
                    if value.is_empty() {
                        return Err("the @ address marker must be followed by a subject name".to_string());
                    }
                    Ok(Some(ResolvedSubject { value: value.to_string(), interpretation: SubjectInterpretation::Forced }))
                } else {
                    Ok(Some(ResolvedSubject { value, interpretation: SubjectInterpretation::Ordinary }))
                }
            }
            (None, None) => Ok(None),
        }
    }

    pub(crate) fn write(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(subject) = &self.subject {
            write!(f, " {}", quote_value(subject))?;
        }
        if let Some(subject) = &self.explicit_subject {
            write!(f, " --subject {}", quote_value(subject))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubjectInterpretation {
    Ordinary,
    Forced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSubject {
    pub value: String,
    pub interpretation: SubjectInterpretation,
}

/// Render a dynamic subject completion in a form that remains addressable.
///
/// A leading `@` belongs to an opaque external identifier, so use the literal
/// `--subject` form. Tokens colliding with verbs or routable nouns use the
/// concise address marker.
pub fn address_subject_for_cli(noun: SubjectNoun, subject: &str) -> String {
    if subject.starts_with(ADDRESS_MARKER) {
        return format!("--subject {}", quote_value(subject));
    }
    if is_reserved_subject_token(noun, subject) {
        format!("{ADDRESS_MARKER}{subject}")
    } else {
        subject.to_string()
    }
}

pub fn subject_parse_hint(error: &clap::Error) -> Option<&'static str> {
    if error.kind() != ErrorKind::UnknownArgument {
        return None;
    }
    let Some(ContextValue::String(argument)) = error.get(ContextKind::InvalidArg) else {
        return None;
    };
    if argument.starts_with('-') {
        return None;
    }
    Some("If this value is a subject whose name matches a command, prefix it with `@`; use `--subject <literal>` for an external name beginning with `@`.")
}

pub(crate) fn format_parse_error(error: clap::Error) -> String {
    let hint = subject_parse_hint(&error);
    let mut message = error.to_string();
    if let Some(hint) = hint {
        message.push('\n');
        message.push_str(hint);
    }
    message
}

fn is_reserved_subject_token(noun: SubjectNoun, subject: &str) -> bool {
    if noun == SubjectNoun::Crew && is_crew_command_subject(subject) {
        return true;
    }

    if noun == SubjectNoun::Host {
        return host_command_tree().find_subcommand(subject).is_some() || is_noun_token(subject_command_tree(), subject);
    }

    let root = subject_command_tree();
    let noun = noun.command_name();
    let Some(command) =
        root.get_subcommands().find(|command| command.get_name() == noun || command.get_all_aliases().any(|alias| alias == noun))
    else {
        return false;
    };
    command.find_subcommand(subject).is_some() || (command.is_allow_external_subcommands_set() && is_noun_token(root, subject))
}

pub(crate) fn is_crew_command_subject(subject: &str) -> bool {
    CREW_COMMAND_SUBJECTS.contains(&subject)
}

fn subject_command_tree() -> &'static clap::Command {
    static TREE: OnceLock<clap::Command> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut command = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("subjects"));
        command.build();
        command
    })
}

fn host_command_tree() -> &'static clap::Command {
    static TREE: OnceLock<clap::Command> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut command = HostNounPartial::command();
        command.build();
        command
    })
}

fn is_noun_token(root: &clap::Command, subject: &str) -> bool {
    let found =
        root.get_subcommands().any(|command| command.get_name() == subject || command.get_all_aliases().any(|alias| alias == subject));
    found
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{address_subject_for_cli, subject_parse_hint, SubjectNoun};
    use crate::commands::checkout::CheckoutNoun;

    #[test]
    fn address_form_is_only_added_when_needed() {
        assert_eq!(address_subject_for_cli(SubjectNoun::Checkout, "feature"), "feature");
        assert_eq!(address_subject_for_cli(SubjectNoun::Checkout, "status"), "@status");
        assert_eq!(address_subject_for_cli(SubjectNoun::Host, "repo"), "@repo");
        assert_eq!(address_subject_for_cli(SubjectNoun::Crew, "complete"), "@complete");
        assert_eq!(address_subject_for_cli(SubjectNoun::Checkout, "@topic"), "--subject @topic");
    }

    #[test]
    fn positional_collision_parse_errors_explain_the_address_marker() {
        let error = CheckoutNoun::try_parse_from(["checkout", "status", "status"]).expect_err("ambiguous subject should fail");
        let hint = subject_parse_hint(&error).expect("subject collision hint");
        assert!(hint.contains("@"));
        assert!(hint.contains("--subject"));
    }

    #[test]
    fn unknown_flags_do_not_get_a_subject_hint() {
        let error = CheckoutNoun::try_parse_from(["checkout", "--bogus"]).expect_err("unknown flag should fail");
        assert!(subject_parse_hint(&error).is_none());
    }
}
