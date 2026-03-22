use std::path::Path;

use flotilla_protocol::{HostName, HostPath, PreparedTerminalCommand};
use tracing::{debug, info, warn};

use crate::{
    attachable::{SharedAttachableStore, TerminalPurpose},
    providers::{
        registry::ProviderRegistry,
        terminal::{TerminalEnvVars, TerminalPool},
        types::WorkspaceConfig,
    },
    template::{self, WorkspaceTemplate},
};

/// Build a session name for a managed terminal from its identity parts.
fn session_name(checkout: &str, role: &str, index: u32) -> String {
    format!("flotilla/{checkout}/{role}/{index}")
}

pub(super) struct TerminalPreparationService<'a> {
    registry: &'a ProviderRegistry,
    config_base: &'a Path,
    attachable_store: &'a SharedAttachableStore,
    daemon_socket_path: Option<&'a Path>,
}

impl<'a> TerminalPreparationService<'a> {
    pub(super) fn new(
        registry: &'a ProviderRegistry,
        config_base: &'a Path,
        attachable_store: &'a SharedAttachableStore,
        daemon_socket_path: Option<&'a Path>,
    ) -> Self {
        Self { registry, config_base, attachable_store, daemon_socket_path }
    }

    pub(super) async fn resolve_workspace_commands(&self, config: &mut WorkspaceConfig) {
        let Some((tp_desc, tp)) = self.registry.terminal_pools.preferred_with_desc() else {
            return;
        };

        resolve_terminal_pool(config, tp.as_ref(), self.attachable_store, &tp_desc.implementation, self.daemon_socket_path).await;
    }

    pub(super) async fn prepare_terminal_commands(
        &self,
        branch: &str,
        checkout_path: &Path,
        requested_commands: &[PreparedTerminalCommand],
        workspace_config: impl FnOnce() -> WorkspaceConfig,
    ) -> Result<Vec<PreparedTerminalCommand>, String> {
        if !requested_commands.is_empty() {
            // The requesting host sent its template's role->command mappings.
            // If a terminal pool is available, wrap each command through it
            // for persistent sessions. Otherwise return as-is for passthrough.
            if let Some((tp_desc, tp)) = self.registry.terminal_pools.preferred_with_desc() {
                let terminal_pool_provider = tp_desc.implementation.as_str();
                let mut resolved = Vec::new();
                let mut role_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
                for cmd in requested_commands {
                    let idx = {
                        let entry = role_index.entry(cmd.role.clone()).or_insert(0);
                        let current = *entry;
                        *entry += 1;
                        current
                    };
                    let sn = session_name(branch, &cmd.role, idx);
                    if let Err(err) = tp.ensure_session(&sn, &cmd.command, checkout_path).await {
                        warn!(session = %sn, err = %err, "failed to ensure terminal");
                    }
                    let env_vars = build_terminal_env_vars(
                        branch,
                        &cmd.role,
                        idx,
                        checkout_path,
                        &cmd.command,
                        self.attachable_store,
                        terminal_pool_provider,
                        self.daemon_socket_path,
                    );
                    match tp.attach_command(&sn, &cmd.command, checkout_path, &env_vars).await {
                        Ok(attach_cmd) => resolved.push(PreparedTerminalCommand { role: cmd.role.clone(), command: attach_cmd }),
                        Err(err) => {
                            warn!(session = %sn, err = %err, "failed to get attach command, using original");
                            resolved.push(cmd.clone());
                        }
                    }
                }
                return Ok(resolved);
            }
            return Ok(requested_commands.to_vec());
        }

        let mut config = workspace_config();
        self.resolve_workspace_commands(&mut config).await;

        let commands = if let Some(resolved) = config.resolved_commands { resolved } else { render_template_commands(&config) };

        Ok(commands.into_iter().map(|(role, command)| PreparedTerminalCommand { role, command }).collect())
    }

    pub(super) fn wrap_remote_attach_commands(
        &self,
        target_host: &HostName,
        checkout_path: &Path,
        commands: &[PreparedTerminalCommand],
    ) -> Result<Vec<PreparedTerminalCommand>, String> {
        wrap_remote_attach_commands(target_host, checkout_path, commands, self.config_base)
    }
}

/// Build the env vars to inject into a managed terminal session.
/// Ensures the attachable binding exists (creates it if needed) so the
/// env var is available on the very first attach.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_terminal_env_vars(
    checkout: &str,
    role: &str,
    index: u32,
    cwd: &Path,
    command: &str,
    attachable_store: &SharedAttachableStore,
    terminal_pool_provider: &str,
    daemon_socket_path: Option<&Path>,
) -> TerminalEnvVars {
    let mut vars = Vec::new();

    let sn = session_name(checkout, role, index);
    match attachable_store.lock() {
        Ok(mut store) => {
            // Ensure the attachable exists before looking up its ID.
            // This creates the binding on first workspace creation so the
            // env var is available immediately, not only after shpool's
            // attach_command .inspect() runs.
            let host = HostName::local();
            let set_checkout = HostPath::new(host.clone(), cwd.to_path_buf());
            let (set_id, changed_set) = store.ensure_terminal_set_with_change(Some(host), Some(set_checkout));
            let (attachable_id, changed_attachable) = store.ensure_terminal_attachable_with_change(
                &set_id,
                "terminal_pool",
                terminal_pool_provider,
                &sn,
                TerminalPurpose { checkout: checkout.to_string(), role: role.to_string(), index },
                command,
                cwd.to_path_buf(),
                flotilla_protocol::TerminalStatus::Disconnected,
            );
            vars.push(("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string()));
            if changed_set || changed_attachable {
                if let Err(err) = store.save() {
                    warn!(err = %err, "failed to persist attachable store after env var injection");
                }
            }
        }
        Err(err) => {
            warn!(err = %err, "attachable store lock poisoned in build_terminal_env_vars");
        }
    }

    if let Some(socket) = daemon_socket_path {
        vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.display().to_string()));
    }

    vars
}

/// Resolve terminal sessions through the pool. Each terminal content entry is
/// ensured running and its attach command is stored in `config.resolved_commands`.
pub(super) async fn resolve_terminal_pool(
    config: &mut WorkspaceConfig,
    terminal_pool: &dyn TerminalPool,
    attachable_store: &SharedAttachableStore,
    terminal_pool_provider: &str,
    daemon_socket_path: Option<&Path>,
) {
    let rendered = parse_workspace_template(config).render(&config.template_vars);
    info!(count = rendered.content.len(), "terminal pool: resolving content entries");
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
            let sn = session_name(&config.name, &entry.role, i);
            if let Err(err) = terminal_pool.ensure_session(&sn, &entry.command, &config.working_directory).await {
                warn!(session = %sn, err = %err, "failed to ensure terminal");
                continue;
            }
            let env_vars = build_terminal_env_vars(
                &config.name,
                &entry.role,
                i,
                &config.working_directory,
                &entry.command,
                attachable_store,
                terminal_pool_provider,
                daemon_socket_path,
            );
            match terminal_pool.attach_command(&sn, &entry.command, &config.working_directory, &env_vars).await {
                Ok(cmd) => {
                    debug!(session = %sn, command = ?entry.command, resolved = ?cmd, "terminal resolved");
                    resolved.push((entry.role.clone(), cmd));
                }
                Err(err) => warn!(session = %sn, err = %err, "failed to get attach command"),
            }
        }
    }
    info!(count = resolved.len(), "terminal pool: resolved commands");
    if !resolved.is_empty() {
        config.resolved_commands = Some(resolved);
    }
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

struct RemoteSshInfo {
    target: String,
    multiplex: bool,
}

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

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

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
