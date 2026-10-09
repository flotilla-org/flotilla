use std::time::Duration;

use color_eyre::Result;
use flotilla_protocol::{commands::CommandValue, output::OutputFormat, Command, CommandAction};

use flotilla_tui::cli::args::{Cli, CliPaths};

use super::daemon::{connect_cli_socket, connect_daemon, host_daemon_socket_required, reset_sigpipe};
use super::targets::resolve_optional_host_node;

pub(crate) async fn run_status(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let endpoint = cli.daemon_endpoint().map_err(|e| color_eyre::eyre::eyre!(e))?;
    flotilla_tui::cli::run_status(&endpoint, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

pub(crate) async fn run_watch(cli: &Cli, format: OutputFormat) -> Result<()> {
    reset_sigpipe();
    let endpoint = cli.daemon_endpoint().map_err(|e| color_eyre::eyre::eyre!(e))?;
    flotilla_tui::cli::run_watch(&endpoint, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

pub(crate) async fn run_wait(
    cli: &Cli,
    leaves: Vec<flotilla_protocol::Leaf>,
    namespace: String,
    freshness_demand: Option<chrono::DateTime<chrono::Utc>>,
    timeout_seconds: Option<u64>,
    format: OutputFormat,
) -> Result<()> {
    reset_sigpipe();
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let remote = cli.remote_daemon().map_err(|error| color_eyre::eyre::eyre!(error))?;
    let daemon = connect_cli_socket(
        remote.as_ref(),
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os(flotilla_core::providers::environment::CONTAINED_DAEMON_REQUIRED_ENV).as_deref()),
    )
    .await
    .map_err(|error| color_eyre::eyre::eyre!(error))?;
    let request = flotilla_protocol::WaitSubscriptionRequest { namespace, leaves, freshness_demand };
    let (subscription_id, mut events) = daemon.subscribe_wait(request).await.map_err(|error| color_eyre::eyre::eyre!(error))?;
    let wait = async move {
        loop {
            match events.recv().await {
                Ok(fire) if fire.subscription_id == subscription_id => break Ok(fire),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    break Err(color_eyre::eyre::eyre!("wait event stream lagged by {skipped} event(s); condition may have fired"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break Err(color_eyre::eyre::eyre!("daemon restarted"));
                }
            }
        }
    };
    let fire = match timeout_seconds {
        Some(seconds) => tokio::time::timeout(Duration::from_secs(seconds), wait)
            .await
            .map_err(|_| color_eyre::eyre::eyre!("timed out waiting for condition after {seconds}s"))??,
        None => wait.await?,
    };
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&fire)?),
        OutputFormat::Human => println!("condition fired: {} (value: {})", fire.leaf, fire.value),
    }
    Ok(())
}

pub(crate) async fn run_topology_command(cli: &Cli, format: OutputFormat, dot: bool) -> Result<()> {
    reset_sigpipe();
    let format = topology_output_format(format, dot).map_err(|e| color_eyre::eyre::eyre!(e))?;
    let daemon = connect_daemon(cli).await?;
    flotilla_tui::cli::run_topology(&*daemon, format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

fn topology_output_format(format: OutputFormat, dot: bool) -> Result<flotilla_tui::cli::TopologyOutputFormat, String> {
    match (format, dot) {
        (OutputFormat::Json, true) => Err("--dot cannot be used with --json".to_string()),
        (_, true) => Ok(flotilla_tui::cli::TopologyOutputFormat::Dot),
        (format, false) => Ok(format.into()),
    }
}

pub(crate) async fn run_logs(
    cli: &Cli,
    host: Option<&str>,
    since: Option<Duration>,
    level: Option<String>,
    target: Option<String>,
) -> Result<()> {
    reset_sigpipe();
    let node_id = resolve_optional_host_node(cli, host).await?;
    let daemon = connect_daemon(cli).await?;
    let result = daemon
        .execute_query(
            Command {
                node_id,
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::QueryDaemonLogs {
                    query: flotilla_protocol::commands::DaemonLogQuery {
                        since_seconds: since.map(|duration| duration.as_secs()),
                        level,
                        target,
                    },
                },
            },
            uuid::Uuid::new_v4(),
        )
        .await
        .map_err(|error| color_eyre::eyre::eyre!(error))?;
    match result {
        CommandValue::DaemonLogs { lines } => {
            for line in lines {
                println!("{line}");
            }
            Ok(())
        }
        CommandValue::Error { message } => Err(color_eyre::eyre::eyre!(message)),
        other => Err(color_eyre::eyre::eyre!("unexpected daemon logs response: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use flotilla_tui::cli::args::{SubCommand, TopologyArgs};
    #[test]
    fn cli_rejects_topology_dot_with_json() {
        assert!(Cli::try_parse_from(["flotilla", "topology", "--dot", "--json"]).is_err());

        let cli = Cli::try_parse_from(["flotilla", "--json", "topology", "--dot"]).expect("clap accepts this global flag order");
        let Some(SubCommand::Topology(TopologyArgs { dot })) = cli.command else {
            panic!("expected topology command");
        };
        assert_eq!(
            topology_output_format(OutputFormat::from_json_flag(cli.json), dot),
            Err("--dot cannot be used with --json".to_string())
        );
    }
}
