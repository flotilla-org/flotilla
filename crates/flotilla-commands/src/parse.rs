use clap::{Command as ClapCommand, FromArgMatches, Parser, Subcommand};

use crate::{
    commands::host::HostNounPartial,
    noun::NounCommand,
    resolved::{Refinable, Resolved},
};

pub fn parse_noun_command(tokens: &[&str]) -> Result<NounCommand, String> {
    let cmd = <NounCommand as Subcommand>::augment_subcommands(ClapCommand::new("flotilla").no_binary_name(true));
    let matches = cmd.try_get_matches_from(tokens).map_err(|e| e.to_string())?;
    <NounCommand as FromArgMatches>::from_arg_matches(&matches).map_err(|e| e.to_string())
}

pub fn parse_host_command(tokens: &[&str]) -> Result<Resolved, String> {
    let partial = HostNounPartial::try_parse_from(tokens).map_err(|e| e.to_string())?;
    partial.refine()?.resolve()
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::CommandAction;

    use super::*;

    #[test]
    fn parse_cr_open() {
        let noun = parse_noun_command(&["cr", "42", "open"]).unwrap();
        let resolved = noun.resolve().unwrap();
        assert!(matches!(resolved, Resolved::NeedsContext { ref command, .. }
            if matches!(command.action, CommandAction::OpenChangeRequest { .. })));
    }

    #[test]
    fn parse_unknown_noun_errors() {
        assert!(parse_noun_command(&["bogus", "verb"]).is_err());
    }

    #[test]
    fn parse_host_routed_command() {
        let resolved = parse_host_command(&["host", "feta", "cr", "42", "open"]).unwrap();
        match &resolved {
            Resolved::NeedsContext { command, .. } => {
                assert!(command.host.is_some());
            }
            _ => panic!("expected NeedsContext"),
        }
    }
}
