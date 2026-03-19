use clap::{CommandFactory, Parser};
use cleat::{
    cli::{Cli, Command},
    vt::VtEngineKind,
};

#[test]
fn help_lists_expected_subcommands() {
    let command = Cli::command();
    let subcommands: Vec<_> = command.get_subcommands().filter(|sub| !sub.is_hide_set()).map(|sub| sub.get_name().to_string()).collect();
    assert_eq!(subcommands, vec!["attach", "create", "list", "capture", "detach", "kill"]);
}

#[test]
fn attach_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "attach", "demo"]).expect("attach positional parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: false, vt: None, cwd: None, cmd: None });
}

#[test]
fn attach_command_parses_no_create() {
    let cli = Cli::try_parse_from(["cleat", "attach", "--no-create", "demo"]).expect("attach --no-create parses");
    assert_eq!(cli.command, Command::Attach { id: Some("demo".into()), no_create: true, vt: None, cwd: None, cmd: None });
}

#[test]
fn attach_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "attach", "--vt", "passthrough", "demo"]).expect("attach --vt parses");
    assert_eq!(cli.command, Command::Attach {
        id: Some("demo".into()),
        no_create: false,
        vt: Some(VtEngineKind::Passthrough),
        cwd: None,
        cmd: None
    });
}

#[test]
fn create_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "create", "--cmd", "bash"]).expect("create parses");
    assert_eq!(cli.command, Command::Create { id: None, json: false, vt: None, cwd: None, cmd: Some("bash".into()) });
}

#[test]
fn create_command_parses_positional_name() {
    let cli = Cli::try_parse_from(["cleat", "create", "demo", "--cmd", "bash"]).expect("create positional parses");
    assert_eq!(cli.command, Command::Create { id: Some("demo".into()), json: false, vt: None, cwd: None, cmd: Some("bash".into()) });
}

#[test]
fn create_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "create", "--json", "demo"]).expect("create --json parses");
    assert_eq!(cli.command, Command::Create { id: Some("demo".into()), json: true, vt: None, cwd: None, cmd: None });
}

#[test]
fn create_command_parses_vt() {
    let cli = Cli::try_parse_from(["cleat", "create", "--vt", "ghostty", "demo"]).expect("create --vt parses");
    assert_eq!(cli.command, Command::Create {
        id: Some("demo".into()),
        json: false,
        vt: Some(VtEngineKind::Ghostty),
        cwd: None,
        cmd: None
    });
}

#[test]
fn list_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "list"]).expect("list parses");
    assert_eq!(cli.command, Command::List { json: false });
}

#[test]
fn list_command_parses_json() {
    let cli = Cli::try_parse_from(["cleat", "list", "--json"]).expect("list --json parses");
    assert_eq!(cli.command, Command::List { json: true });
}

#[test]
fn capture_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "capture", "session-1"]).expect("capture parses");
    assert_eq!(cli.command, Command::Capture { id: "session-1".into() });
}

#[test]
fn detach_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "detach", "session-1"]).expect("detach parses");
    assert_eq!(cli.command, Command::Detach { id: "session-1".into() });
}

#[test]
fn kill_command_parses() {
    let cli = Cli::try_parse_from(["cleat", "kill", "session-1"]).expect("kill parses");
    assert_eq!(cli.command, Command::Kill { id: "session-1".into() });
}
