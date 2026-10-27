use super::connection::{connect_cli_socket, host_daemon_socket_required};
use crate::cli::args::{ArchiveSubCommand, Cli, CliPaths};
use color_eyre::Result;
use flotilla_protocol::output::OutputFormat;

pub async fn run_archive_command(cli: &Cli, command: ArchiveSubCommand, format: OutputFormat) -> Result<()> {
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        remote.as_ref(),
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os("FLOTILLA_CONTAINED_HOST_DAEMON").as_deref()),
    )
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    match command {
        ArchiveSubCommand::Show { kind, name, home } => {
            let resource = daemon.archive_read(kind, name, home).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&resource)?),
                OutputFormat::Human => print!("{}", human(&resource)),
            }
        }
    }
    Ok(())
}

/// The resource's identity and bundle key, its rows as YAML, then one line per entry.
fn human(resource: &serde_json::Value) -> String {
    let text = |value: &serde_json::Value| value.as_str().map_or_else(|| value.to_string(), str::to_owned);
    let mut out = format!(
        "{} {} (home {}, version {})\nbundle {}\n",
        text(&resource["kind"]),
        text(&resource["name"]),
        text(&resource["home"]),
        resource["version"],
        text(&resource["key"])
    );
    out.push_str(&serde_yml::to_string(&resource["tables"]).unwrap_or_default());
    out.push_str("entries:\n");
    for entry in resource["entries"].as_array().into_iter().flatten() {
        let edge = match entry.get("edge") {
            Some(edge) => format!(" {} {} -> {}", text(edge), text(&entry["from"]), text(&entry["to"])),
            None => String::new(),
        };
        out.push_str(&format!("  {} {} {}{edge} by {}\n", entry["seq"], text(&entry["at"]), text(&entry["cause"]), text(&entry["writer"])));
    }
    out
}

#[cfg(test)]
mod tests {
    // The human view names the resource and lists each entry with its edge and labels.
    #[test]
    fn human_view_lists_the_entries() {
        let resource = serde_json::json!({
            "kind": "Message", "name": "m", "home": "alpha", "version": 2, "key": "archive/alpha/ms/m",
            "tables": {"message": [{"state": "dropped"}]},
            "entries": [
                {"seq": 1, "at": "t1", "cause": "create", "writer": "alpha"},
                {"seq": 2, "at": "t2", "cause": "edge", "writer": "alpha", "edge": "drop", "from": "Pending", "to": "Dropped"},
            ],
        });
        let view = super::human(&resource);
        assert!(view.starts_with("Message m (home alpha, version 2)\nbundle archive/alpha/ms/m\n"), "{view}");
        assert!(view.contains("state: dropped"), "{view}");
        assert!(view.ends_with("entries:\n  1 t1 create by alpha\n  2 t2 edge drop Pending -> Dropped by alpha\n"), "{view}");
    }
}
