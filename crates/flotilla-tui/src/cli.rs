use std::path::Path;

use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, Table};
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{
    output::OutputFormat, Command, CommandResult, DaemonEvent, RepoDetailResponse, RepoProvidersResponse, RepoWorkResponse, StatusResponse,
};

use crate::socket::SocketDaemon;

fn format_work_items_table(items: &[flotilla_protocol::snapshot::WorkItem]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Kind", "Branch", "Description", "PR", "Session", "Issues"]);
    for item in items {
        table.add_row(vec![
            Cell::new(format!("{:?}", item.kind)),
            Cell::new(item.branch.as_deref().unwrap_or("-")),
            Cell::new(&item.description),
            Cell::new(item.change_request_key.as_deref().unwrap_or("-")),
            Cell::new(item.session_key.as_deref().unwrap_or("-")),
            Cell::new(if item.issue_keys.is_empty() { "-".into() } else { item.issue_keys.join(", ") }),
        ]);
    }
    table
}

fn format_status_response_human(status: &StatusResponse) -> String {
    if status.repos.is_empty() {
        return "No repos tracked.\n".into();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Repo", "Path", "Work Items", "Errors", "Health"]);
    for repo in &status.repos {
        let name = repo.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut health: Vec<String> = repo
            .provider_health
            .iter()
            .flat_map(|(cat, providers)| {
                providers.iter().map(move |(name, ok)| format!("{cat}/{name}: {}", if *ok { "ok" } else { "error" }))
            })
            .collect();
        health.sort();
        let health_str = if health.is_empty() { "-".into() } else { health.join(", ") };
        table.add_row(vec![
            Cell::new(&name),
            Cell::new(repo.path.display()),
            Cell::new(repo.work_item_count),
            Cell::new(repo.error_count),
            Cell::new(&health_str),
        ]);
    }
    format!("{table}\n")
}

/// Extract a short display name from a repo path (last path component).
/// Falls back to the full path display for root or non-UTF-8 paths,
/// matching `flotilla_core::model::repo_name`.
fn repo_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Format a `CommandResult` as a short human-readable string.
fn format_command_result(result: &flotilla_protocol::commands::CommandResult) -> String {
    use flotilla_protocol::commands::CommandResult;
    match result {
        CommandResult::Ok => "ok".to_string(),
        CommandResult::RepoAdded { path } => format!("repo added: {}", path.display()),
        CommandResult::RepoRemoved { path } => format!("repo removed: {}", path.display()),
        CommandResult::Refreshed { repos } => format!("refreshed {} repo(s)", repos.len()),
        CommandResult::CheckoutCreated { branch, .. } => format!("checkout created: {branch}"),
        CommandResult::CheckoutRemoved { branch } => format!("checkout removed: {branch}"),
        CommandResult::BranchNameGenerated { name, .. } => format!("branch name: {name}"),
        CommandResult::CheckoutStatus(_) => "checkout status received".to_string(),
        CommandResult::Error { message } => format!("error: {message}"),
        CommandResult::Cancelled => "cancelled".to_string(),
    }
}

pub(crate) fn format_event_human(event: &flotilla_protocol::DaemonEvent) -> String {
    use flotilla_protocol::{DaemonEvent, PeerConnectionState};
    match event {
        DaemonEvent::SnapshotFull(snap) => {
            format!("[snapshot] {}: full snapshot (seq {}, {} work items)", repo_name(&snap.repo), snap.seq, snap.work_items.len())
        }
        DaemonEvent::SnapshotDelta(delta) => {
            format!(
                "[delta]    {}: delta seq {}\u{2192}{} ({} changes)",
                repo_name(&delta.repo),
                delta.prev_seq,
                delta.seq,
                delta.changes.len()
            )
        }
        DaemonEvent::RepoAdded(info) => {
            format!("[repo]     {}: added", info.name)
        }
        DaemonEvent::RepoRemoved { path } => {
            format!("[repo]     {}: removed", repo_name(path))
        }
        DaemonEvent::CommandStarted { repo, description, .. } => {
            format!("[command]  {}: started \"{}\"", repo_name(repo), description)
        }
        DaemonEvent::CommandFinished { repo, result, .. } => {
            format!("[command]  {}: finished \u{2192} {}", repo_name(repo), format_command_result(result))
        }
        DaemonEvent::CommandStepUpdate { repo, description, step_index, step_count, .. } => {
            format!("[step]     {}: {} ({}/{})", repo_name(repo), description, step_index + 1, step_count)
        }
        DaemonEvent::PeerStatusChanged { host, status } => {
            let state = match status {
                PeerConnectionState::Connected => "connected",
                PeerConnectionState::Disconnected => "disconnected",
                PeerConnectionState::Connecting => "connecting",
                PeerConnectionState::Reconnecting => "reconnecting",
            };
            format!("[peer]     {host}: {state}")
        }
    }
}

pub async fn run_status(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;
    let status = daemon.get_status().await?;
    let output = match format {
        OutputFormat::Human => format_status_response_human(&status),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&status),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_repo_detail(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let detail = daemon.get_repo_detail(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_detail_human(&detail),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&detail),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_repo_providers(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let providers = daemon.get_repo_providers(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_providers_human(&providers),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&providers),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_repo_work(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let work = daemon.get_repo_work(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_work_human(&work),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&work),
    };
    print!("{output}");
    Ok(())
}

fn format_repo_detail_human(detail: &RepoDetailResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", detail.path.display()));
    if let Some(slug) = &detail.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if !detail.work_items.is_empty() {
        let table = format_work_items_table(&detail.work_items);
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !detail.errors.is_empty() {
        out.push_str("\nErrors:\n");
        for err in &detail.errors {
            out.push_str(&format!("  [{}/{}] {}\n", err.category, err.provider, err.message));
        }
    }
    out
}

fn format_repo_providers_human(resp: &RepoProvidersResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }

    if !resp.host_discovery.is_empty() {
        out.push_str("\nHost Discovery:\n");
        for entry in &resp.host_discovery {
            let mut details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            details.sort();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.repo_discovery.is_empty() {
        out.push_str("\nRepo Discovery:\n");
        for entry in &resp.repo_discovery {
            let mut details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            details.sort();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.providers.is_empty() {
        out.push_str("\nProviders:\n");
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Category", "Name", "Health"]);
        for p in &resp.providers {
            table.add_row(vec![Cell::new(&p.category), Cell::new(&p.name), Cell::new(if p.healthy { "ok" } else { "error" })]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !resp.unmet_requirements.is_empty() {
        out.push_str("\nUnmet Requirements:\n");
        for ur in &resp.unmet_requirements {
            out.push_str(&format!("  {}: {}\n", ur.factory, ur.requirement));
        }
    }
    out
}

fn format_repo_work_human(resp: &RepoWorkResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if resp.work_items.is_empty() {
        out.push_str("No work items.\n");
    } else {
        let table = format_work_items_table(&resp.work_items);
        out.push_str(&table.to_string());
        out.push('\n');
    }
    out
}

pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();

    if matches!(format, OutputFormat::Human) {
        eprintln!("watching events (Ctrl-C to stop)...");
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                let line = match format {
                    OutputFormat::Human => format_event_human(&event),
                    OutputFormat::Json => flotilla_protocol::output::json_line(&event),
                };
                println!("{line}");
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

pub async fn run_command(daemon: &dyn DaemonHandle, command: Command, format: OutputFormat) -> Result<(), String> {
    let mut rx = daemon.subscribe();
    let command_id = daemon.execute(command).await?;

    loop {
        match rx.recv().await {
            Ok(event @ DaemonEvent::CommandStarted { command_id: id, .. }) if id == command_id => {
                if matches!(format, OutputFormat::Human) {
                    println!("{}", format_event_human(&event));
                }
            }
            Ok(event @ DaemonEvent::CommandStepUpdate { command_id: id, .. }) if id == command_id => {
                if matches!(format, OutputFormat::Human) {
                    println!("{}", format_event_human(&event));
                }
            }
            Ok(ref event @ DaemonEvent::CommandFinished { command_id: id, ref result, .. }) if id == command_id => {
                match format {
                    OutputFormat::Human => {
                        println!("{}", format_event_human(&event));
                    }
                    OutputFormat::Json => {
                        println!("{}", flotilla_protocol::output::json_pretty(&result));
                    }
                }
                let result = result.clone();
                return match result {
                    CommandResult::Error { message } => Err(message),
                    CommandResult::Cancelled => Err("command cancelled".into()),
                    _ => Ok(()),
                };
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if matches!(format, OutputFormat::Human) {
                    eprintln!("warning: skipped {n} events");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err("daemon disconnected".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    fn health(entries: &[(&str, &str, bool)]) -> HashMap<String, HashMap<String, bool>> {
        let mut map: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for (cat, name, ok) in entries {
            map.entry(cat.to_string()).or_default().insert(name.to_string(), *ok);
        }
        map
    }

    mod status_human {
        use flotilla_protocol::{RepoSummary, StatusResponse};

        use super::*;
        use crate::cli::format_status_response_human;

        #[test]
        fn empty_repos() {
            let status = StatusResponse { repos: vec![] };
            assert_eq!(format_status_response_human(&status), "No repos tracked.\n");
        }

        #[test]
        fn single_repo_with_health() {
            let status = StatusResponse {
                repos: vec![RepoSummary {
                    path: PathBuf::from("/tmp/my-repo"),
                    slug: Some("org/my-repo".into()),
                    provider_health: health(&[("vcs", "Git", true)]),
                    work_item_count: 3,
                    error_count: 0,
                }],
            };
            let output = format_status_response_human(&status);
            assert!(output.contains("my-repo"), "should contain repo name");
            assert!(output.contains("3"), "should show work item count");
        }
    }

    mod watch_human {
        use std::path::PathBuf;

        use flotilla_protocol::{commands::CommandResult, snapshot::Snapshot, DaemonEvent, HostName, PeerConnectionState, SnapshotDelta};

        use crate::cli::format_event_human;

        fn dummy_snapshot(seq: u64, repo: &str, work_item_count: usize) -> Snapshot {
            use std::collections::HashMap;

            use flotilla_protocol::snapshot::{WorkItem, WorkItemIdentity, WorkItemKind};

            Snapshot {
                seq,
                repo: PathBuf::from(repo),
                host_name: HostName::new("test"),
                work_items: (0..work_item_count)
                    .map(|i| WorkItem {
                        kind: WorkItemKind::Checkout,
                        identity: WorkItemIdentity::Checkout(flotilla_protocol::HostPath::new(
                            HostName::new("test"),
                            PathBuf::from(format!("/tmp/wt{i}")),
                        )),
                        host: HostName::new("test"),
                        branch: None,
                        description: String::new(),
                        checkout: None,
                        change_request_key: None,
                        session_key: None,
                        issue_keys: vec![],
                        workspace_refs: vec![],
                        is_main_checkout: false,
                        debug_group: vec![],
                        source: None,
                        terminal_keys: vec![],
                    })
                    .collect(),
                providers: Default::default(),
                provider_health: HashMap::new(),
                errors: vec![],
                issue_total: None,
                issue_has_more: false,
                issue_search_results: None,
            }
        }

        #[test]
        fn snapshot_full() {
            let event = DaemonEvent::SnapshotFull(Box::new(dummy_snapshot(42, "/tmp/my-repo", 5)));
            let line = format_event_human(&event);
            assert!(line.contains("[snapshot]"), "should have snapshot tag");
            assert!(line.contains("my-repo"), "should extract repo name from path");
            assert!(line.contains("seq 42"), "should show seq");
            assert!(line.contains("5 work items"), "should show work item count");
        }

        #[test]
        fn snapshot_delta() {
            let event = DaemonEvent::SnapshotDelta(Box::new(SnapshotDelta {
                seq: 42,
                prev_seq: 41,
                repo: PathBuf::from("/tmp/my-repo"),
                changes: vec![],
                work_items: vec![],
                issue_total: None,
                issue_has_more: false,
                issue_search_results: None,
            }));
            let line = format_event_human(&event);
            assert!(line.contains("[delta]"), "should have delta tag");
            assert!(line.contains("41→42") || line.contains("41->42"), "should show prev→seq");
        }

        #[test]
        fn repo_added() {
            let event = DaemonEvent::RepoAdded(Box::new(flotilla_protocol::snapshot::RepoInfo {
                name: "added-repo".into(),
                path: PathBuf::from("/tmp/added-repo"),
                labels: Default::default(),
                provider_names: Default::default(),
                provider_health: Default::default(),
                loading: false,
            }));
            let line = format_event_human(&event);
            assert!(line.contains("[repo]"), "should have repo tag");
            assert!(line.contains("added-repo"), "should show repo name");
            assert!(line.contains("added"), "should say added");
        }

        #[test]
        fn repo_removed() {
            let event = DaemonEvent::RepoRemoved { path: PathBuf::from("/tmp/old-repo") };
            let line = format_event_human(&event);
            assert!(line.contains("[repo]"), "should have repo tag");
            assert!(line.contains("old-repo"), "should extract name");
            assert!(line.contains("removed"), "should say removed");
        }

        #[test]
        fn command_started() {
            let event = DaemonEvent::CommandStarted {
                command_id: 1,
                host: HostName::local(),
                repo: PathBuf::from("/tmp/my-repo"),
                description: "Refreshing...".into(),
            };
            let line = format_event_human(&event);
            assert!(line.contains("[command]"), "should have command tag");
            assert!(line.contains("started"), "should say started");
            assert!(line.contains("Refreshing..."), "should include description");
        }

        #[test]
        fn command_finished_ok() {
            let event = DaemonEvent::CommandFinished {
                command_id: 1,
                host: HostName::local(),
                repo: PathBuf::from("/tmp/my-repo"),
                result: CommandResult::Ok,
            };
            let line = format_event_human(&event);
            assert!(line.contains("[command]"), "should have command tag");
            assert!(line.contains("finished"), "should say finished");
            assert!(line.contains("ok"), "should show ok result");
        }

        #[test]
        fn command_finished_error() {
            let event = DaemonEvent::CommandFinished {
                command_id: 1,
                host: HostName::local(),
                repo: PathBuf::from("/tmp/my-repo"),
                result: CommandResult::Error { message: "boom".into() },
            };
            let line = format_event_human(&event);
            assert!(line.contains("error: boom"), "should show error message");
        }

        #[test]
        fn peer_all_states() {
            for (state, expected) in [
                (PeerConnectionState::Connected, "connected"),
                (PeerConnectionState::Disconnected, "disconnected"),
                (PeerConnectionState::Connecting, "connecting"),
                (PeerConnectionState::Reconnecting, "reconnecting"),
            ] {
                let event = DaemonEvent::PeerStatusChanged { host: HostName::new("host-2"), status: state };
                let line = format_event_human(&event);
                assert!(line.contains("[peer]"), "should have peer tag for {expected}");
                assert!(line.contains("host-2"), "should show host name for {expected}");
                assert!(line.contains(expected), "should contain '{expected}'");
            }
        }
    }
}
