use std::path::Path;

use flotilla_protocol::{arg::Arg, HostName, HostPath, PreparedTerminalCommand, ResolvedPaneCommand};
use tracing::{debug, info, warn};

use crate::{
    providers::types::WorkspaceConfig,
    template::{self, WorkspaceTemplate},
    terminal_manager::TerminalManager,
};

pub(super) struct TerminalPreparationService<'a> {
    terminal_manager: &'a TerminalManager,
    daemon_socket_path: Option<&'a Path>,
}

impl<'a> TerminalPreparationService<'a> {
    pub(super) fn new(terminal_manager: &'a TerminalManager, daemon_socket_path: Option<&'a Path>) -> Self {
        Self { terminal_manager, daemon_socket_path }
    }

    pub(super) async fn resolve_workspace_commands(&self, config: &mut WorkspaceConfig) {
        let rendered = parse_workspace_template(config).render(&config.template_vars);
        info!(count = rendered.content.len(), "terminal manager: resolving content entries");
        let host = HostName::local();
        let checkout_path = HostPath::new(host.clone(), config.working_directory.clone());
        let set_id = match self.terminal_manager.allocate_set(host, checkout_path) {
            Ok(id) => id,
            Err(err) => {
                warn!(err = %err, "failed to allocate terminal set");
                return;
            }
        };
        let socket_str = self.daemon_socket_path.map(|p| p.display().to_string());
        let mut resolved = Vec::new();
        for entry in &rendered.content {
            if entry.content_type != "terminal" {
                debug!(
                    role = %entry.role,
                    content_type = %entry.content_type,
                    "skipping non-terminal content",
                );
                continue;
            }
            let count = entry.count.unwrap_or(1);
            for i in 0..count {
                let attachable_id = match self.terminal_manager.allocate_terminal(
                    set_id.clone(),
                    &entry.role,
                    i,
                    &config.name,
                    &entry.command,
                    config.working_directory.clone(),
                ) {
                    Ok(id) => id,
                    Err(err) => {
                        warn!(role = %entry.role, %i, err = %err, "failed to allocate terminal");
                        continue;
                    }
                };
                if let Err(err) = self.terminal_manager.ensure_running(&attachable_id).await {
                    warn!(attachable_id = %attachable_id, err = %err, "failed to ensure terminal");
                    continue;
                }
                match self.terminal_manager.attach_command(&attachable_id, socket_str.as_deref()).await {
                    Ok(cmd) => {
                        debug!(attachable_id = %attachable_id, command = ?entry.command, resolved = ?cmd, "terminal resolved");
                        resolved.push((entry.role.clone(), cmd));
                    }
                    Err(err) => warn!(attachable_id = %attachable_id, err = %err, "failed to get attach command"),
                }
            }
        }
        info!(count = resolved.len(), "terminal manager: resolved commands");
        if !resolved.is_empty() {
            config.resolved_commands = Some(resolved);
        }
    }

    pub(super) async fn prepare_terminal_commands(
        &self,
        branch: &str,
        checkout_path: &Path,
        requested_commands: &[PreparedTerminalCommand],
        workspace_config: impl FnOnce() -> WorkspaceConfig,
    ) -> Result<Vec<ResolvedPaneCommand>, String> {
        if !requested_commands.is_empty() {
            let host = HostName::local();
            let hp = HostPath::new(host.clone(), checkout_path.to_path_buf());
            let set_id = self.terminal_manager.allocate_set(host, hp)?;
            let socket_str = self.daemon_socket_path.map(|p| p.display().to_string());

            let mut resolved = Vec::new();
            let mut role_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for cmd in requested_commands {
                let idx = {
                    let entry = role_index.entry(cmd.role.clone()).or_insert(0);
                    let current = *entry;
                    *entry += 1;
                    current
                };
                let attachable_id = match self.terminal_manager.allocate_terminal(
                    set_id.clone(),
                    &cmd.role,
                    idx,
                    branch,
                    &cmd.command,
                    checkout_path.to_path_buf(),
                ) {
                    Ok(id) => id,
                    Err(err) => {
                        warn!(role = %cmd.role, err = %err, "failed to allocate terminal");
                        // Fallback: wrap original command as Arg::Literal
                        resolved.push(ResolvedPaneCommand { role: cmd.role.clone(), args: vec![Arg::Literal(cmd.command.clone())] });
                        continue;
                    }
                };
                if let Err(err) = self.terminal_manager.ensure_running(&attachable_id).await {
                    warn!(attachable_id = %attachable_id, err = %err, "failed to ensure terminal");
                }
                match self.terminal_manager.attach_args(&attachable_id, socket_str.as_deref()) {
                    Ok(args) => {
                        debug!(attachable_id = %attachable_id, command = ?cmd.command, ?args, "terminal resolved");
                        resolved.push(ResolvedPaneCommand { role: cmd.role.clone(), args });
                    }
                    Err(err) => {
                        warn!(attachable_id = %attachable_id, err = %err, "failed to get attach args, using original");
                        // Fallback: wrap original command as Arg::Literal
                        resolved.push(ResolvedPaneCommand { role: cmd.role.clone(), args: vec![Arg::Literal(cmd.command.clone())] });
                    }
                }
            }
            return Ok(resolved);
        }

        let mut config = workspace_config();
        self.resolve_workspace_commands(&mut config).await;

        let commands = if let Some(resolved) = config.resolved_commands { resolved } else { render_template_commands(&config) };

        Ok(commands.into_iter().map(|(role, command)| ResolvedPaneCommand { role, args: vec![Arg::Literal(command)] }).collect())
    }
}

