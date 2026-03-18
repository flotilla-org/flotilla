use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{HostName, HostPath, ManagedTerminal, ManagedTerminalId, TerminalStatus};

use super::TerminalPool;
use crate::{
    attachable::{
        terminal_session_binding_ref, AttachableContent, AttachableId, AttachableStoreApi, BindingObjectKind, SharedAttachableStore,
        TerminalPurpose,
    },
    providers::{run, CommandRunner},
};

pub struct ShpoolTerminalPool {
    runner: Arc<dyn CommandRunner>,
    socket_path: PathBuf,
    config_path: PathBuf,
    attachable_store: SharedAttachableStore,
}

/// Shpool config content managed by flotilla.
/// Disables prompt prefix (flotilla manages its own UI) and forwards
/// terminal environment variables that would otherwise be lost when
/// the shpool daemon spawns shells outside the terminal emulator.
/// Note: `forward_env` only takes effect when creating new sessions,
/// not when reattaching to existing ones (shpool limitation).
const FLOTILLA_SHPOOL_CONFIG: &str = include_str!("shpool_config.toml");
const SHPOOL_DAEMON_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShpoolDaemonState {
    Missing,
    HealthyWithPid,
    HealthyWithoutPid,
    InconclusiveWithoutPid,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShpoolNoPidProbe {
    Healthy,
    Inconclusive,
    Stale,
}

impl ShpoolTerminalPool {
    /// Create a new ShpoolTerminalPool, cleaning up stale sockets and
    /// spawning the daemon with flotilla's managed config.
    pub async fn create(runner: Arc<dyn CommandRunner>, socket_path: PathBuf, attachable_store: SharedAttachableStore) -> Self {
        let config_path = socket_path.parent().unwrap_or(Path::new(".")).join("config.toml");
        let config_stale = Self::config_needs_update(&config_path);
        let mut daemon_state = Self::detect_daemon_state(Arc::clone(&runner), &socket_path, &config_path).await;

        if daemon_state == ShpoolDaemonState::Stale {
            let pid_path = socket_path.with_file_name("daemonized-shpool.pid");
            if pid_path.exists() {
                let _ = Self::stop_daemon(&socket_path, "shpool").await;
            } else {
                Self::clean_stale_socket(&socket_path);
            }
            daemon_state = ShpoolDaemonState::Missing;
        }

        if config_stale && daemon_state == ShpoolDaemonState::HealthyWithPid {
            // Daemon is alive but config changed. Validate we can persist
            // the new config BEFORE killing the daemon, so a write failure
            // doesn't tear down sessions for nothing.
            let tmp_path = config_path.with_extension("toml.tmp");
            if Self::write_config(&tmp_path) {
                tracing::info!("shpool config changed, restarting daemon");
                if Self::stop_daemon(&socket_path, "shpool").await {
                    if let Err(e) = std::fs::rename(&tmp_path, &config_path) {
                        tracing::warn!(err = %e, "failed to rename config, cleaning up temp");
                        let _ = std::fs::remove_file(&tmp_path);
                    }
                } else {
                    // Stop failed — delete temp, old config stays for retry.
                    let _ = std::fs::remove_file(&tmp_path);
                }
            }
        } else if config_stale && daemon_state == ShpoolDaemonState::HealthyWithoutPid {
            tracing::warn!("shpool config changed but daemon has no pid file; writing config for future restart");
            Self::write_config(&config_path);
        } else if config_stale && daemon_state == ShpoolDaemonState::InconclusiveWithoutPid {
            tracing::warn!("shpool probe was inconclusive without a pid file; leaving socket and config untouched");
        } else if config_stale {
            // No daemon running, safe to write config directly.
            Self::write_config(&config_path);
        }
        Self::start_daemon(&socket_path, &config_path).await;
        Self { runner, socket_path, config_path, attachable_store }
    }

    /// Sync constructor for tests — skips daemon lifecycle.
    #[cfg(test)]
    pub(crate) fn new(runner: Arc<dyn CommandRunner>, socket_path: PathBuf, attachable_store: SharedAttachableStore) -> Self {
        let config_path = socket_path.parent().unwrap_or(Path::new(".")).join("config.toml");
        Self::write_config(&config_path);
        Self { runner, socket_path, config_path, attachable_store }
    }

    /// Check if a process is alive. Returns true for both "alive and ours"
    /// (kill returns 0) and "alive but not ours" (EPERM).
    #[cfg(unix)]
    fn is_process_alive(pid: i32) -> bool {
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        // EPERM means the process exists but we can't signal it
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Verify that the process at `pid` has a name containing `expected_name`.
    /// Used to guard against PID reuse: if shpool died uncleanly and another
    /// process got the same PID, we must not SIGTERM it.
    #[cfg(unix)]
    fn is_expected_process(pid: i32, expected_name: &str) -> bool {
        use sysinfo::{Pid, System};
        let mut sys = System::new();
        let sysinfo_pid = Pid::from(pid as usize);
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[sysinfo_pid]), true, sysinfo::ProcessRefreshKind::nothing());
        sys.process(sysinfo_pid).map(|p| p.name().to_string_lossy().contains(expected_name)).unwrap_or(false)
    }

    /// Remove stale shpool socket and pid files when the daemon is dead.
    ///
    /// On macOS, `connect()` to a stale Unix socket succeeds (unlike Linux
    /// where it returns ConnectionRefused), causing shpool's auto-daemonize
    /// to think a daemon is running when it isn't.
    #[cfg(unix)]
    fn clean_stale_socket(socket_path: &Path) {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        if !socket_path.exists() {
            return;
        }

        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                if let Some(pid) = contents.trim().parse::<i32>().ok().filter(|&p| p > 0) {
                    if Self::is_process_alive(pid) {
                        tracing::debug!(%pid, "shpool daemon is alive, keeping socket");
                        return;
                    }
                    tracing::info!(%pid, "shpool daemon is dead, removing stale socket");
                }
                // PID file exists but daemon is dead (or unparseable) — remove both
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
            }
            Err(_) => {
                // No pid file but socket exists — can't verify liveness, remove it
                tracing::info!("no pid file found, removing orphaned shpool socket");
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }

    #[cfg(not(unix))]
    fn clean_stale_socket(_socket_path: &Path) {
        // Unix sockets don't exist on non-Unix platforms
    }

    #[cfg(unix)]
    async fn detect_daemon_state(runner: Arc<dyn CommandRunner>, socket_path: &Path, config_path: &Path) -> ShpoolDaemonState {
        if !socket_path.exists() {
            return ShpoolDaemonState::Missing;
        }

        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");
        match std::fs::read_to_string(&pid_path) {
            Ok(contents) => {
                let Some(pid) = contents.trim().parse::<i32>().ok().filter(|&p| p > 0) else {
                    return ShpoolDaemonState::Stale;
                };
                if Self::is_process_alive(pid) && Self::is_expected_process(pid, "shpool") {
                    ShpoolDaemonState::HealthyWithPid
                } else {
                    ShpoolDaemonState::Stale
                }
            }
            Err(_) => match Self::probe_daemon_without_pid_file(runner, socket_path, config_path).await {
                ShpoolNoPidProbe::Healthy => ShpoolDaemonState::HealthyWithoutPid,
                ShpoolNoPidProbe::Inconclusive => ShpoolDaemonState::InconclusiveWithoutPid,
                ShpoolNoPidProbe::Stale => ShpoolDaemonState::Stale,
            },
        }
    }

    #[cfg(not(unix))]
    async fn detect_daemon_state(_runner: Arc<dyn CommandRunner>, socket_path: &Path, _config_path: &Path) -> ShpoolDaemonState {
        if socket_path.exists() {
            ShpoolDaemonState::HealthyWithoutPid
        } else {
            ShpoolDaemonState::Missing
        }
    }

    #[cfg(unix)]
    async fn probe_daemon_without_pid_file(runner: Arc<dyn CommandRunner>, socket_path: &Path, config_path: &Path) -> ShpoolNoPidProbe {
        let socket_path_str = socket_path.display().to_string();
        let config_path_str = config_path.display().to_string();
        let args = ["--no-daemonize", "--socket", &socket_path_str, "-c", &config_path_str, "list", "--json"];
        let label = crate::providers::command_channel_label("shpool", &args);

        match tokio::time::timeout(SHPOOL_DAEMON_PROBE_TIMEOUT, runner.run_output("shpool", &args, Path::new("/"), &label)).await {
            Ok(Ok(output)) if output.success => ShpoolNoPidProbe::Healthy,
            Ok(Ok(output)) => {
                if Self::probe_failure_is_definitely_stale(&output.stderr) {
                    ShpoolNoPidProbe::Stale
                } else {
                    ShpoolNoPidProbe::Inconclusive
                }
            }
            Ok(Err(err)) => {
                if Self::probe_failure_is_definitely_stale(&err) {
                    ShpoolNoPidProbe::Stale
                } else {
                    ShpoolNoPidProbe::Inconclusive
                }
            }
            Err(_) => ShpoolNoPidProbe::Inconclusive,
        }
    }

    #[cfg(not(unix))]
    async fn probe_daemon_without_pid_file(_runner: Arc<dyn CommandRunner>, _socket_path: &Path, _config_path: &Path) -> ShpoolNoPidProbe {
        ShpoolNoPidProbe::Inconclusive
    }

    fn probe_failure_is_definitely_stale(message: &str) -> bool {
        let lower = message.to_ascii_lowercase();
        lower.contains("connection refused")
            || lower.contains("no such file")
            || lower.contains("not found")
            || lower.contains("enoent")
            || lower.contains("does not exist")
    }

    /// Spawn the shpool daemon if one isn't already running.
    ///
    /// `clean_stale_socket` must be called first — it removes sockets for dead
    /// daemons, so if the socket still exists here the daemon is alive and we
    /// can reuse it. If the spawn fails, logs a warning — shpool's built-in
    /// auto-daemonize from `attach` will still work as a fallback.
    #[cfg(unix)]
    async fn start_daemon(socket_path: &Path, config_path: &Path) {
        // If socket exists after clean_stale_socket, a live daemon is already
        // running — reuse it rather than tearing down persistent sessions.
        if socket_path.exists() {
            tracing::debug!("shpool daemon already running, reusing existing");
            return;
        }

        let socket_str = socket_path.display().to_string();
        let config_str = config_path.display().to_string();
        let log_path = socket_path.with_file_name("daemonized-shpool.log");
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        match std::fs::File::create(&log_path) {
            Ok(log_file) => {
                // Clone for stderr before consuming for stdout
                let log_stderr = match log_file.try_clone() {
                    Ok(f) => f.into(),
                    Err(_) => std::process::Stdio::null(),
                };
                let result = tokio::process::Command::new("shpool")
                    .args(["--socket", &socket_str, "-c", &config_str, "daemon"])
                    .stdin(std::process::Stdio::null())
                    .stdout(log_file)
                    .stderr(log_stderr)
                    .spawn();

                match result {
                    Ok(child) => {
                        if let Some(pid) = child.id() {
                            if let Err(e) = std::fs::write(&pid_path, pid.to_string()) {
                                tracing::warn!(err = %e, path = %pid_path.display(), "failed to write shpool pid file");
                            }
                        } else {
                            tracing::warn!("shpool daemon spawned without a pid");
                        }
                        // Child handle is intentionally dropped — tokio does not
                        // kill on drop, so the daemon outlives this handle.
                        tracing::info!("spawned shpool daemon");
                        // Wait for socket to appear (up to 2s)
                        for _ in 0..20 {
                            if socket_path.exists() {
                                tracing::debug!("shpool socket is ready");
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        tracing::warn!("shpool socket did not appear within 2s");
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to spawn shpool daemon");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to create shpool log file");
            }
        }
    }

    #[cfg(not(unix))]
    async fn start_daemon(_socket_path: &Path, _config_path: &Path) {
        // shpool is Unix-only
    }

    /// Gracefully stop a running shpool daemon by sending SIGTERM and
    /// waiting for it to exit. Removes the socket and pid files afterward.
    /// This is load-bearing: `start_daemon()` checks socket existence as
    /// its first guard, so the socket must be gone for a replacement to spawn.
    /// Returns true if the daemon was stopped (or was already dead),
    /// false if it's still alive. `expected_name` is checked against the
    /// process name to guard against PID reuse.
    #[cfg(unix)]
    async fn stop_daemon(socket_path: &Path, expected_name: &str) -> bool {
        let pid_path = socket_path.with_file_name("daemonized-shpool.pid");

        // Read and parse the pid — if we can't, just clean up files
        let pid = match std::fs::read_to_string(&pid_path) {
            Ok(contents) => match contents.trim().parse::<i32>().ok().filter(|&p| p > 0) {
                Some(pid) => pid,
                None => {
                    tracing::warn!("shpool pid file unparseable, removing socket");
                    let _ = std::fs::remove_file(socket_path);
                    let _ = std::fs::remove_file(&pid_path);
                    return true;
                }
            },
            Err(_) => {
                tracing::warn!("no shpool pid file found, removing socket");
                let _ = std::fs::remove_file(socket_path);
                return true;
            }
        };

        // Guard against PID reuse: if shpool died uncleanly and another
        // process got the same PID, we must not SIGTERM it.
        if !Self::is_expected_process(pid, expected_name) {
            tracing::warn!(%pid, "pid file references non-shpool process, removing stale artifacts");
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(&pid_path);
            return true;
        }

        // Send SIGTERM
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        if rc != 0 {
            // ESRCH = process already dead, safe to clean up.
            // EPERM = alive but can't signal — leave socket so start_daemon reuses it.
            if !Self::is_process_alive(pid) {
                tracing::debug!(%pid, "shpool daemon already dead, cleaning up");
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
                return true;
            }
            tracing::warn!(%pid, "cannot signal shpool daemon, keeping existing");
            return false;
        }

        // Wait for process to exit (up to 2s).
        // If the daemon was started in this same process (same-session config
        // restart), it becomes a zombie after SIGTERM until reaped. waitpid
        // with WNOHANG reaps it so is_process_alive sees it as dead. If the
        // daemon is not our child, waitpid returns ECHILD which we ignore.
        for _ in 0..20 {
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if !Self::is_process_alive(pid) {
                tracing::debug!(%pid, "shpool daemon exited after SIGTERM");
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&pid_path);
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Daemon still alive after timeout — leave socket in place so
        // start_daemon() reuses the existing daemon rather than racing
        // a second one against it.
        tracing::warn!(%pid, "shpool daemon did not exit within 2s after SIGTERM, keeping existing");
        false
    }

    #[cfg(not(unix))]
    async fn stop_daemon(_socket_path: &Path, _expected_name: &str) -> bool {
        true
    }

    /// Check whether the config file needs updating (missing or stale).
    fn config_needs_update(path: &Path) -> bool {
        match std::fs::read_to_string(path) {
            Ok(existing) => existing != FLOTILLA_SHPOOL_CONFIG,
            Err(_) => true,
        }
    }

    /// Write the flotilla-managed shpool config. Returns true on success.
    fn write_config(path: &Path) -> bool {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(path = %parent.display(), err = %e, "failed to create shpool config dir");
                return false;
            }
        }
        if let Err(e) = std::fs::write(path, FLOTILLA_SHPOOL_CONFIG) {
            tracing::warn!(path = %path.display(), err = %e, "failed to write shpool config");
            return false;
        }
        true
    }

    /// Parse the JSON output of `shpool list --json`.
    fn parse_list_json(json: &str) -> Result<Vec<ManagedTerminal>, String> {
        let parsed: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("failed to parse shpool list: {e}"))?;

        let sessions = parsed["sessions"].as_array().ok_or("shpool list: no sessions array")?;

        let mut terminals = Vec::new();
        for session in sessions {
            let name = session["name"].as_str().ok_or("shpool session missing name")?;

            // Only show flotilla-managed sessions (prefixed "flotilla/")
            let Some(rest) = name.strip_prefix("flotilla/") else {
                continue;
            };

            // Parse "checkout/role/index" from the right — checkout may contain
            // slashes (e.g. "feature/foo"), but role and index never do.
            let Some((before_index, index_str)) = rest.rsplit_once('/') else {
                continue;
            };
            let Some((checkout, role)) = before_index.rsplit_once('/') else {
                continue;
            };
            let index: u32 = match index_str.parse() {
                Ok(index) => index,
                Err(err) => {
                    tracing::warn!(session = name, index = index_str, err = %err, "failed to parse managed terminal index, defaulting to 0");
                    0
                }
            };

            let status_str = session["status"].as_str().unwrap_or("").to_ascii_lowercase();
            let status = match status_str.as_str() {
                "attached" => TerminalStatus::Running,
                "disconnected" => TerminalStatus::Disconnected,
                _ => TerminalStatus::Disconnected,
            };

            terminals.push(ManagedTerminal {
                id: ManagedTerminalId { checkout: checkout.into(), role: role.into(), index },
                role: role.into(),
                command: String::new(),            // shpool doesn't report the original command
                working_directory: PathBuf::new(), // populated separately if needed
                status,
                attachable_id: None,
                attachable_set_id: None,
            });
        }

        Ok(terminals)
    }

    fn persist_expected_attachable(store: &mut dyn AttachableStoreApi, id: &ManagedTerminalId, command: &str, cwd: &Path) -> bool {
        let host = HostName::local();
        let checkout_path = cwd.to_path_buf();
        let set_checkout = HostPath::new(host.clone(), checkout_path.clone());
        let (set_id, changed_set) = store.ensure_terminal_set_with_change(Some(host), Some(set_checkout));
        let session_name = terminal_session_binding_ref(id);
        let (_, changed_attachable) = store.ensure_terminal_attachable_with_change(
            &set_id,
            "terminal_pool",
            "shpool",
            &session_name,
            TerminalPurpose { checkout: id.checkout.clone(), role: id.role.clone(), index: id.index },
            command,
            checkout_path,
            TerminalStatus::Disconnected,
        );
        changed_set || changed_attachable
    }

    fn reconcile_known_attachable(store: &mut dyn AttachableStoreApi, terminal: &ManagedTerminal, session_name: &str) -> bool {
        // TODO(#360): prune stale attachables/bindings when sessions disappear so
        // the registry does not grow unbounded over time.
        let Some(attachable_id) = store
            .lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, session_name)
            .map(|id| AttachableId::new(id.to_string()))
        else {
            tracing::debug!(session = session_name, "ignoring unknown shpool session without persisted binding");
            return false;
        };
        let (set_id, persisted_working_directory) = {
            let Some(existing) = store.registry().attachables.get(&attachable_id) else {
                tracing::warn!(session = session_name, attachable_id = %attachable_id, "shpool binding points to missing attachable");
                return false;
            };
            let persisted_working_directory = match &existing.content {
                AttachableContent::Terminal(existing_terminal) => existing_terminal.working_directory.clone(),
            };
            (existing.set_id.clone(), persisted_working_directory)
        };
        let working_directory = if terminal.working_directory.as_os_str().is_empty() {
            persisted_working_directory
        } else {
            terminal.working_directory.clone()
        };
        let (_, changed_attachable) = store.ensure_terminal_attachable_with_change(
            &set_id,
            "terminal_pool",
            "shpool",
            session_name,
            TerminalPurpose { checkout: terminal.id.checkout.clone(), role: terminal.id.role.clone(), index: terminal.id.index },
            &terminal.command,
            working_directory,
            terminal.status.clone(),
        );
        changed_attachable
    }

    /// Emit `Disconnected` terminals for bindings whose sessions are not in the live shpool session list.
    fn disconnected_terminals_from_bindings(
        store: &mut dyn AttachableStoreApi,
        observed_sessions: &HashSet<String>,
    ) -> (Vec<ManagedTerminal>, bool) {
        let known_bindings: Vec<(String, AttachableId)> = store
            .registry()
            .bindings
            .iter()
            .filter(|binding| {
                binding.provider_category == "terminal_pool"
                    && binding.provider_name == "shpool"
                    && binding.object_kind == BindingObjectKind::Attachable
                    && !observed_sessions.contains(&binding.external_ref)
            })
            .map(|binding| (binding.external_ref.clone(), AttachableId::new(binding.object_id.clone())))
            .collect();

        let mut terminals = Vec::new();
        let mut any_changed = false;

        for (session_name, attachable_id) in known_bindings {
            let Some((set_id, purpose, command, working_directory)) = ({
                store.registry().attachables.get(&attachable_id).map(|attachable| match &attachable.content {
                    AttachableContent::Terminal(existing_terminal) => (
                        attachable.set_id.clone(),
                        existing_terminal.purpose.clone(),
                        existing_terminal.command.clone(),
                        existing_terminal.working_directory.clone(),
                    ),
                })
            }) else {
                tracing::warn!(session = session_name, attachable_id = %attachable_id, "shpool binding points to missing attachable");
                continue;
            };

            let (_, changed_attachable) = store.ensure_terminal_attachable_with_change(
                &set_id,
                "terminal_pool",
                "shpool",
                &session_name,
                purpose.clone(),
                &command,
                working_directory.clone(),
                TerminalStatus::Disconnected,
            );
            any_changed |= changed_attachable;

            terminals.push(ManagedTerminal {
                id: ManagedTerminalId { checkout: purpose.checkout, role: purpose.role.clone(), index: purpose.index },
                role: purpose.role,
                command,
                working_directory,
                status: TerminalStatus::Disconnected,
                attachable_id: Some(attachable_id),
                attachable_set_id: Some(set_id),
            });
        }

        (terminals, any_changed)
    }
}

