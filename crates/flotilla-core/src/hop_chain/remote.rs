use flotilla_protocol::{arg::Arg, HostName};
use tracing::warn;

use super::{ResolutionContext, ResolvedAction, SendKeyStep};
use crate::{config::HostsConfig, path_context::DaemonHostPath};

/// Resolves a `Hop::RemoteToHost` into SSH-specific actions on the context.
///
/// Two methods for the two combination strategies:
/// - `resolve_wrap`: nest the inner command as an SSH argument
/// - `resolve_enter`: open an SSH session, then type the inner command
pub trait RemoteHopResolver: Send + Sync {
    fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String>;
    fn resolve_enter(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String>;
}

/// SSH-based remote hop resolver. Extracts the SSH wrapping knowledge previously
/// hardcoded in `wrap_remote_attach_commands()` into the hop chain model.
pub struct SshRemoteHopResolver {
    hosts: HostsConfig,
    /// Pre-resolved multiplex control path, if the directory was created successfully.
    multiplex_ctrl_path: Option<DaemonHostPath>,
}

/// Resolved SSH connection info for a single host.
struct SshInfo {
    target: String,
    multiplex: bool,
}

impl SshRemoteHopResolver {
    /// Create from pre-loaded hosts config and a config base path (for SSH control socket dir).
    /// The control socket directory is created eagerly here, not during arg building.
    pub fn new(config_base: DaemonHostPath, hosts: HostsConfig) -> Self {
        let ctrl_dir = config_base.join("ssh");
        let multiplex_ctrl_path = match std::fs::create_dir_all(ctrl_dir.as_path()) {
            Ok(()) => Some(ctrl_dir.join("ctrl-%r@%h-%p")),
            Err(err) => {
                warn!(err = %err, "failed to create SSH control socket directory, multiplexing disabled");
                None
            }
        };
        Self { hosts, multiplex_ctrl_path }
    }

    /// Look up SSH connection info for a given HostName.
    fn ssh_info(&self, host: &HostName) -> Result<SshInfo, String> {
        let (label, remote) = self
            .hosts
            .hosts
            .iter()
            .find(|(_, h)| h.expected_host_name == host.as_str())
            .ok_or_else(|| format!("unknown remote host: {host}"))?;

        let target = match &remote.user {
            Some(user) => format!("{user}@{}", remote.hostname),
            None => remote.hostname.clone(),
        };
        let multiplex = self.hosts.resolved_ssh_multiplex(label);
        Ok(SshInfo { target, multiplex })
    }

    /// Build the SSH prefix args: `ssh -t [-o ControlMaster=auto ...] <target>`
    fn ssh_prefix_args(&self, info: &SshInfo) -> Vec<Arg> {
        let mut args = vec![Arg::Literal("ssh".into()), Arg::Literal("-t".into())];

        if info.multiplex {
            if let Some(ref ctrl_path) = self.multiplex_ctrl_path {
                args.push(Arg::Literal("-o".into()));
                args.push(Arg::Literal("ControlMaster=auto".into()));
                args.push(Arg::Literal("-o".into()));
                // Inner double-quotes protect against SSH's config parser splitting on
                // whitespace (e.g. macOS "Application Support"). Assumes the path itself
                // contains no double-quotes, which is safe for filesystem paths.
                args.push(Arg::Quoted(format!("ControlPath=\"{ctrl_path}\"")));
                args.push(Arg::Literal("-o".into()));
                args.push(Arg::Literal("ControlPersist=60".into()));
            }
        }

        args.push(Arg::Quoted(info.target.clone()));
        args
    }
}

impl RemoteHopResolver for SshRemoteHopResolver {
    /// Wrap case: pop the inner Command, wrap it in SSH + ${SHELL:-/bin/sh} -l -c, push back.
    ///
    /// Produces an Arg tree equivalent to:
    ///   ssh -t [multiplex_args] 'user@host' '${SHELL:-/bin/sh} -l -c "cd /dir && inner_cmd"'
    ///
    /// In Arg terms (single-quote model):
    ///   [Literal("ssh"), Literal("-t"), ...multiplex..., Quoted("user@host"),
    ///     NestedCommand([Literal("${SHELL:-/bin/sh}"), Literal("-l"), Literal("-c"),
    ///       NestedCommand([Literal("cd"), Quoted("/dir"), Literal("&&"), ...inner...])])]
    fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
        let info = self.ssh_info(host)?;

