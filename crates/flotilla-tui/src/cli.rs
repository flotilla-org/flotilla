use std::path::Path;

use flotilla_core::daemon::DaemonHandle;

use crate::socket::SocketDaemon;

pub async fn run_status(socket_path: &Path) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path)
        .await
        .map_err(|e| format!("cannot connect to daemon: {e}"))?;

    let repos = daemon.list_repos().await.map_err(|e| e.to_string())?;

    if repos.is_empty() {
        println!("No repos tracked.");
        return Ok(());
    }

    for repo in &repos {
        let name = &repo.name;
        let path = repo.path.display();
        let health: Vec<String> = repo
            .provider_health
            .iter()
            .flat_map(|(category, providers)| {
                providers.iter().map(move |(name, v)| {
                    format!("{category}/{name}: {}", if *v { "ok" } else { "error" })
                })
            })
            .collect();
        let loading = if repo.loading { " (loading)" } else { "" };
        println!("{name}{loading}  {path}");
        if !health.is_empty() {
            println!("  providers: {}", health.join(", "));
        }
    }

    Ok(())
}

pub async fn run_watch(socket_path: &Path) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path)
        .await
        .map_err(|e| format!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();
    println!("watching events (Ctrl-C to stop)...");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let json =
                    serde_json::to_string_pretty(&event).unwrap_or_else(|_| format!("{event:?}"));
                println!("{json}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(_) => {
                eprintln!("daemon disconnected");
                break;
            }
        }
    }

    Ok(())
}
