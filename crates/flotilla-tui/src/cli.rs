use std::{fmt::Write, path::Path};

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::output::OutputFormat;

use crate::socket::SocketDaemon;

pub(crate) fn format_status_human(repos: &[flotilla_protocol::snapshot::RepoInfo]) -> String {
    if repos.is_empty() {
        return "No repos tracked.\n".to_string();
    }
    let mut out = String::new();
    for (i, repo) in repos.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let loading = if repo.loading { "  (loading)" } else { "" };
        writeln!(out, "{}  {}{}", repo.name, repo.path.display(), loading).expect("write to string");
        let health: Vec<String> = repo
            .provider_health
            .iter()
            .flat_map(|(category, providers)| {
                providers.iter().map(move |(name, v)| format!("{category}/{name}: {}", if *v { "ok" } else { "error" }))
            })
            .collect();
        if !health.is_empty() {
            writeln!(out, "  {}", health.join("  ")).expect("write to string");
        }
    }
    out
}

pub(crate) fn format_status_json(repos: &[flotilla_protocol::snapshot::RepoInfo]) -> String {
    #[derive(Debug, serde::Serialize)]
    struct StatusResponse<'a> {
        repos: &'a [flotilla_protocol::snapshot::RepoInfo],
    }
    flotilla_protocol::output::json_pretty(&StatusResponse { repos })
}

/// Extract a short display name from a repo path (last path component).
#[allow(dead_code)] // Used by format_event_human; wired into run_watch in the next task.
fn repo_name(path: &std::path::Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown")
}

/// Format a `CommandResult` as a short human-readable string.
#[allow(dead_code)] // Used by format_event_human; wired into run_watch in the next task.
fn format_command_result(result: &flotilla_protocol::commands::CommandResult) -> String {
    use flotilla_protocol::commands::CommandResult;
    match result {
        CommandResult::Ok => "ok".to_string(),
        CommandResult::CheckoutCreated { branch } => format!("checkout created: {branch}"),
        CommandResult::BranchNameGenerated { name, .. } => format!("branch name: {name}"),
        CommandResult::CheckoutStatus(_) => "checkout status received".to_string(),
        CommandResult::Error { message } => format!("error: {message}"),
    }
}

#[allow(dead_code)] // Wired into run_watch in the next task.
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
    let repos = daemon.list_repos().await.map_err(|e| e.to_string())?;

    let output = match format {
        OutputFormat::Human => format_status_human(&repos),
        OutputFormat::Json => format_status_json(&repos),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let _ = format;
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();
    println!("watching events (Ctrl-C to stop)...");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let json = serde_json::to_string_pretty(&event).unwrap_or_else(|_| format!("{event:?}"));
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use flotilla_protocol::snapshot::{RepoInfo, RepoLabels};

    fn make_repo(name: &str, path: &str, loading: bool, health: HashMap<String, HashMap<String, bool>>) -> RepoInfo {
        RepoInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            labels: RepoLabels::default(),
            provider_names: HashMap::new(),
            provider_health: health,
            loading,
        }
    }

    fn health(entries: &[(&str, &str, bool)]) -> HashMap<String, HashMap<String, bool>> {
        let mut map: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for (cat, name, ok) in entries {
            map.entry(cat.to_string()).or_default().insert(name.to_string(), *ok);
        }
        map
    }

    mod status_human {
        use super::*;
        use crate::cli::format_status_human;

        #[test]
        fn empty_repos() {
            assert_eq!(format_status_human(&[]), "No repos tracked.\n");
        }

        #[test]
        fn single_repo_healthy() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", false, health(&[("vcs", "Git", true)]))];
            let output = format_status_human(&repos);
            assert!(output.contains("my-repo"), "should contain repo name");
            assert!(output.contains("/tmp/my-repo"), "should contain repo path");
            assert!(output.contains("vcs/Git: ok"), "should show health");
            assert!(!output.contains("loading"), "should not show loading");
        }

        #[test]
        fn repo_loading() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", true, HashMap::new())];
            let output = format_status_human(&repos);
            assert!(output.contains("(loading)"), "should show loading indicator");
        }

        #[test]
        fn repo_with_error_health() {
            let repos = vec![make_repo("r", "/tmp/r", false, health(&[("code_review", "GitHub", false)]))];
            let output = format_status_human(&repos);
            assert!(output.contains("code_review/GitHub: error"), "should show error health");
        }
    }

    mod status_json {
        use super::*;
        use crate::cli::format_status_json;

        #[test]
        fn empty_repos_json() {
            let output = format_status_json(&[]);
            let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
            assert_eq!(parsed["repos"], serde_json::json!([]));
        }

        #[test]
        fn repos_wrapped_in_object() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", false, HashMap::new())];
            let output = format_status_json(&repos);
            let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
            assert!(parsed["repos"].is_array(), "should have repos array");
            assert_eq!(parsed["repos"][0]["name"], "my-repo");
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
            let event =
                DaemonEvent::CommandStarted { command_id: 1, repo: PathBuf::from("/tmp/my-repo"), description: "Refreshing...".into() };
            let line = format_event_human(&event);
            assert!(line.contains("[command]"), "should have command tag");
            assert!(line.contains("started"), "should say started");
            assert!(line.contains("Refreshing..."), "should include description");
        }

        #[test]
        fn command_finished_ok() {
            let event = DaemonEvent::CommandFinished { command_id: 1, repo: PathBuf::from("/tmp/my-repo"), result: CommandResult::Ok };
            let line = format_event_human(&event);
            assert!(line.contains("[command]"), "should have command tag");
            assert!(line.contains("finished"), "should say finished");
            assert!(line.contains("ok"), "should show ok result");
        }

        #[test]
        fn command_finished_error() {
            let event = DaemonEvent::CommandFinished {
                command_id: 1,
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