        // Pop the inner action — must be a Command
        let inner_action = context.actions.pop().ok_or("resolve_wrap: no inner action on stack")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("resolve_wrap: expected Command on stack, got {other:?}")),
        };

        // Build the innermost args, optionally prefixed with cd
        let shell_inner_args = if let Some(ref dir) = context.working_directory {
            let mut cd_args = vec![Arg::Literal("cd".into()), Arg::Quoted(dir.to_string()), Arg::Literal("&&".into())];
            if inner_args.is_empty() {
                // Empty inner command = open a login shell at the remote directory
                cd_args.push(Arg::Literal("exec".into()));
                cd_args.push(Arg::Literal("${SHELL:-/bin/sh}".into()));
                cd_args.push(Arg::Literal("-l".into()));
            } else {
                cd_args.extend(inner_args);
            }
            cd_args
        } else if inner_args.is_empty() {
            // No working directory, no inner command — just a login shell
            vec![Arg::Literal("exec".into()), Arg::Literal("${SHELL:-/bin/sh}".into()), Arg::Literal("-l".into())]
        } else {
            inner_args
        };

        // Build: ${SHELL:-/bin/sh} -l -c <NestedCommand(shell_inner_args)>
        let login_wrapper = vec![
            Arg::Literal("${SHELL:-/bin/sh}".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(shell_inner_args),
        ];

        // Build: ssh -t [multiplex] target <NestedCommand(login_wrapper)>
        let mut ssh_args = self.ssh_prefix_args(&info);
        ssh_args.push(Arg::NestedCommand(login_wrapper));

        context.actions.push(ResolvedAction::Command(ssh_args));
        // Working directory has been consumed (baked into the cd prefix)
        context.working_directory = None;
        Ok(())
    }

    /// SendKeys case: convert inner Command to SendKeys, push SSH enter command.
    ///
    /// Produces two actions on the stack:
    ///   1. Command: `ssh -t [multiplex] user@host` (bottom — runs first)
    ///   2. SendKeys: Type(flattened inner command) + WaitForPrompt (top)
    fn resolve_enter(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String> {
        let info = self.ssh_info(host)?;

        // Pop the inner action — must be a Command
        let inner_action = context.actions.pop().ok_or("resolve_enter: no inner action on stack")?;
        let inner_args = match inner_action {
            ResolvedAction::Command(args) => args,
            other => return Err(format!("resolve_enter: expected Command on stack, got {other:?}")),
        };

        // Flatten inner command to a string for typing
        let mut type_parts = Vec::new();
        if let Some(ref dir) = context.working_directory {
            type_parts.push(format!("cd {}", flotilla_protocol::arg::shell_quote(&dir.to_string())));
            if !inner_args.is_empty() {
                type_parts.push("&&".into());
                type_parts.push(flotilla_protocol::arg::flatten(&inner_args, 0));
            }
        } else if !inner_args.is_empty() {
            type_parts.push(flotilla_protocol::arg::flatten(&inner_args, 0));
        }

        let type_text = type_parts.join(" ");

        // Push SendKeys for the inner command (if non-empty)
        if !type_text.is_empty() {
            context.actions.push(ResolvedAction::SendKeys { steps: vec![SendKeyStep::Type(type_text), SendKeyStep::WaitForPrompt] });
        }

        // Push SSH enter command (no remote command — just open the session)
        let ssh_args = self.ssh_prefix_args(&info);
        context.actions.push(ResolvedAction::Command(ssh_args));

        // Working directory consumed
        context.working_directory = None;
        Ok(())
    }
}

/// No-op remote hop resolver that always errors. Used when the hop plan
/// contains no `RemoteToHost` hops (e.g. local-only attach).
pub struct NoopRemoteHopResolver;

impl RemoteHopResolver for NoopRemoteHopResolver {
    fn resolve_wrap(&self, host: &HostName, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no remote transport available to reach host: {host}"))
    }

    fn resolve_enter(&self, host: &HostName, _context: &mut ResolutionContext) -> Result<(), String> {
        Err(format!("no remote transport available to reach host: {host}"))
    }
}

/// Create an `SshRemoteHopResolver` by loading hosts config from disk.
pub fn ssh_resolver_from_config(config_base: &DaemonHostPath) -> Result<SshRemoteHopResolver, String> {
    let config = crate::config::ConfigStore::with_base(config_base.as_path());
    let hosts = config.load_hosts()?;
    Ok(SshRemoteHopResolver::new(config_base.clone(), hosts))
}
