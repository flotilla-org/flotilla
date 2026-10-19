//! Flotilla process policy and helper scripts over Rigging's shared runtime.
pub mod observation_error;
pub use observation_error::ObservationError;
mod git_config_guard;
pub mod ssh_runner;
pub use rigging_http_client::http_execute;
pub use rigging_http_client::{HttpClient, ReqwestHttpClient};
pub use rigging_process::run;
pub use rigging_process::{
    available_space, command_channel_label, command_channel_label_with, command_timeout_message, default_channel_label, http_channel_label,
    http_channel_label_with, probe, rename_command_timeout, scan_cache, ChannelLabel, ChannelLabeler, ChannelRequest, CommandOutput,
    CommandProcess, CommandRunner, DefaultLabeler, EnvVars, ProcessEnvVars, TaskId, REPLAY_LABELS_ENABLED,
};
use sha2::{Digest, Sha256};
use std::path::Path;
pub struct GitConfigGuard;
#[async_trait::async_trait]
impl rigging_process::CommandGuard for GitConfigGuard {
    fn scratch_namespace(&self) -> &str {
        "flotilla"
    }
    async fn check(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<(), String> {
        git_config_guard::guard_host_git_config_async(cmd, args, cwd).await
    }
}
/// Guarded Rigging runner. The same-named value below preserves existing unit-struct-style call sites.
/// Callers can migrate to explicit `ProcessCommandRunner::new(GitConfigGuard)` construction;
/// retain the value shim until those call sites have migrated.
pub type ProcessCommandRunner = rigging_process::ProcessRunner<GitConfigGuard>;
/// Compatibility value for callers that previously constructed the unit struct as `ProcessCommandRunner`.
#[allow(non_upper_case_globals)]
pub const ProcessCommandRunner: ProcessCommandRunner = rigging_process::ProcessRunner::new(GitConfigGuard);
/// Consumer-owned replay namespace; GitHub fixtures have always used endpoint-only keys.
pub const GH_API_REPLAY_NAMESPACE: &str = "gh_api";

/// Construct a GitHub replay label, including explicit task labels.
pub fn gh_api_label(endpoint: String) -> ChannelLabel {
    ChannelLabel::Extra(GH_API_REPLAY_NAMESPACE.into(), endpoint)
}

pub fn gh_api_channel_label(method: &'static str, endpoint: &str) -> ChannelLabel {
    gh_api_channel_label_with::<REPLAY_LABELS_ENABLED, _>(method, endpoint, &DefaultLabeler)
}
pub fn gh_api_channel_label_with<const ENABLED: bool, L: ChannelLabeler + ?Sized>(
    // Retained for invocation compatibility; historical GitHub labels intentionally ignore the method.
    _method: &'static str,
    endpoint: &str,
    labeler: &L,
) -> ChannelLabel {
    if ENABLED {
        labeler.label_for(&ChannelRequest::Extra { namespace: GH_API_REPLAY_NAMESPACE, key: endpoint })
    } else {
        default_channel_label()
    }
}
pub(crate) const INSTALL_MANAGED_SCRIPT: &str = include_str!("scripts/install_managed_script.sh");
pub(crate) const INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME: &str = "flotilla-bootstrap-install-managed-script";
pub const FLOTILLA_HELPER_NAME: &str = "flotilla-helper";
pub const FLOTILLA_HELPER_SCRIPT: &str = include_str!("scripts/flotilla_helper.sh");
pub fn helper_exec_script(helper_path: &str, subcommand: &str, args: &[&str]) -> Result<String, String> {
    let helper_dir = Path::new(helper_path).parent().ok_or_else(|| format!("installed helper path has no parent: {helper_path}"))?;
    let mut parts = vec![
        format!("PATH={}:\"$PATH\"", flotilla_protocol::arg::shell_quote(&helper_dir.to_string_lossy())),
        "exec".to_string(),
        flotilla_protocol::arg::shell_quote("flotilla-helper"),
        flotilla_protocol::arg::shell_quote(subcommand),
    ];
    parts.extend(args.iter().map(|arg| flotilla_protocol::arg::shell_quote(arg)));
    Ok(parts.join(" "))
}

pub fn atomic_write_script(path: &Path, temp_suffix: &str, mode: Option<u32>) -> Result<String, String> {
    let parent = path.parent().ok_or_else(|| format!("file path has no parent: {}", path.display()))?;
    let target = path.to_string_lossy();
    let temporary = format!("{target}.flotilla-tmp-{temp_suffix}");
    if mode.is_some_and(|mode| mode > 0o777) {
        return Err("file mode must contain only permission bits".to_string());
    }
    let protect = mode.map(|mode| format!("chmod {mode:o} \"$tmp\"; ")).unwrap_or_default();
    let private_create = if mode.is_some() { "umask 077; " } else { "" };
    Ok(format!(
        "set -eu; mkdir -p {}; tmp={}; trap 'rm -f \"$tmp\"' EXIT; {private_create}cat > \"$tmp\"; {protect}mv \"$tmp\" {}; trap - EXIT",
        flotilla_protocol::arg::shell_quote(&parent.to_string_lossy()),
        flotilla_protocol::arg::shell_quote(&temporary),
        flotilla_protocol::arg::shell_quote(&target),
    ))
}

/// `shell_flag` follows the runner's shell policy: contained writes avoid
/// login files, while static SSH hosts retain their configured login path.
pub async fn install_managed_helper_script(
    runner: &dyn CommandRunner,
    command: &str,
    command_prefix: &[&str],
    shell_flag: &str,
    helper_name: &str,
    helper_content: &str,
) -> Result<String, String> {
    let helper_hash = format!("{:x}", Sha256::digest(helper_content.as_bytes()));
    let mut owned_args: Vec<String> = command_prefix.iter().map(|arg| (*arg).to_string()).collect();
    owned_args.extend([
        "sh".to_string(),
        shell_flag.to_string(),
        INSTALL_MANAGED_SCRIPT.to_string(),
        // `sh -c` treats the next argument as `$0`; this is only a diagnostic
        // placeholder for the one-time bootstrap script text above.
        INSTALL_MANAGED_SCRIPT_BOOTSTRAP_NAME.to_string(),
        helper_name.to_string(),
        helper_hash,
        helper_content.to_string(),
    ]);
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let helper_path = runner.run(command, &arg_refs, Path::new("/"), &ChannelLabel::Default).await?;
    let helper_path = helper_path.trim();
    if helper_path.is_empty() {
        return Err(format!("managed helper installer returned empty path for {helper_name}"));
    }
    Ok(helper_path.to_string())
}

pub use rigging_process::run_output;

#[cfg(test)]
mod test_support {
    pub use rigging_replay_testkit::testing::TimeoutOnlyRunner;
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    // Glue: GitHub fixtures require the historical namespace and endpoint-only key, independent of HTTP method.
    #[test]
    fn github_labels_preserve_fixture_channel_identity() {
        let expected = ChannelLabel::Extra("gh_api".into(), "repos/team/repo/pulls".into());
        assert_eq!(gh_api_label("repos/team/repo/pulls".into()), expected);
        for method in ["GET", "POST"] {
            assert_eq!(gh_api_channel_label(method, "repos/team/repo/pulls"), expected);
        }
    }
}
