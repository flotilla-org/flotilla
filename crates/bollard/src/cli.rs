use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};

use crate::server::SessionService;

#[derive(Debug, Parser)]
#[command(name = "bollard", version)]
pub struct Cli {
    #[arg(long, hide = true)]
    pub runtime_root: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    Attach {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long)]
        no_create: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cmd: Option<String>,
    },
    Create {
        #[arg(value_name = "ID")]
        id: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        cmd: Option<String>,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Kill {
        id: String,
    },
    #[command(hide = true)]
    Serve {
        #[arg(long)]
        id: String,
    },
}

pub fn parse() -> Cli {
    Cli::parse()
}

pub fn command() -> clap::Command {
    Cli::command()
}

pub fn execute(cli: Cli, service: &SessionService) -> Result<Option<String>, String> {
    match cli.command {
        Command::Attach { id, no_create, cwd, cmd } => {
            let (_attached, guard) = service.attach(id, cwd, cmd, no_create)?;
            guard.relay_stdio()?;
            Ok(None)
        }
        Command::Create { id, json, cwd, cmd } => {
            let created = service.create(id, cwd, cmd)?;
            if json {
                serde_json::to_string(&created).map(Some).map_err(|err| format!("serialize create result: {err}"))
            } else {
                Ok(Some(created.id))
            }
        }
        Command::List { json } => {
            let sessions = service.list()?;
            if json {
                serde_json::to_string(&sessions).map(Some).map_err(|err| format!("serialize list result: {err}"))
            } else if sessions.is_empty() {
                Ok(None)
            } else {
                Ok(Some(sessions.iter().map(format_session_human).collect::<Vec<_>>().join("\n")))
            }
        }
        Command::Kill { id } => {
            service.kill(&id)?;
            Ok(None)
        }
        Command::Serve { id } => {
            service.serve(&id)?;
            Ok(None)
        }
    }
}

fn format_session_human(session: &crate::protocol::SessionInfo) -> String {
    let mut fields = vec![session.id.clone(), format_session_status(&session.status).to_string()];
    if let Some(cwd) = &session.cwd {
        fields.push(cwd.display().to_string());
    } else if let Some(cmd) = &session.cmd {
        fields.push(cmd.clone());
    }
    fields.join("\t")
}

fn format_session_status(status: &crate::protocol::SessionStatus) -> &'static str {
    match status {
        crate::protocol::SessionStatus::Attached => "attached",
        crate::protocol::SessionStatus::Detached => "detached",
    }
}
