use clap::{
    error::{ContextKind, ContextValue, ErrorKind},
    CommandFactory, Subcommand,
};

use crate::{commands::host::HostNounPartial, noun::NounCommand, quote_value};

pub const ADDRESS_MARKER: char = '@';

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedSubject {
    pub value: String,
    pub forced: bool,
}

pub(crate) fn resolve_subject(positional: Option<String>, explicit: Option<String>) -> Result<Option<ResolvedSubject>, String> {
    match (positional, explicit) {
        (Some(_), Some(_)) => Err("subject may be supplied either positionally or with --subject, not both".to_string()),
        (None, Some(value)) => Ok(Some(ResolvedSubject { value, forced: true })),
        (Some(value), None) => {
            if let Some(value) = value.strip_prefix(ADDRESS_MARKER) {
                if value.is_empty() {
                    return Err("the @ address marker must be followed by a subject name".to_string());
                }
                Ok(Some(ResolvedSubject { value: value.to_string(), forced: true }))
            } else {
                Ok(Some(ResolvedSubject { value, forced: false }))
            }
        }
        (None, None) => Ok(None),
    }
}

pub(crate) fn write_subject(f: &mut std::fmt::Formatter<'_>, positional: Option<&String>, explicit: Option<&String>) -> std::fmt::Result {
    if let Some(subject) = positional {
        write!(f, " {}", quote_value(subject))?;
    }
    if let Some(subject) = explicit {
        write!(f, " --subject {}", quote_value(subject))?;
    }
    Ok(())
}

/// Render a dynamic subject completion in a form that remains addressable.
///
/// A leading `@` belongs to an opaque external identifier, so use the literal
/// `--subject` form. Tokens colliding with verbs or routable nouns use the
/// concise address marker.
pub fn address_subject_for_cli(noun: &str, subject: &str) -> String {
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

fn is_reserved_subject_token(noun: &str, subject: &str) -> bool {
    if noun == "crew" && matches!(subject, "list" | "complete" | "fail") {
        return true;
    }

    if noun == "host" {
        let mut command = HostNounPartial::command();
        command.build();
        if command.find_subcommand(subject).is_some() {
            return true;
        }
        return is_noun_token(subject);
    }

    let root = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("subjects"));
    let Some(command) =
        root.get_subcommands().find(|command| command.get_name() == noun || command.get_all_aliases().any(|alias| alias == noun))
    else {
        return false;
    };
    command.find_subcommand(subject).is_some() || (command.is_allow_external_subcommands_set() && is_noun_token(subject))
}

fn is_noun_token(subject: &str) -> bool {
    let root = <NounCommand as Subcommand>::augment_subcommands(clap::Command::new("subjects"));
    let found =
        root.get_subcommands().any(|command| command.get_name() == subject || command.get_all_aliases().any(|alias| alias == subject));
    found
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{address_subject_for_cli, subject_parse_hint};
    use crate::commands::checkout::CheckoutNoun;

    #[test]
    fn address_form_is_only_added_when_needed() {
        assert_eq!(address_subject_for_cli("checkout", "feature"), "feature");
        assert_eq!(address_subject_for_cli("checkout", "status"), "@status");
        assert_eq!(address_subject_for_cli("host", "repo"), "@repo");
        assert_eq!(address_subject_for_cli("crew", "complete"), "@complete");
        assert_eq!(address_subject_for_cli("checkout", "@topic"), "--subject @topic");
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
