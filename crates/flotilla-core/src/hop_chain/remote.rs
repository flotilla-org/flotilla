use flotilla_protocol::{arg::Arg, HostName};
use tracing::warn;

use super::{ResolutionContext, ResolvedAction};
use crate::{config::HostsConfig, path_context::DaemonHostPath};

/// Resolves a `Hop::RemoteToHost` into SSH-specific actions on the context.
///
pub trait RemoteHopResolver: Send + Sync {
    fn resolve_wrap(&self, host: &HostName, context: &mut ResolutionContext) -> Result<(), String>;
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

    /// Build the SSH prefix args: `ssh [-t] [-o ControlMaster=auto ...] <target>`
    fn ssh_prefix_args(&self, info: &SshInfo, allocate_tty: bool) -> Vec<Arg> {
        let mut args = vec![Arg::Literal("ssh".into())];
        if allocate_tty {
            args.push(Arg::Literal("-t".into()));
        }

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

    /// Build a single SSH hop that runs `command` on `host`.
    ///
    /// Unlike `resolve_wrap`, this intentionally does not inspect or wrap an
    /// already-resolved inner action. Store-backed recursive attach uses this
    /// to emit one next-hop command and lets the next daemon resolve the
    /// following hop locally. The command still runs through a login shell so
    /// user-installed tools such as `flotilla` under `~/.cargo/bin` are found.
    pub fn one_hop_command_args(&self, host: &HostName, command: Vec<Arg>) -> Result<Vec<Arg>, String> {
        let info = self.ssh_info(host)?;
        let mut ssh_args = self.ssh_prefix_args(&info, true);
        ssh_args.push(Arg::NestedCommand(vec![
            Arg::Literal("${SHELL:-/bin/sh}".into()),
            Arg::Literal("-l".into()),
            Arg::Literal("-c".into()),
            Arg::NestedCommand(command),
        ]));
        Ok(ssh_args)
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
        let ResolvedAction::Command(inner_args) = inner_action;

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
        let mut ssh_args = self.ssh_prefix_args(&info, true);
        ssh_args.push(Arg::NestedCommand(login_wrapper));

        context.actions.push(ResolvedAction::Command(ssh_args));
        // Working directory has been consumed (baked into the cd prefix)
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
}

/// Create an `SshRemoteHopResolver` by loading hosts config from disk.
pub fn ssh_resolver_from_config(config_base: &DaemonHostPath) -> Result<SshRemoteHopResolver, String> {
    let config = crate::config::ConfigStore::with_base(config_base.as_path());
    let hosts = config.load_hosts()?;
    Ok(SshRemoteHopResolver::new(config_base.clone(), hosts))
}