#[async_trait]
impl TerminalPool for ShpoolTerminalPool {
    async fn list_terminals(&self) -> Result<Vec<ManagedTerminal>, String> {
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        let result = run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "list", "--json"], Path::new("/"));

        match result {
            Ok(json) => {
                let parsed_terminals = Self::parse_list_json(&json)?;

                // Hold the store lock in a block so it is dropped before any async kill calls.
                let (bound_terminals, orphan_session_names) = {
                    let Ok(mut store) = self.attachable_store.lock() else {
                        tracing::warn!("attachable store lock poisoned while registering shpool terminals");
                        return Ok(parsed_terminals);
                    };
                    let mut any_changed = false;
                    let observed_sessions: HashSet<String> =
                        parsed_terminals.iter().map(|terminal| terminal_session_binding_ref(&terminal.id)).collect();

                    // Partition live terminals into bound (have a binding) and orphans (no binding).
                    let mut bound = Vec::new();
                    let mut orphans = Vec::new();
                    for terminal in &parsed_terminals {
                        let session_name = terminal_session_binding_ref(&terminal.id);
                        let has_binding =
                            store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, &session_name).is_some();
                        if has_binding {
                            any_changed |= Self::reconcile_known_attachable(store.as_mut(), terminal, &session_name);
                            bound.push(terminal.clone());
                        } else {
                            tracing::info!(session = %session_name, "reaping orphan shpool session with no binding");
                            orphans.push(session_name);
                        }
                    }

                    // Emit Disconnected terminals for bindings whose sessions are not in the live list.
                    let (disconnected_terminals, disconnected_changed) =
                        Self::disconnected_terminals_from_bindings(store.as_mut(), &observed_sessions);
                    bound.extend(disconnected_terminals);
                    any_changed |= disconnected_changed;

                    if any_changed {
                        if let Err(err) = store.save() {
                            tracing::warn!(err = %err, "failed to persist attachable registry after shpool refresh");
                        }
                    }

                    (bound, orphans)
                };

                // Best-effort kill of orphan sessions (no binding in attachable store).
                for session_name in &orphan_session_names {
                    let kill_result = run!(
                        self.runner,
                        "shpool",
                        &["--socket", &socket_path_str, "-c", &config_path_str, "kill", session_name],
                        Path::new("/")
                    );
                    if let Err(err) = kill_result {
                        tracing::debug!(session = %session_name, err = %err, "best-effort orphan kill failed");
                    }
                }

                Ok(bound_terminals)
            }
            Err(e) => {
                tracing::debug!(err = %e, "shpool list failed (daemon may not be running)");
                // Return all known terminals as Disconnected so they remain visible
                // in the snapshot. Do NOT reap — we can't distinguish "shpool down"
                // from "all sessions gone".
                let Ok(mut store) = self.attachable_store.lock() else {
                    return Ok(vec![]);
                };
                let empty = HashSet::new();
                let (disconnected, _) = Self::disconnected_terminals_from_bindings(store.as_mut(), &empty);
                Ok(disconnected)
            }
        }
    }

    async fn ensure_running(&self, _id: &ManagedTerminalId, _command: &str, _cwd: &Path) -> Result<(), String> {
        // No-op: shpool creates sessions on first `attach`. The actual session
        // creation happens when the workspace manager runs the attach_command.
        Ok(())
    }

    async fn attach_command(
        &self,
        id: &ManagedTerminalId,
        command: &str,
        cwd: &Path,
        env_vars: &super::TerminalEnvVars,
    ) -> Result<String, String> {
        let session_name = terminal_session_binding_ref(id);
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        let cwd_str = cwd.display().to_string();
        fn sq(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        // shpool attach creates the session if it doesn't exist (using --cmd/--dir),
        // or reattaches if it does (ignoring --cmd/--dir).
        // --cmd does a direct exec with no shell environment, so we wrap commands
        // in an interactive login shell to get the full user environment (PATH,
        // node, direnv, aliases, etc). Empty commands omit --cmd, letting shpool
        // use the user's default shell.
        let env_prefix = if env_vars.is_empty() {
            String::new()
        } else {
            let pairs: Vec<String> = env_vars.iter().map(|(k, v)| format!("{k}={}", sq(v))).collect();
            format!("env {} ", pairs.join(" "))
        };
        let cmd_part = if command.is_empty() && env_vars.is_empty() {
            String::new()
        } else {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            // shell-words parses: /bin/zsh -lic 'claude'
            // → ["/bin/zsh", "-lic", "claude"]
            // Interactive login shell resolves aliases and has full PATH.
            let escaped_cmd = command.replace('\'', "'\\''");
            let inner =
                if command.is_empty() { format!("{env_prefix}{shell}") } else { format!("{env_prefix}{shell} -lic '{escaped_cmd}'") };
            format!(" --cmd {}", sq(&inner))
        };
        Ok(format!(
            "shpool --socket {} -c {} attach{} --dir {} {}",
            sq(&socket_path_str),
            sq(&config_path_str),
            cmd_part,
            sq(&cwd_str),
            sq(&session_name),
        ))
        .inspect(|_| {
            let Ok(mut store) = self.attachable_store.lock() else {
                tracing::warn!("attachable store lock poisoned while persisting expected shpool attachable");
                return;
            };
            if Self::persist_expected_attachable(store.as_mut(), id, command, cwd) {
                if let Err(err) = store.save() {
                    tracing::warn!(err = %err, "failed to persist attachable registry after shpool attach command");
                }
            }
        })
    }

    async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
        let session_name = terminal_session_binding_ref(id);
        let socket_path_str = self.socket_path.display().to_string();
        let config_path_str = self.config_path.display().to_string();
        run!(self.runner, "shpool", &["--socket", &socket_path_str, "-c", &config_path_str, "kill", &session_name], Path::new("/"))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use async_trait::async_trait;

    use super::*;
    use crate::{
        attachable::{BindingObjectKind, SharedAttachableStore},
        providers::{testing::MockRunner, ChannelLabel, CommandOutput, CommandRunner},
    };

    struct DelayedRunner {
        delay: Duration,
    }

    #[async_trait]
    impl CommandRunner for DelayedRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            tokio::time::sleep(self.delay).await;
            Ok("{\"sessions\":[]}".into())
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            match self.run(cmd, args, cwd, label).await {
                Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
                Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
            }
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    /// Create a ShpoolTerminalPool in a temp dir so config writes succeed.
    fn test_store(dir: &tempfile::TempDir) -> SharedAttachableStore {
        crate::attachable::shared_file_backed_attachable_store(dir.path())
    }

    /// Create a ShpoolTerminalPool in a temp dir so config writes succeed.
    fn test_pool(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, SharedAttachableStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir for shpool test");
        let socket_path = dir.path().join("shpool.socket");
        let store = test_store(&dir);
        let pool = ShpoolTerminalPool::new(runner, socket_path, Arc::clone(&store));
        (pool, store, dir)
    }

    #[test]
    fn write_config_writes_expected_content() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_path = dir.path().join("config.toml");
        assert!(ShpoolTerminalPool::write_config(&config_path));
        let content = std::fs::read_to_string(&config_path).expect("config should have been written");
        assert!(content.contains("prompt_prefix = \"\""));
        assert!(content.contains("TERMINFO"));
        assert!(content.contains("COLORTERM"));
    }

    #[test]
    fn config_needs_update_tracks_staleness() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config_path = dir.path().join("config.toml");

        // File doesn't exist → needs update
        assert!(ShpoolTerminalPool::config_needs_update(&config_path));

        // Write config, now it matches → no update needed
        ShpoolTerminalPool::write_config(&config_path);
        assert!(!ShpoolTerminalPool::config_needs_update(&config_path));

        // Modify externally → needs update again
        std::fs::write(&config_path, "stale config").expect("write stale");
        assert!(ShpoolTerminalPool::config_needs_update(&config_path));
    }

    #[test]
    fn parse_list_json_with_flotilla_sessions() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/my-feature/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/my-feature/agent/0",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                },
                {
                    "name": "user-manual-session",
                    "started_at_unix_ms": 1709900002000,
                    "status": "Attached"
                }
            ]
        }"#;

        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(terminals.len(), 2); // user-manual-session filtered out

        assert_eq!(terminals[0].id.checkout, "my-feature");
        assert_eq!(terminals[0].id.role, "shell");
        assert_eq!(terminals[0].id.index, 0);
        assert_eq!(terminals[0].status, TerminalStatus::Running);

        assert_eq!(terminals[1].id.checkout, "my-feature");
        assert_eq!(terminals[1].id.role, "agent");
        assert_eq!(terminals[1].status, TerminalStatus::Disconnected);
    }

    #[test]
    fn parse_list_json_with_slashy_branch_names() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/feature/foo/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/feat/deep/nested/agent/1",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                }
            ]
        }"#;

        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert_eq!(terminals.len(), 2);

        assert_eq!(terminals[0].id.checkout, "feature/foo");
        assert_eq!(terminals[0].id.role, "shell");
        assert_eq!(terminals[0].id.index, 0);

        assert_eq!(terminals[1].id.checkout, "feat/deep/nested");
        assert_eq!(terminals[1].id.role, "agent");
        assert_eq!(terminals[1].id.index, 1);
    }

    #[test]
    fn parse_list_json_empty_sessions() {
        let json = r#"{"sessions": []}"#;
        let terminals = ShpoolTerminalPool::parse_list_json(json).unwrap();
        assert!(terminals.is_empty());
    }

    #[test]
    fn parse_list_json_invalid_json() {
        assert!(ShpoolTerminalPool::parse_list_json("not json").is_err());
    }

    #[tokio::test]
    async fn ensure_running_is_noop() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        assert!(pool.ensure_running(&id, "bash", Path::new("/home/dev")).await.is_ok());
    }

    #[tokio::test]
    async fn attach_command_includes_cmd_dir_and_config() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        let cmd = pool.attach_command(&id, "bash", Path::new("/home/dev"), &vec![]).await.unwrap();
        assert!(cmd.contains("shpool"));
        assert!(cmd.contains("attach"));
        assert!(cmd.contains("--cmd"));
        assert!(cmd.contains("-lic"));
        assert!(cmd.contains("bash"));
        assert!(cmd.contains("--dir"));
        assert!(cmd.contains("/home/dev"));
        assert!(cmd.contains("flotilla/feat/shell/0"));
        assert!(cmd.contains("-c"), "should pass config file: {cmd}");
        assert!(cmd.contains("config.toml"), "should reference config.toml: {cmd}");
    }

    #[tokio::test]
    async fn attach_command_empty_cmd_omits_cmd_flag() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        let cmd = pool.attach_command(&id, "", Path::new("/home/dev"), &vec![]).await.unwrap();
        assert!(cmd.contains("shpool"));
        assert!(cmd.contains("attach"));
        assert!(!cmd.contains("--cmd"));
        assert!(cmd.contains("--dir"));
        assert!(cmd.contains("-c"));
    }

    #[tokio::test]
    async fn attach_command_injects_env_vars_into_cmd() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "agent".into(), index: 0 };
        let env = vec![
            ("FLOTILLA_ATTACHABLE_ID".to_string(), "att-uuid-123".to_string()),
            ("FLOTILLA_DAEMON_SOCKET".to_string(), "/tmp/flotilla.sock".to_string()),
        ];
        let cmd = pool.attach_command(&id, "claude", Path::new("/home/dev"), &env).await.unwrap();
        assert!(cmd.contains("--cmd"), "should have --cmd: {cmd}");
        assert!(cmd.contains("FLOTILLA_ATTACHABLE_ID"), "should contain attachable id env: {cmd}");
        assert!(cmd.contains("att-uuid-123"), "should contain attachable id value: {cmd}");
        assert!(cmd.contains("FLOTILLA_DAEMON_SOCKET"), "should contain socket env: {cmd}");
        assert!(cmd.contains("claude"), "should contain original command: {cmd}");
    }

    #[tokio::test]
    async fn attach_command_env_vars_with_empty_command_still_generates_cmd() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        let env = vec![("FLOTILLA_ATTACHABLE_ID".to_string(), "att-1".to_string())];
        let cmd = pool.attach_command(&id, "", Path::new("/home/dev"), &env).await.unwrap();
        assert!(cmd.contains("--cmd"), "env vars with empty command should still produce --cmd: {cmd}");
        assert!(cmd.contains("FLOTILLA_ATTACHABLE_ID"), "should contain env var: {cmd}");
    }

    #[tokio::test]
    async fn list_terminals_returns_empty_when_daemon_not_running() {
        let (pool, _store, _dir) = test_pool(Arc::new(MockRunner::new(vec![Err("connection refused".into())])));
        let terminals = pool.list_terminals().await.unwrap();
        assert!(terminals.is_empty());
    }

    #[tokio::test]
    async fn list_terminals_updates_existing_attachable_bindings() {
        let json = r#"{
            "sessions": [
                {
                    "name": "flotilla/my-feature/shell/0",
                    "started_at_unix_ms": 1709900000000,
                    "status": "Attached"
                },
                {
                    "name": "flotilla/my-feature/agent/0",
                    "started_at_unix_ms": 1709900001000,
                    "status": "Disconnected"
                }
            ]
        }"#;
        let (pool, store, _dir) = test_pool(Arc::new(MockRunner::new(vec![Ok(json.into())])));

        let shell_id = ManagedTerminalId { checkout: "my-feature".into(), role: "shell".into(), index: 0 };
        let agent_id = ManagedTerminalId { checkout: "my-feature".into(), role: "agent".into(), index: 0 };
        let cwd = Path::new("/home/dev/project");

        pool.attach_command(&shell_id, "bash", cwd, &vec![]).await.expect("seed shell binding");
        pool.attach_command(&agent_id, "claude", cwd, &vec![]).await.expect("seed agent binding");

        let terminals = pool.list_terminals().await.expect("list terminals");
        assert_eq!(terminals.len(), 2);

        let store = store.lock().expect("lock store");
        assert_eq!(store.registry().sets.len(), 1);
        assert_eq!(store.registry().attachables.len(), 2);
        assert!(store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/my-feature/shell/0").is_some());
        assert!(store.lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/my-feature/agent/0").is_some());
        let shell_attachable_id = store
            .lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/my-feature/shell/0")
            .expect("shell binding");
        let shell_attachable =
            store.registry().attachables.get(&flotilla_protocol::AttachableId::new(shell_attachable_id)).expect("shell attachable");
        let agent_attachable_id = store
            .lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/my-feature/agent/0")
            .expect("agent binding");
        let agent_attachable =
            store.registry().attachables.get(&flotilla_protocol::AttachableId::new(agent_attachable_id)).expect("agent attachable");
        let crate::attachable::AttachableContent::Terminal(shell_terminal) = &shell_attachable.content;
        let crate::attachable::AttachableContent::Terminal(agent_terminal) = &agent_attachable.content;
        assert_eq!(
            shell_terminal.working_directory.as_path(),
            cwd,
            "known terminal binding should stay anchored to the real checkout path"
        );
        assert_eq!(shell_terminal.status, TerminalStatus::Running, "scan should update the known terminal status");
        assert_eq!(
            agent_terminal.status,
            TerminalStatus::Disconnected,
            "scan should reconcile disconnected known terminals without creating new identity"
        );
    }

    #[tokio::test]
    async fn list_terminals_reaps_orphan_sessions() {
        let json = r#"{
            "sessions": [
                {"name": "flotilla/my-feature/shell/0", "started_at_unix_ms": 1709900000000, "status": "Attached"},
                {"name": "flotilla/orphan/agent/0", "started_at_unix_ms": 1709900001000, "status": "Attached"}
            ]
        }"#;
        let runner = Arc::new(MockRunner::new(vec![
            Ok(json.into()),   // list
            Ok(String::new()), // kill for orphan
        ]));
        let (pool, _store, _dir) = test_pool(runner.clone());

        // Only create a binding for my-feature/shell/0
        let shell_id = ManagedTerminalId { checkout: "my-feature".into(), role: "shell".into(), index: 0 };
        pool.attach_command(&shell_id, "bash", Path::new("/repo"), &vec![]).await.expect("seed binding");

        let terminals = pool.list_terminals().await.expect("list terminals");

        // Only the bound terminal should be returned
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].id.checkout, "my-feature");

        // Verify the kill was issued (all responses consumed)
        assert_eq!(runner.remaining(), 0, "orphan kill should have consumed the second response");
    }

    #[tokio::test]
    async fn list_terminals_skips_reap_when_shpool_unreachable() {
        let runner = Arc::new(MockRunner::new(vec![Err("connection refused".into())]));
        let (pool, store, _dir) = test_pool(runner);

        // Pre-populate a binding
        {
            let mut s = store.lock().expect("lock store");
            let set_id = s.ensure_terminal_set(Some(HostName::local()), Some(HostPath::new(HostName::local(), "/repo/wt-feat")));
            s.ensure_terminal_attachable(
                &set_id,
                "terminal_pool",
                "shpool",
                "flotilla/feat/shell/0",
                TerminalPurpose { checkout: "feat".into(), role: "shell".into(), index: 0 },
                "bash",
                PathBuf::from("/repo/wt-feat"),
                TerminalStatus::Running,
            );
        }

        let terminals = pool.list_terminals().await.expect("should succeed");

        // Known terminal should appear as Disconnected (not vanish)
        assert_eq!(terminals.len(), 1, "known terminal should be emitted as Disconnected when shpool is down");
        assert_eq!(terminals[0].status, TerminalStatus::Disconnected);
        assert_eq!(terminals[0].id.checkout, "feat");

        // Binding should still exist (reaper did NOT run)
        let s = store.lock().expect("lock store");
        assert_eq!(s.registry().attachables.len(), 1, "attachable should not be reaped when shpool is down");
    }

    #[tokio::test]
    async fn list_terminals_emits_disconnected_from_bindings() {
        // Shpool reports only one of two known sessions
        let json = r#"{
            "sessions": [
                {"name": "flotilla/my-feature/shell/0", "started_at_unix_ms": 1709900000000, "status": "Attached"}
            ]
        }"#;
        let runner = Arc::new(MockRunner::new(vec![Ok(json.into())]));
        let (pool, _store, _dir) = test_pool(runner);

        // Create bindings for both sessions
        let shell_id = ManagedTerminalId { checkout: "my-feature".into(), role: "shell".into(), index: 0 };
        let agent_id = ManagedTerminalId { checkout: "my-feature".into(), role: "agent".into(), index: 0 };
        pool.attach_command(&shell_id, "bash", Path::new("/repo"), &vec![]).await.expect("seed shell");
        pool.attach_command(&agent_id, "claude", Path::new("/repo"), &vec![]).await.expect("seed agent");

        let terminals = pool.list_terminals().await.expect("list terminals");

        // Both should be returned: one Running, one Disconnected
        assert_eq!(terminals.len(), 2);
        let shell = terminals.iter().find(|t| t.id.role == "shell").expect("shell");
        let agent = terminals.iter().find(|t| t.id.role == "agent").expect("agent");
        assert_eq!(shell.status, TerminalStatus::Running);
        assert_eq!(agent.status, TerminalStatus::Disconnected);
    }

    #[tokio::test]
    async fn attach_command_registers_expected_attachable_binding() {
        let (pool, store, _dir) = test_pool(Arc::new(MockRunner::new(vec![])));
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };

        let command = pool.attach_command(&id, "bash", Path::new("/home/dev/project"), &vec![]).await.expect("attach command");

        assert!(command.contains(" attach"));
        assert!(command.contains("flotilla/feat/shell/0"));

        let store = store.lock().expect("lock store");
        let attachable_id = store
            .lookup_binding("terminal_pool", "shpool", BindingObjectKind::Attachable, "flotilla/feat/shell/0")
            .expect("attach command should persist the expected session binding");
        let attachable = store.registry().attachables.get(&flotilla_protocol::AttachableId::new(attachable_id)).expect("attachable");
        let crate::attachable::AttachableContent::Terminal(terminal) = &attachable.content;
        assert_eq!(store.registry().sets.len(), 1);
        assert_eq!(terminal.working_directory.as_path(), Path::new("/home/dev/project"));
    }

    #[test]
    fn clean_stale_socket_removes_dead_pid_artifacts() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // Create a socket file and a pid file pointing to a dead process
        std::fs::write(&socket_path, b"").expect("create fake socket");
        // PID 99999999 is almost certainly not running
        std::fs::write(&pid_path, "99999999").expect("create fake pid");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");
    }

    #[test]
    fn clean_stale_socket_removes_orphan_socket_without_pid_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Socket exists but no pid file
        std::fs::write(&socket_path, b"").expect("create fake socket");

        ShpoolTerminalPool::clean_stale_socket(&socket_path);

        assert!(!socket_path.exists(), "orphan socket should be removed");
    }

    #[test]
    fn clean_stale_socket_noop_when_nothing_exists() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Nothing exists — should not panic
        ShpoolTerminalPool::clean_stale_socket(&socket_path);
    }

    #[tokio::test]
    async fn detect_daemon_state_without_pid_file_is_healthy_when_probe_succeeds() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        std::fs::write(&socket_path, b"").expect("create fake socket");

        let runner = Arc::new(MockRunner::new(vec![Ok("{\"sessions\":[]}".into())]));
        let state = ShpoolTerminalPool::detect_daemon_state(runner, &socket_path, &dir.path().join("config.toml")).await;

        assert_eq!(state, ShpoolDaemonState::HealthyWithoutPid);
    }

    #[tokio::test]
    async fn detect_daemon_state_without_pid_file_is_inconclusive_when_probe_fails() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        std::fs::write(&socket_path, b"").expect("create fake socket");

        let runner = Arc::new(MockRunner::new(vec![Err("connection failed".into())]));
        let state = ShpoolTerminalPool::detect_daemon_state(runner, &socket_path, &dir.path().join("config.toml")).await;

        assert_eq!(state, ShpoolDaemonState::InconclusiveWithoutPid);
    }

    #[tokio::test]
    async fn detect_daemon_state_without_pid_file_is_inconclusive_when_probe_times_out() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        std::fs::write(&socket_path, b"").expect("create fake socket");

        let runner = Arc::new(DelayedRunner { delay: SHPOOL_DAEMON_PROBE_TIMEOUT + Duration::from_millis(50) });
        let state = ShpoolTerminalPool::detect_daemon_state(runner, &socket_path, &dir.path().join("config.toml")).await;

        assert_eq!(state, ShpoolDaemonState::InconclusiveWithoutPid);
    }

    #[tokio::test]
    async fn detect_daemon_state_without_pid_file_is_stale_when_probe_reports_connection_refused() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        std::fs::write(&socket_path, b"").expect("create fake socket");

        let runner = Arc::new(MockRunner::new(vec![Err("connection refused".into())]));
        let state = ShpoolTerminalPool::detect_daemon_state(runner, &socket_path, &dir.path().join("config.toml")).await;

        assert_eq!(state, ShpoolDaemonState::Stale);
    }

    #[tokio::test]
    async fn stop_daemon_cleans_up_dead_pid() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // PID 99999999 is above both Linux pid_max (4194304) and macOS
        // kern.pid_max (99998), so kill() returns ESRCH (no such process).
        // This exercises the SIGTERM-failure → dead-process cleanup path.
        std::fs::write(&socket_path, b"").expect("create fake socket");
        std::fs::write(&pid_path, "99999999").expect("create fake pid");

        ShpoolTerminalPool::stop_daemon(&socket_path, "shpool").await;

        assert!(!socket_path.exists(), "socket should be removed");
        assert!(!pid_path.exists(), "pid file should be removed");
    }

    #[tokio::test]
    async fn stop_daemon_handles_missing_pid_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");

        // Socket exists but no pid file — should not panic
        std::fs::write(&socket_path, b"").expect("create fake socket");

        ShpoolTerminalPool::stop_daemon(&socket_path, "shpool").await;

        // Socket should still be removed (best-effort cleanup)
        assert!(!socket_path.exists(), "socket should be removed");
    }

    #[tokio::test]
    async fn stop_daemon_sigterms_live_process() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // Spawn a real process that will respond to SIGTERM.
        // Pass "sleep" as expected_name so the PID-reuse guard accepts it.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep process");
        let pid = child.id();

        // Wait until sysinfo can see the process name — avoids flakiness
        // where is_expected_process returns false because /proc/<pid>/stat
        // isn't fully populated yet under load.
        let mut visible = false;
        for _ in 0..50 {
            if ShpoolTerminalPool::is_expected_process(pid as i32, "sleep") {
                visible = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(visible, "sleep process should be visible to sysinfo before testing stop_daemon");

        std::fs::write(&socket_path, b"").expect("create fake socket");
        std::fs::write(&pid_path, pid.to_string()).expect("write pid file");

        ShpoolTerminalPool::stop_daemon(&socket_path, "sleep").await;

        assert!(!socket_path.exists(), "socket should be removed after SIGTERM");
        assert!(!pid_path.exists(), "pid file should be removed after SIGTERM");
        // Process should be dead
        assert!(!ShpoolTerminalPool::is_process_alive(pid as i32), "process should be dead after SIGTERM");
        let _ = child.wait();
    }

    #[tokio::test]
    async fn stop_daemon_rejects_wrong_process_name() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let socket_path = dir.path().join("shpool.socket");
        let pid_path = dir.path().join("daemonized-shpool.pid");

        // Spawn a sleep process but tell stop_daemon to expect "shpool"
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep process");
        let pid = child.id();

        std::fs::write(&socket_path, b"").expect("create fake socket");
        std::fs::write(&pid_path, pid.to_string()).expect("write pid file");

        // Should detect PID reuse (sleep != shpool), clean up files,
        // but NOT kill the process.
        let stopped = ShpoolTerminalPool::stop_daemon(&socket_path, "shpool").await;
        assert!(stopped, "should return true (stale artifacts cleaned)");
        assert!(!socket_path.exists(), "socket should be removed");
        assert!(!pid_path.exists(), "pid file should be removed");
        // Process should still be alive — we didn't SIGTERM it
        assert!(ShpoolTerminalPool::is_process_alive(pid as i32), "non-shpool process should NOT be killed");

        // Clean up the sleep process
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let _ = child.wait();
    }

    /// Create a ShpoolTerminalPool via the async factory method.
    async fn test_pool_async(runner: Arc<MockRunner>) -> (ShpoolTerminalPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir for shpool test");
        let socket_path = dir.path().join("shpool.socket");
        let pool = ShpoolTerminalPool::create(runner, socket_path, test_store(&dir)).await;
        (pool, dir)
    }

    #[tokio::test]
    async fn create_writes_config_and_returns_pool() {
        // No mock responses needed — start_daemon spawns shpool directly
        // (not through MockRunner), and will fail gracefully in test.
        let runner = Arc::new(MockRunner::new(vec![]));
        let (_pool, dir) = test_pool_async(runner).await;
        let config_path = dir.path().join("config.toml");
        assert!(config_path.exists(), "config should be written");
        // display_name removed — verified via ProviderDescriptor now
    }
}
