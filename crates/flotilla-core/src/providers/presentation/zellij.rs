use std::{path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::providers::{run, types::*, CommandRunner};

/// Timeout for individual `zellij action` calls.  Combined with the 1-permit
/// semaphore this limits the blast radius when Zellij is unresponsive: at most
/// one child process can be waiting at a time, and callers give up after the
/// timeout.  Note that the timed-out child process itself may linger until the
/// Zellij server recovers or is killed — the runner's `Command::output()` does
/// not set `kill_on_drop`.
const ZELLIJ_ACTION_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ZellijPresentationManager {
    runner: Arc<dyn CommandRunner>,
    /// Optional override for the session name. When `None`, falls back to
    /// the `ZELLIJ_SESSION_NAME` environment variable.
    session_name_override: Option<String>,
    /// Serialise all `zellij action` calls so we don't pile up child processes
    /// when the server is slow or unresponsive.
    action_semaphore: Semaphore,
}

impl ZellijPresentationManager {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner, session_name_override: None, action_semaphore: Semaphore::new(1) }
    }

    /// Create a manager targeting a specific session name, avoiding the need
    /// to read `ZELLIJ_SESSION_NAME` from the process environment.
    pub fn with_session_name(runner: Arc<dyn CommandRunner>, session_name: String) -> Self {
        Self { runner, session_name_override: Some(session_name), action_semaphore: Semaphore::new(1) }
    }

    /// Run `zellij action <args>` and return stdout, or an error on failure.
    ///
    /// Serialised via a semaphore so at most one `zellij action` child is
    /// outstanding at a time, and wrapped in a timeout so callers give up
    /// rather than blocking forever.
    async fn zellij_action(&self, args: &[&str]) -> Result<String, String> {
        let _permit = self.action_semaphore.acquire().await.map_err(|_| "zellij action semaphore closed".to_string())?;

        let mut cmd_args = vec!["action"];
        cmd_args.extend_from_slice(args);

        let action_desc = args.first().copied().unwrap_or("unknown");
        match tokio::time::timeout(ZELLIJ_ACTION_TIMEOUT, async { run!(self.runner, "zellij", &cmd_args, Path::new(".")) }).await {
            Ok(result) => result.map(|s| s.trim().to_string()),
            Err(_) => {
                warn!(action = %action_desc, timeout_secs = ZELLIJ_ACTION_TIMEOUT.as_secs(), "zellij action timed out");
                Err(format!("zellij action '{action_desc}' timed out after {}s", ZELLIJ_ACTION_TIMEOUT.as_secs()))
            }
        }
    }

    /// Check that `zellij --version` reports >= 0.44.1, when stable tab and
    /// pane targeting are available for workspace creation.
    /// Parses output like "zellij 0.44.1".
    pub async fn check_version(runner: &dyn CommandRunner) -> Result<(), String> {
        let version_str = run!(runner, "zellij", &["--version"], Path::new("."))
            .map_err(|e| format!("failed to run zellij --version: {e}"))?
            .trim()
            .to_string();
        let version_part = version_str.strip_prefix("zellij ").ok_or_else(|| format!("unexpected zellij version output: {version_str}"))?;

        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.len() < 2 {
            return Err(format!("cannot parse zellij version: {version_part}"));
        }

        let major: u32 = parts[0].parse().map_err(|_| format!("invalid major version: {}", parts[0]))?;
        let minor: u32 = parts[1].parse().map_err(|_| format!("invalid minor version: {}", parts[1]))?;
        let patch: u32 = parts
            .get(2)
            .and_then(|part| part.split('-').next())
            .ok_or_else(|| format!("cannot parse zellij version: {version_part}"))?
            .parse()
            .map_err(|_| format!("invalid patch version: {}", parts[2]))?;

        if (major, minor, patch) < (0, 44, 1) {
            return Err(format!("zellij >= 0.44.1 required, found {version_part}"));
        }

        info!(version = %version_part, "zellij version OK");
        Ok(())
    }

    /// Return the current Zellij session name. The session name must have been
    /// resolved at probe time and passed to `with_session_name()`.
    pub fn session_name(&self) -> Result<String, String> {
        self.session_name_override
            .clone()
            .ok_or_else(|| "zellij session name not resolved at probe time (ZELLIJ_SESSION_NAME was not set)".to_string())
    }

    /// Append a command in the form expected by Zellij's pane actions. Using
    /// `sh -c` preserves template commands as a single shell expression.
    fn append_command_args<'a>(args: &mut Vec<&'a str>, command: &'a str) {
        args.extend(["--", "sh", "-c", command]);
    }

    fn parse_tab_id(output: &str) -> Result<u64, String> {
        let tab_id = output.trim();
        tab_id.parse::<u64>().map_err(|_| format!("zellij new-tab returned invalid tab id: {tab_id:?}"))
    }

    fn parse_pane_id(output: &str) -> Result<String, String> {
        let pane_id = output.trim();
        let numeric_id =
            pane_id.strip_prefix("terminal_").ok_or_else(|| format!("zellij new-pane returned invalid terminal pane id: {pane_id:?}"))?;
        numeric_id.parse::<u32>().map_err(|_| format!("zellij new-pane returned invalid terminal pane id: {pane_id:?}"))?;
        Ok(pane_id.to_string())
    }

    async fn initial_terminal_pane_id(&self, tab_id: u64) -> Result<String, String> {
        let output = self.zellij_action(&["list-panes", "--json", "--all"]).await?;
        let panes: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|error| format!("zellij list-panes: {error}"))?;
        let mut terminals =
            panes.iter().filter(|pane| pane["tab_id"].as_u64() == Some(tab_id) && pane["is_plugin"].as_bool() == Some(false));
        let first = terminals.next().ok_or_else(|| format!("zellij tab {tab_id} did not contain a terminal pane"))?;
        let initial = std::iter::once(first).chain(terminals).find(|pane| pane["is_focused"].as_bool() == Some(true)).unwrap_or(first);
        let pane_id = initial["id"].as_u64().ok_or_else(|| format!("zellij tab {tab_id} returned a terminal pane without an id"))?;
        Ok(format!("terminal_{pane_id}"))
    }
}

