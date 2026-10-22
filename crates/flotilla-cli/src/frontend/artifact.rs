use super::connection::{connect_cli_socket, host_daemon_socket_required};
use crate::cli::args::{ArtifactSubCommand, Cli, CliPaths};
use color_eyre::Result;
use flotilla_protocol::output::OutputFormat;
use std::path::PathBuf;

pub async fn run_artifact_command(cli: &Cli, command: ArtifactSubCommand, format: OutputFormat) -> Result<()> {
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
        ArtifactSubCommand::Put { kind, about, summary, file } => {
            if about.is_none() && kind != "decision-ledger" {
                return Err(color_eyre::eyre::eyre!("--about is required except for decision-ledger"));
            }
            let source_path = tokio::fs::canonicalize(&file).await?;
            let media_type = match file.extension().and_then(|value| value.to_str()).unwrap_or("") {
                "json" => "application/json",
                "md" => "text/markdown",
                "html" | "htm" => "text/html",
                "txt" | "log" => "text/plain",
                "yaml" | "yml" => "application/yaml",
                _ => "application/octet-stream",
            };
            let summary = summary.into_iter().collect();
            let (address, digest, view_url) = daemon
                .artifact_put(kind, about.unwrap_or_default(), summary, media_type.to_string(), source_path)
                .await
                .map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => {
                    let mut value = serde_json::json!({"address": address, "digest": digest});
                    if let Some(url) = view_url {
                        value["view_url"] = url.into();
                    }
                    println!("{value}");
                }
                OutputFormat::Human => {
                    println!("{address}\n{digest}");
                    if let Some(url) = view_url {
                        println!("{url}");
                    }
                }
            }
        }
        ArtifactSubCommand::Get { reference, output } => {
            let path = output.unwrap_or_else(|| PathBuf::from(reference.rsplit('/').next().unwrap_or(&reference)));
            let destination = if path.is_absolute() { path.clone() } else { std::env::current_dir()?.join(&path) };
            let (size, view_url) =
                daemon.artifact_get(reference.clone(), destination).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => {
                    let mut value = serde_json::json!({"path": path, "size": size, "address": reference});
                    if let Some(url) = view_url {
                        value["view_url"] = url.into();
                    }
                    println!("{value}");
                }
                OutputFormat::Human => {
                    println!("{}\n{reference}", path.display());
                    if let Some(url) = view_url {
                        println!("{url}");
                    }
                }
            }
        }
        ArtifactSubCommand::List { convoy, kind, about } => {
            let items = daemon.artifact_list(convoy, kind, about).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&items)?),
                OutputFormat::Human => {
                    for item in items {
                        println!(
                            "artifact/{}\t{}\t{}\t{}\t{}\t{}\t{}",
                            item["metadata"]["name"].as_str().unwrap_or("?"),
                            item["spec"]["convoy"].as_str().unwrap_or("?"),
                            item["spec"]["producer"].as_str().unwrap_or("?"),
                            item["spec"]["kind"].as_str().unwrap_or("?"),
                            item["spec"]["subject"].as_str().unwrap_or("?"),
                            item["spec"]["digest"].as_str().unwrap_or("?"),
                            item["view_url"].as_str().unwrap_or(""),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