/// Render template commands without terminal pool resolution.
/// Used when no terminal pool is available.
pub(super) fn render_fallback_commands(workspace_config: impl FnOnce() -> WorkspaceConfig) -> Vec<PreparedTerminalCommand> {
    let config = workspace_config();
    render_template_commands(&config).into_iter().map(|(role, command)| PreparedTerminalCommand { role, command }).collect()
}

fn render_template_commands(config: &WorkspaceConfig) -> Vec<(String, String)> {
    let rendered = parse_workspace_template(config).render(&config.template_vars);
    let mut commands = Vec::new();
    for entry in &rendered.content {
        if entry.content_type != "terminal" {
            continue;
        }
        let count = entry.count.unwrap_or(1);
        for _ in 0..count {
            commands.push((entry.role.clone(), entry.command.clone()));
        }
    }
    commands
}

fn parse_workspace_template(config: &WorkspaceConfig) -> WorkspaceTemplate {
    if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|err| {
            warn!(err = %err, "failed to parse workspace template, using default");
            template::default_template()
        })
    } else {
        template::default_template()
    }
}

// Legacy string-based SSH wrapping. No longer used in production code (replaced
// by the hop chain in Task 11). Retained for existing unit tests until Task 13
// removes them entirely.

#[cfg(test)]
pub(super) fn wrap_remote_attach_commands(
    target_host: &HostName,
    checkout_path: &Path,
    commands: &[PreparedTerminalCommand],
    config_base: &Path,
) -> Result<Vec<PreparedTerminalCommand>, String> {
    let info = remote_ssh_info(target_host, config_base)?;
    let remote_dir = checkout_path.display().to_string();

    let multiplex_args = if info.multiplex {
        let ctrl_dir = config_base.join("ssh");
        if let Err(err) = std::fs::create_dir_all(&ctrl_dir) {
            warn!(err = %err, "failed to create SSH control socket directory, disabling multiplexing");
            String::new()
        } else {
            let ctrl_path = ctrl_dir.join("ctrl-%r@%h-%p");
            format!(" -o ControlMaster=auto -o ControlPath={} -o ControlPersist=60", shell_quote(&ctrl_path.display().to_string()))
        }
    } else {
        String::new()
    };

    Ok(commands
        .iter()
        .map(|entry| {
            let inner = if entry.command.is_empty() {
                // Empty command = open a login shell at the remote directory
                format!("cd {} && exec $SHELL -l", shell_quote(&remote_dir))
            } else {
                format!("cd {} && {}", shell_quote(&remote_dir), entry.command)
            };
            let login_wrapped = format!("$SHELL -l -c \"{}\"", escape_for_double_quotes(&inner));
            PreparedTerminalCommand {
                role: entry.role.clone(),
                command: format!("ssh -t{} {} {}", multiplex_args, shell_quote(&info.target), shell_quote(&login_wrapped)),
            }
        })
        .collect())
}

#[cfg(test)]
struct RemoteSshInfo {
    target: String,
    multiplex: bool,
}

#[cfg(test)]
fn remote_ssh_info(target_host: &HostName, config_base: &Path) -> Result<RemoteSshInfo, String> {
    let config = crate::config::ConfigStore::with_base(config_base);
    let hosts = config.load_hosts()?;
    let (label, remote) = hosts
        .hosts
        .iter()
        .find(|(_, host)| host.expected_host_name == target_host.as_str())
        .ok_or_else(|| format!("unknown remote host: {target_host}"))?;
    let target = match &remote.user {
        Some(user) => format!("{user}@{}", remote.hostname),
        None => remote.hostname.clone(),
    };
    let multiplex = hosts.resolved_ssh_multiplex(label);
    Ok(RemoteSshInfo { target, multiplex })
}

#[cfg(test)]
fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

#[cfg(test)]
pub(super) fn escape_for_double_quotes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' | '$' | '`' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}
