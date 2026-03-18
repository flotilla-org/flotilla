use clap::{CommandFactory, Parser};
use bollard::cli::{Cli, Command};

#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert_eq!(subcommands, vec!["attach", "create", "list", "kill"]);
}

#[test]
fn attach_command_parses() {
    let cli = Cli::try_parse_from(["bollard", "attach", "demo"]).expect("attach positional parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: false, cwd: None, cmd: None });
}

#[test]
fn attach_command_parses_no_create() {
    let cli = Cli::try_parse_from(["bollard", "attach", "--no-create", "demo"]).expect("attach --no-create parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: true, cwd: None, cmd: None });
}

#[test]
fn create_command_parses() {
    let cli = Cli::try_parse_from(["bollard", "create", "--cmd", "bash"]).expect("create parses");
    assert_eq!(cli.command, Command::Create { id: None, json: false, cwd: None, cmd: Some("bash".into()) });
}

#[test]
fn create_command_parses_positional_name() {
    let cli = Cli::try_parse_from(["bollard", "create", "demo", "--cmd", "bash"]).expect("create positional parses");
    assert_eq!(cli.command, Command::Create { id: Some("demo".into()), json: false, cwd: None, cmd: Some("bash".into()) });
}

#[test]
fn create_command_parses_json() {
    let cli = Cli::try_parse_from(["bollard", "create", "--json", "demo"]).expect("create --json parses");
    assert_eq!(cli.command, Command::Create { id: Some("demo".into()), json: true, cwd: None, cmd: None });
}

#[test]
fn list_command_parses() {
    let cli = Cli::try_parse_from(["bollard", "list"]).expect("list parses");
    assert_eq!(cli.command, Command::List { json: false });
}

#[test]
fn list_command_parses_json() {
    let cli = Cli::try_parse_from(["bollard", "list", "--json"]).expect("list --json parses");
    assert_eq!(cli.command, Command::List { json: true });
}

#[test]
fn kill_command_parses() {
    let cli = Cli::try_parse_from(["bollard", "kill", "session-1"]).expect("kill parses");
    assert_eq!(cli.command, Command::Kill { id: "session-1".into() });
}