#[async_trait]
impl super::PresentationManager for ZellijPresentationManager {
    async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
        let output = self.zellij_action(&["list-tabs", "--json"]).await?;
        let tabs: Vec<serde_json::Value> = serde_json::from_str(&output).map_err(|e| format!("zellij list-tabs: {e}"))?;

        let session = self.session_name()?;

        let workspaces = tabs
            .iter()
            .filter_map(|tab| {
                let tab_id = tab["tab_id"].as_u64()?;
                let name = tab["name"].as_str()?.to_string();
                let ws_ref = format!("{session}:{tab_id}");
                Some((ws_ref, Workspace { name, attachable_set_id: None }))
            })
            .collect();

        Ok(workspaces)
    }

    async fn create_workspace(&self, config: &WorkspaceAttachRequest) -> Result<(String, Workspace), String> {
        info!(workspace = %config.name, "zellij: creating workspace");

        let rendered = super::resolve_template(config);
        let working_dir = config.working_directory.as_path().display().to_string();

        let mut new_tab_args = vec!["new-tab", "--name", &config.name, "--cwd", &working_dir];
        if let Some(command) = rendered.panes.first().and_then(|pane| pane.surfaces.first()).map(|surface| surface.command.as_str()) {
            if !command.is_empty() {
                Self::append_command_args(&mut new_tab_args, command);
            }
        }
        let tab_id = Self::parse_tab_id(&self.zellij_action(&new_tab_args).await?)?;
        let tab_id_arg = tab_id.to_string();

        // The tab-id two-step: stamp the created tab into the PM's metadata
        // plane (scope, kind, factory id) so the manifest resolver can group
        // it. Best-effort — a missing metadata plane never fails creation.
        if let Some(stamp) = &config.stamp {
            let payload = flotilla_manifest::stamp::tab_stamp(tab_id, stamp).to_pipe_payload();
            let args = ["pipe", "--name", flotilla_manifest::keys::APPLY_METADATA_PATCH_PIPE, "--", &payload];
            if let Err(err) = run!(self.runner, "zellij", &args, Path::new(".")) {
                warn!(%err, %tab_id, "zellij: could not stamp workspace metadata");
            }
        }

        let created_pane_count = rendered.panes.iter().map(|pane| pane.surfaces.len()).sum::<usize>();
        let focused_pane_index = rendered.panes.iter().position(|pane| pane.focus);
        let first_pane_id =
            if focused_pane_index == Some(0) && created_pane_count > 1 { Some(self.initial_terminal_pane_id(tab_id).await?) } else { None };
        let mut focused_pane_id = first_pane_id.clone();
        let mut active_pane_id = first_pane_id.clone();

        // The current Zellij forks accept a stable tab ID on every pane
        // creation action. This keeps the entire sequence in the new tab even
        // when another client changes the session's focus between commands.
        const SHELL_FALLBACK: &str = "exec \"${SHELL:-sh}\"";
        for (pane_index, pane) in rendered.panes.iter().enumerate() {
            let surfaces_to_skip = usize::from(pane_index == 0);
            let mut pane_first_id = if pane_index == 0 { first_pane_id.clone() } else { None };

            for (surface_index, surface) in pane.surfaces.iter().enumerate().skip(surfaces_to_skip) {
                let mut args = vec!["new-pane", "--tab-id", &tab_id_arg];
                if surface_index == 0 {
                    args.extend(["--direction", pane.split.as_deref().unwrap_or("right")]);
                } else {
                    args.push("--stacked");
                }
                args.extend(["--cwd", &working_dir]);
                Self::append_command_args(&mut args, if surface.command.is_empty() { SHELL_FALLBACK } else { &surface.command });

                let pane_id = Self::parse_pane_id(&self.zellij_action(&args).await?)?;
                pane_first_id.get_or_insert_with(|| pane_id.clone());
                active_pane_id = Some(pane_id);
            }

            if focused_pane_index == Some(pane_index) {
                focused_pane_id = pane_first_id;
            }
        }

        if let Some(pane_id) = focused_pane_id.filter(|pane_id| Some(pane_id) != active_pane_id.as_ref()) {
            self.zellij_action(&["focus-pane-id", &pane_id]).await?;
        }

        let session = self.session_name()?;
        let ws_ref = format!("{session}:{tab_id}");
        info!(workspace = %config.name, "zellij: workspace ready");
        Ok((ws_ref, Workspace { name: config.name.clone(), attachable_set_id: None }))
    }

    async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let tab_id = ws_ref.rsplit_once(':').map(|(_, id)| id).ok_or_else(|| format!("invalid zellij ws_ref: {ws_ref}"))?;
        info!(%ws_ref, %tab_id, "zellij: switching to tab by id");
        self.zellij_action(&["go-to-tab-by-id", tab_id]).await?;
        Ok(())
    }

    async fn delete_workspace(&self, ws_ref: &str) -> Result<(), String> {
        let tab_id = ws_ref.rsplit_once(':').map(|(_, id)| id).ok_or_else(|| format!("invalid zellij ws_ref: {ws_ref}"))?;
        info!(%ws_ref, %tab_id, "zellij: closing tab by id");
        self.zellij_action(&["close-tab", "--tab-id", tab_id]).await?;
        Ok(())
    }

    fn binding_scope_prefix(&self) -> String {
        match self.session_name() {
            Ok(session) => format!("{session}:"),
            Err(_) => String::new(),
        }
    }
}
