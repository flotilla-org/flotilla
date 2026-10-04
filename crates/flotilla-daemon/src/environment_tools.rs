use std::{
    env,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::{
    config::ConfigStore,
    in_process::InProcessDaemon,
    path_context::DaemonHostPath,
    providers::{
        environment::{
            contained_daemon_socket_path, EnvironmentTool, EnvironmentToolAsset, EnvironmentToolAssetAccess, EnvironmentToolAssetKind,
            EnvironmentVariableUpdate, CONTAINED_DAEMON_REQUIRED_ENV,
        },
        ChannelLabel, CommandRunner,
    },
};
use tokio::sync::OnceCell;

pub(crate) const DOCKER_PROVIDER_KIND: &str = "docker";

pub(crate) const ENVIRONMENT_FLOTILLA_DIRECTORY: &str = "/opt/flotilla/bin";
pub(crate) const ENVIRONMENT_FLOTILLA_PATH: &str = "/usr/local/bin/flotilla";
const FLOTILLA_LAUNCHER_NAME: &str = "contained-flotilla-launcher";
const FLOTILLA_LAUNCHER: &str = "#!/bin/sh\nexec /opt/flotilla/bin/flotilla \"$@\"\n";
#[cfg(test)]
pub(crate) const ENVIRONMENT_DAEMON_SOCKET_PATH: &str = "/run/flotilla-daemon/flotilla.sock";
pub(crate) const ENVIRONMENT_CLEAT_PATH: &str = "/usr/local/bin/cleat";
pub(crate) const ENVIRONMENT_CLEAT_LIBRARY_DIR: &str = "/usr/local/lib/flotilla";
pub(crate) const ENVIRONMENT_CLEAT_GHOSTTY_LIBRARY_PATH: &str = "/usr/local/lib/flotilla/libghostty-vt.so.0";
pub(crate) const ENVIRONMENT_CLEAT_RUNTIME_DIR: &str = "/var/lib/flotilla/cleat";
const CLEAT_GHOSTTY_LIBRARY: &str = "libghostty-vt.so.0";
#[cfg(test)]
const FLEET_INSTALL_MARKER: &str = "# managed by fleet-install";
const FLEET_INSTALL_LAUNCHER_PREFIX: &[u8] = b"#!/usr/bin/env bash\n# managed by fleet-install\n";

#[async_trait]
pub(crate) trait EnvironmentToolFactory: Send + Sync {
    fn provider_kinds(&self) -> &[&str];
    async fn prepare(&self, environment_name: &str, context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String>;
}

/// Per-provisioning host facts, shared with the runtime's resource limits.
/// Factories query only the facts their tool needs.
#[async_trait]
pub(crate) trait EnvironmentToolContext: Send + Sync {
    async fn rust_build_jobs(&self) -> Result<usize, String>;
}

struct LocalToolContext<'a> {
    daemon: &'a Arc<InProcessDaemon>,
    config: &'a Arc<ConfigStore>,
    daemon_socket_path: Option<DaemonHostPath>,
}

// Enrollment order is delivery order. Add new local-host tools here; runtime
// and provider delivery plumbing do not need to know their identities.
type ToolRegistration = fn(&LocalToolContext<'_>) -> Arc<dyn EnvironmentToolFactory>;
const REGISTERED_TOOLS: &[ToolRegistration] =
    &[FlotillaCliTool::for_local_host, CleatTool::for_local_host, RustBuildLimitsTool::for_local_host];

/// Prepares the provider-neutral set of tools required by a new environment.
///
/// Factories resolve host assets and durable state. The resulting
/// `EnvironmentTool` values are handed to the selected `EnvironmentProvider`,
/// which owns the delivery strategy.
pub(crate) struct EnvironmentToolProvisioner {
    factories: Vec<Arc<dyn EnvironmentToolFactory>>,
}

impl EnvironmentToolProvisioner {
    pub(crate) fn for_local_host(
        daemon: &Arc<InProcessDaemon>,
        config: &Arc<ConfigStore>,
        daemon_socket_path: Option<DaemonHostPath>,
    ) -> Self {
        let context = LocalToolContext { daemon, config, daemon_socket_path };
        Self::new(REGISTERED_TOOLS.iter().map(|register| register(&context)).collect())
    }

    pub(crate) fn new(factories: Vec<Arc<dyn EnvironmentToolFactory>>) -> Self {
        Self { factories }
    }

    /// Delivery order also determines error precedence: stop at the first
    /// applicable factory failure, without preparing any later tools.
    pub(crate) async fn prepare(
        &self,
        provider_kind: &str,
        environment_name: &str,
        context: &dyn EnvironmentToolContext,
    ) -> Result<Vec<EnvironmentTool>, String> {
        let mut tools = Vec::with_capacity(self.factories.len());
        for factory in &self.factories {
            if factory.provider_kinds().contains(&provider_kind) {
                tools.push(factory.prepare(environment_name, context).await?);
            } else {
                tracing::debug!(provider_kind, supported_kinds = ?factory.provider_kinds(), "skip inapplicable environment tool factory");
            }
        }
        Ok(tools)
    }

    #[cfg(test)]
    pub(crate) fn fixed(
        flotilla_binary_path: DaemonHostPath,
        daemon_socket_path: DaemonHostPath,
        cleat_binary_path: DaemonHostPath,
        cleat_ghostty_library_path: DaemonHostPath,
        state_dir: PathBuf,
    ) -> Self {
        Self::new(vec![
            Arc::new(FlotillaCliTool { binary_path: Ok(flotilla_binary_path), daemon_socket_path: Ok(daemon_socket_path) }),
            Arc::new(CleatTool {
                binary_path: Ok(cleat_binary_path),
                ghostty_library_path: OnceCell::new_with(Some(cleat_ghostty_library_path)),
                state_root: state_dir.join("contained-cleat"),
                runner: None,
            }),
            Arc::new(RustBuildLimitsTool { state_dir }),
        ])
    }

    #[cfg(test)]
    pub(crate) fn with_unavailable_cleat(
        flotilla_binary_path: DaemonHostPath,
        daemon_socket_path: DaemonHostPath,
        error: impl Into<String>,
    ) -> Self {
        Self::new(vec![
            Arc::new(FlotillaCliTool { binary_path: Ok(flotilla_binary_path), daemon_socket_path: Ok(daemon_socket_path) }),
            Arc::new(FailingTool { name: "cleat", error: error.into() }),
        ])
    }
}

struct FlotillaCliTool {
    binary_path: Result<DaemonHostPath, String>,
    daemon_socket_path: Result<DaemonHostPath, String>,
}

#[async_trait]
impl EnvironmentToolFactory for FlotillaCliTool {
    fn provider_kinds(&self) -> &[&str] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, _environment_name: &str, _context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        let binary_path =
            self.binary_path.as_ref().map_err(|error| format!("flotilla CLI unavailable for environment provisioning: {error}"))?;
        let daemon_socket_path =
            self.daemon_socket_path.as_ref().map_err(|error| format!("flotilla CLI unavailable for environment provisioning: {error}"))?;
        let binary_directory =
            binary_path.as_path().parent().ok_or_else(|| format!("flotilla CLI binary has no parent directory: {binary_path}"))?;
        let launcher_path = daemon_socket_path
            .as_path()
            .parent()
            .ok_or_else(|| format!("daemon socket has no parent directory: {daemon_socket_path}"))?
            .join(FLOTILLA_LAUNCHER_NAME);
        ensure_flotilla_launcher(&launcher_path).await?;
        let environment_socket_path = contained_daemon_socket_path(daemon_socket_path.as_path());
        Ok(EnvironmentTool::new("flotilla", ENVIRONMENT_FLOTILLA_PATH)
            .with_asset(EnvironmentToolAsset::new(
                binary_directory.to_path_buf(),
                ENVIRONMENT_FLOTILLA_DIRECTORY,
                EnvironmentToolAssetKind::Directory,
                EnvironmentToolAssetAccess::ReadOnly,
                "the flotilla CLI",
            ))
            .with_asset(EnvironmentToolAsset::new(
                launcher_path,
                ENVIRONMENT_FLOTILLA_PATH,
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the flotilla CLI",
            ))
            .with_asset(EnvironmentToolAsset::new(
                daemon_socket_path.as_path().to_path_buf(),
                environment_socket_path.clone(),
                EnvironmentToolAssetKind::UnixSocket,
                EnvironmentToolAssetAccess::SharedWritable,
                "the daemon socket",
            ))
            .with_environment(EnvironmentVariableUpdate::set(
                "FLOTILLA_DAEMON_SOCKET",
                environment_socket_path.to_string_lossy(),
                "the daemon socket",
            ))
            .with_environment(EnvironmentVariableUpdate::set(CONTAINED_DAEMON_REQUIRED_ENV, "1", "the contained host-daemon requirement")))
    }
}

async fn ensure_flotilla_launcher(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let current = tokio::fs::read(path).await.ok();
    if current.as_deref() != Some(FLOTILLA_LAUNCHER.as_bytes()) {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("create flotilla CLI launcher directory {}: {error}", parent.display()))?;
        }
        let temporary_path = path.with_file_name(format!(".{FLOTILLA_LAUNCHER_NAME}-{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary_path, FLOTILLA_LAUNCHER)
            .await
            .map_err(|error| format!("write flotilla CLI launcher {}: {error}", temporary_path.display()))?;
        #[cfg(unix)]
        tokio::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|error| format!("make flotilla CLI launcher executable at {}: {error}", temporary_path.display()))?;
        if let Err(error) = tokio::fs::rename(&temporary_path, path).await {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(format!("publish flotilla CLI launcher at {}: {error}", path.display()));
        }
    }
    #[cfg(unix)]
    {
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .await
            .map_err(|error| format!("make flotilla CLI launcher executable at {}: {error}", path.display()))?;
    }
    Ok(())
}

fn stage_flotilla_binary(binary_path: &DaemonHostPath, staging_directory: &Path) -> Result<DaemonHostPath, String> {
    std::fs::create_dir_all(staging_directory)
        .map_err(|error| format!("create flotilla CLI staging directory {}: {error}", staging_directory.display()))?;
    let staged_path = staging_directory.join("flotilla");
    let temporary_path = staging_directory.join(format!(".flotilla-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::copy(binary_path.as_path(), &temporary_path)
        .map_err(|error| format!("stage flotilla CLI from {} to {}: {error}", binary_path.as_path().display(), temporary_path.display()))?;
    if let Err(error) = std::fs::rename(&temporary_path, &staged_path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(format!("publish staged flotilla CLI at {}: {error}", staged_path.display()));
    }
    Ok(DaemonHostPath::new(staged_path))
}

struct CleatTool {
    binary_path: Result<DaemonHostPath, String>,
    ghostty_library_path: OnceCell<DaemonHostPath>,
    state_root: PathBuf,
    runner: Option<Arc<dyn CommandRunner>>,
}

#[async_trait]
impl EnvironmentToolFactory for CleatTool {
    fn provider_kinds(&self) -> &[&str] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, environment_name: &str, _context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        let binary_path = self.binary_path.as_ref().map_err(|error| format!("cleat unavailable for environment provisioning: {error}"))?;
        let ghostty_library_path = self
            .ghostty_library_path
            .get_or_try_init(|| resolve_cleat_ghostty_library(self.runner.as_deref(), binary_path))
            .await
            .map_err(|error| format!("cleat unavailable for environment provisioning: {error}"))?;
        let state_path = self.state_root.join(environment_name);
        tokio::fs::create_dir_all(&state_path)
            .await
            .map_err(|error| format!("create durable cleat state directory {}: {error}", state_path.display()))?;

        Ok(EnvironmentTool::new("cleat", ENVIRONMENT_CLEAT_PATH)
            .with_asset(EnvironmentToolAsset::new(
                binary_path.as_path().to_path_buf(),
                ENVIRONMENT_CLEAT_PATH,
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the cleat CLI",
            ))
            .with_asset(EnvironmentToolAsset::new(
                ghostty_library_path.as_path().to_path_buf(),
                ENVIRONMENT_CLEAT_GHOSTTY_LIBRARY_PATH,
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the cleat VT library",
            ))
            .with_asset(EnvironmentToolAsset::new(
                state_path,
                ENVIRONMENT_CLEAT_RUNTIME_DIR,
                EnvironmentToolAssetKind::Directory,
                EnvironmentToolAssetAccess::SharedWritable,
                "durable cleat state",
            ))
            .with_environment(EnvironmentVariableUpdate::set("CLEAT_RUNTIME_DIR", ENVIRONMENT_CLEAT_RUNTIME_DIR, "durable cleat state"))
            .with_environment(EnvironmentVariableUpdate::prepend_path("LD_LIBRARY_PATH", ENVIRONMENT_CLEAT_LIBRARY_DIR)))
    }
}

#[cfg(test)]
struct FailingTool {
    name: &'static str,
    error: String,
}

#[cfg(test)]
#[async_trait]
impl EnvironmentToolFactory for FailingTool {
    fn provider_kinds(&self) -> &[&str] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, _environment_name: &str, _context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        Err(format!("{} unavailable for environment provisioning: {}", self.name, self.error))
    }
}

fn resolve_cleat_binary_from(path: &Path, current_dir: &Path, search_path: Option<&std::ffi::OsStr>) -> Result<DaemonHostPath, String> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else if path.components().count() > 1 {
        current_dir.join(path)
    } else {
        let search_path = search_path.ok_or_else(|| format!("resolve {}: PATH is unavailable", path.display()))?;
        env::split_paths(search_path)
            .map(|directory| directory.join(path))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| format!("resolve {}: binary is no longer present on PATH", path.display()))?
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| format!("resolve host binary {}: {error}", candidate.display()))?;
    if !canonical.is_file() {
        return Err(format!("resolved host binary is not a file: {}", canonical.display()));
    }
    let canonical = resolve_fleet_cleat_launcher(&canonical)?.unwrap_or(canonical);
    Ok(DaemonHostPath::new(canonical))
}

fn resolve_fleet_cleat_launcher(path: &Path) -> Result<Option<PathBuf>, String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut prefix = [0; FLEET_INSTALL_LAUNCHER_PREFIX.len()];
    if file.read_exact(&mut prefix).is_err() || prefix != FLEET_INSTALL_LAUNCHER_PREFIX {
        return Ok(None);
    }
    let mut command_text = String::new();
    file.read_to_string(&mut command_text).map_err(|error| format!("read fleet cleat launcher {}: {error}", path.display()))?;
    let mut lines = command_text.lines();
    let command = lines.next().ok_or_else(|| format!("fleet cleat launcher has no exec command: {}", path.display()))?;
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(format!("fleet cleat launcher has unexpected trailing content: {}", path.display()));
    }
    let encoded_target = command
        .strip_prefix("exec ")
        .and_then(|command| command.strip_suffix(" \"$@\""))
        .ok_or_else(|| format!("fleet cleat launcher has an unexpected exec command: {}", path.display()))?;
    let target = decode_bash_printf_q_word(encoded_target)
        .ok_or_else(|| format!("fleet cleat launcher target is not a supported shell word: {}", path.display()))?;
    let target = PathBuf::from(target);
    if !target.is_absolute() {
        return Err(format!("fleet cleat launcher target is not absolute: {}", target.display()));
    }
    let canonical = std::fs::canonicalize(&target)
        .map_err(|error| format!("resolve fleet cleat binary {} from launcher {}: {error}", target.display(), path.display()))?;
    if !canonical.is_file() {
        return Err(format!("resolved fleet cleat binary is not a file: {}", canonical.display()));
    }
    Ok(Some(canonical))
}

/// Decodes the ordinary one-word form emitted by Bash's `printf %q`.
///
/// Fleet install roots must be filesystem paths, so control-character paths
/// (which Bash renders using `$'...'`) are deliberately unsupported.
fn decode_bash_printf_q_word(encoded: &str) -> Option<String> {
    if encoded.is_empty() {
        return None;
    }
    let mut decoded = String::with_capacity(encoded.len());
    let mut characters = encoded.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => decoded.push(characters.next()?),
            character if character.is_whitespace() || "'\"$`;|&()<>".contains(character) => return None,
            character => decoded.push(character),
        }
    }
    Some(decoded)
}

fn resolve_cleat_binary(path: &Path) -> Result<DaemonHostPath, String> {
    let current_dir = env::current_dir().map_err(|error| format!("resolve current directory for {}: {error}", path.display()))?;
    let search_path = env::var_os("PATH");
    resolve_cleat_binary_from(path, &current_dir, search_path.as_deref())
}

#[cfg(any(target_os = "linux", test))]
fn adjacent_flotilla_binary(daemon_binary: &Path) -> Result<DaemonHostPath, String> {
    let parent = daemon_binary.parent().ok_or_else(|| format!("daemon binary has no parent directory: {}", daemon_binary.display()))?;
    let flotilla_binary = parent.join("flotilla");
    if !flotilla_binary.is_file() {
        return Err(format!("flotilla binary not found next to daemon at {}", flotilla_binary.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let permissions = std::fs::metadata(&flotilla_binary)
            .map_err(|error| format!("inspect flotilla binary at {}: {error}", flotilla_binary.display()))?
            .permissions();
        if permissions.mode() & 0o111 == 0 {
            return Err(format!("flotilla binary is not executable: {}", flotilla_binary.display()));
        }
    }
    Ok(DaemonHostPath::new(flotilla_binary))
}

fn running_daemon_flotilla_binary() -> Result<DaemonHostPath, String> {
    #[cfg(target_os = "linux")]
    {
        let daemon_binary = std::env::current_exe().map_err(|error| format!("resolve running daemon binary: {error}"))?;
        adjacent_flotilla_binary(&daemon_binary)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("daemon-adjacent flotilla injection requires a Linux host; generation-addressed environment binaries are not available yet"
            .to_string())
    }
}

async fn resolve_cleat_ghostty_library(
    runner: Option<&dyn CommandRunner>,
    cleat_binary_path: &DaemonHostPath,
) -> Result<DaemonHostPath, String> {
    if let Some(generation_root) = cleat_binary_path.as_path().parent().and_then(Path::parent) {
        let bundled_library = generation_root.join("lib").join(CLEAT_GHOSTTY_LIBRARY);
        if bundled_library.is_file() {
            let canonical = std::fs::canonicalize(&bundled_library)
                .map_err(|error| format!("resolve bundled cleat runtime library {}: {error}", bundled_library.display()))?;
            return Ok(DaemonHostPath::new(canonical));
        }
    }
    let runner = runner.ok_or_else(|| "local command runner unavailable for cleat asset discovery".to_string())?;
    let binary = cleat_binary_path.as_path().to_string_lossy().into_owned();
    let output = runner.run_output("ldd", &[&binary], Path::new("/"), &ChannelLabel::Default).await?;
    if !output.success {
        return Err(format!("inspect cleat runtime libraries: {}", output.stderr.trim()));
    }
    let path = output.stdout.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(CLEAT_GHOSTTY_LIBRARY) && fields.next() == Some("=>"))
            .then(|| fields.next())
            .flatten()
            .filter(|path| *path != "not")
    });
    let path = path.ok_or_else(|| format!("cleat runtime library {CLEAT_GHOSTTY_LIBRARY} was not resolved by ldd"))?;
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(format!("cleat runtime library resolved to a non-absolute path: {}", path.display()));
    }
    Ok(DaemonHostPath::new(path))
}

/// Cargo invokes this with the rustc path as its first argument. Keeping the
/// linker option in a wrapper leaves repository Cargo config and RUSTFLAGS to
/// Cargo's own resolution rules instead of replacing either source. The cap is
/// limited to x86_64 Linux, where rustc's default lld path is known here.
pub(crate) const RUSTC_LINKER_WRAPPER: &str = r#"#!/bin/sh
compiler=$1
shift
if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ]; then
  # rustc defaults to lld on x86_64-unknown-linux-gnu. Respect a repository
  # that explicitly selects another linker or disables lld.
  linker=default
  target=host
  target_next=0
  for arg in "$@"; do
    if [ "$target_next" = 1 ]; then
      target=$arg
      target_next=0
    fi
    case "$arg" in
      --target) target_next=1 ;;
      --target=*) target=${arg#--target=} ;;
      *fuse-ld=lld*|*linker-features=+lld*) linker=lld ;;
      *linker-features=-lld*|*fuse-ld=*) linker=other ;;
      *linker=*)
        case "$arg" in *lld*) linker=lld ;; *) linker=other ;; esac ;;
    esac
  done
  if [ -n "${FLOTILLA_LINKER_THREADS:-}" ] && { [ "$target" = host ] || [ "$target" = x86_64-unknown-linux-gnu ]; } && { [ "$linker" = default ] || [ "$linker" = lld ]; }; then
    exec "$compiler" "$@" -C "link-arg=-Wl,--threads=$FLOTILLA_LINKER_THREADS"
  fi
fi
exec "$compiler" "$@"
"#;

pub(crate) const CONTAINED_RUSTC_WRAPPER_PATH: &str = "/usr/local/bin/flotilla-rustc-wrapper";

pub(crate) const CONTAINED_CARGO_SHIM_DIRECTORY: &str = "/usr/local/lib/flotilla-rust-build-limits";
pub(crate) const CONTAINED_CARGO_SHIM_PATH: &str = "/usr/local/lib/flotilla-rust-build-limits/cargo";
pub(crate) const CARGO_BUILD_PROFILE_SHIM: &str = r#"#!/bin/sh
# flotilla-cargo-profile-shim
# Remove our directory before resolving Cargo (including rustup's Cargo proxy).
case "$0" in
  */*) shim_dir=${0%/*} ;;
  *) shim_dir=. ;;
esac
shim_dir=$(CDPATH= cd -- "$shim_dir" && pwd -P) || exit 127
remaining=$PATH
filtered=
while :; do
  entry=${remaining%%:*}
  # Empty entries mean cwd. Preserve their spelling unless they lead back
  # to this shim, just as for trailing slashes and directory symlinks.
  entry_dir=$(CDPATH= cd -- "${entry:-.}" 2>/dev/null && pwd -P) || entry_dir=$entry
  if [ "$entry_dir" != "$shim_dir" ]; then
    filtered=$filtered:$entry
  fi
  case "$remaining" in
    *:*) remaining=${remaining#*:} ;;
    *) break ;;
  esac
done
# Resolve with a filtered PATH, but preserve the caller's PATH for Cargo's
# subprocesses and external subcommands that may invoke Cargo themselves.
missing_cargo() {
  echo "flotilla cargo profile shim: no Cargo executable found after removing the shim directory from PATH" >&2
  exit 127
}
[ -n "$filtered" ] || missing_cargo
cargo=$(PATH=${filtered#:} command -v cargo) || missing_cargo
# A different directory may contain a file symlink to this same shim.
# The contained sh must support test -ef (dash, bash, BusyBox and macOS sh do).
[ "$cargo" -ef "$0" ] && missing_cargo
# rustup's +toolchain selector must precede Cargo options.
case "${1:-}" in
  +*) toolchain=$1; shift
      exec "$cargo" "$toolchain" --config 'profile.dev.package."*".debug=0' "$@" ;;
  *) exec "$cargo" --config 'profile.dev.package."*".debug=0' "$@" ;;
esac
"#;

fn stage_local_cargo_shim(state_dir: &Path) -> Result<PathBuf, String> {
    stage_local_build_tool(state_dir, "cargo-profile-shim", CARGO_BUILD_PROFILE_SHIM)
}

pub(crate) fn stage_local_rustc_wrapper(state_dir: &Path) -> Result<PathBuf, String> {
    stage_local_build_tool(state_dir, "rustc-linker-cap", RUSTC_LINKER_WRAPPER)
}

pub(crate) fn stage_local_build_tool(state_dir: &Path, name: &str, contents: &str) -> Result<PathBuf, String> {
    let directory = state_dir.join("environment-tools");
    std::fs::create_dir_all(&directory).map_err(|error| format!("create build tool directory: {error}"))?;
    let path = directory.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(contents) {
        return Ok(path);
    }
    let staged = directory.join(format!("{name}-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&staged, contents).map_err(|error| format!("stage build tool {name}: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("mark build tool {name} executable: {error}"))?;
    }
    std::fs::rename(&staged, &path).map_err(|error| format!("install build tool {name}: {error}"))?;
    Ok(path)
}

pub(crate) async fn stage_local_rustc_wrapper_async(state_dir: PathBuf) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || stage_local_rustc_wrapper(&state_dir))
        .await
        .map_err(|error| format!("stage rustc wrapper task: {error}"))?
}

impl FlotillaCliTool {
    fn for_local_host(context: &LocalToolContext<'_>) -> Arc<dyn EnvironmentToolFactory> {
        Arc::new(Self {
            binary_path: running_daemon_flotilla_binary()
                .and_then(|path| stage_flotilla_binary(&path, context.config.state_dir().join("environment-tools/flotilla-bin").as_path())),
            daemon_socket_path: context.daemon_socket_path.clone().ok_or_else(|| "daemon socket path unavailable".to_string()),
        })
    }
}
impl CleatTool {
    fn for_local_host(context: &LocalToolContext<'_>) -> Arc<dyn EnvironmentToolFactory> {
        Arc::new(Self {
            binary_path: context
                .daemon
                .local_environment_bag()
                .and_then(|bag| bag.find_binary("cleat").cloned())
                .map(|path| resolve_cleat_binary(path.as_path()))
                .transpose()
                .and_then(|path| path.ok_or_else(|| "binary unavailable for contained environment delivery".to_string())),
            ghostty_library_path: OnceCell::new(),
            state_root: context.config.state_dir().join("contained-cleat").as_path().to_path_buf(),
            runner: context.daemon.local_command_runner(),
        })
    }
}
struct RustBuildLimitsTool {
    state_dir: PathBuf,
}
impl RustBuildLimitsTool {
    fn for_local_host(context: &LocalToolContext<'_>) -> Arc<dyn EnvironmentToolFactory> {
        Arc::new(Self { state_dir: context.config.state_dir().as_path().to_path_buf() })
    }
}
#[async_trait]
impl EnvironmentToolFactory for RustBuildLimitsTool {
    fn provider_kinds(&self) -> &[&str] {
        &[DOCKER_PROVIDER_KIND]
    }
    async fn prepare(&self, _environment_name: &str, context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
        let jobs = context.rust_build_jobs().await?;
        let state_dir = self.state_dir.clone();
        let (wrapper_host, cargo_shim_host) = tokio::task::spawn_blocking(move || {
            Ok::<_, String>((stage_local_rustc_wrapper(&state_dir)?, stage_local_cargo_shim(&state_dir)?))
        })
        .await
        .map_err(|error| format!("stage Rust build limits task: {error}"))??;
        Ok(EnvironmentTool::new("rust-build-limits", CONTAINED_RUSTC_WRAPPER_PATH)
            .with_asset(EnvironmentToolAsset::new(
                wrapper_host,
                CONTAINED_RUSTC_WRAPPER_PATH,
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the Rust linker cap",
            ))
            .with_asset(EnvironmentToolAsset::new(
                cargo_shim_host,
                CONTAINED_CARGO_SHIM_PATH,
                EnvironmentToolAssetKind::File,
                EnvironmentToolAssetAccess::ReadOnly,
                "the Rust dependency debug profile",
            ))
            .with_environment(EnvironmentVariableUpdate::prepend_path("PATH", CONTAINED_CARGO_SHIM_DIRECTORY))
            .with_environment(EnvironmentVariableUpdate::set(
                "CARGO_PROFILE_DEV_DEBUG",
                "line-tables-only",
                "the Rust workspace debug profile",
            ))
            .with_environment(EnvironmentVariableUpdate::set(
                "RUSTC_WORKSPACE_WRAPPER",
                CONTAINED_RUSTC_WRAPPER_PATH,
                "the Rust linker cap",
            ))
            .with_environment(EnvironmentVariableUpdate::set("CARGO_BUILD_JOBS", jobs.to_string(), "the Rust build share"))
            .with_environment(EnvironmentVariableUpdate::set("FLOTILLA_LINKER_THREADS", jobs.to_string(), "the Rust linker cap")))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{
            atomic::{AtomicUsize, Ordering},
            LazyLock,
        },
    };

    use flotilla_core::providers::discovery::test_support::DiscoveryMockRunner;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolves_detected_host_binary_names_through_path() {
        let temp = TempDir::new().expect("tempdir");
        let binary = temp.path().join("cleat");
        fs::write(&binary, b"test binary").expect("write binary");
        let search_path = env::join_paths([temp.path()]).expect("search path");

        let resolved =
            resolve_cleat_binary_from(Path::new("cleat"), Path::new("/not-used"), Some(&search_path)).expect("resolve detected binary");

        assert_eq!(resolved.as_path(), binary.canonicalize().expect("canonical binary"));
    }

    #[tokio::test]
    async fn prepares_cleat_from_a_fleet_launcher() {
        let temp = TempDir::new().expect("tempdir");
        let fleet_root = temp.path().join("fleet root");
        let generation = fleet_root.join("releases/generation-1");
        let binary = generation.join("bin/cleat");
        let library = generation.join("lib/libghostty-vt.so.0");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("generation binary directory");
        fs::create_dir_all(library.parent().expect("library parent")).expect("generation library directory");
        fs::write(&binary, b"cleat binary").expect("generation binary");
        fs::write(&library, b"ghostty library").expect("generation library");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&generation, fleet_root.join("current")).expect("current generation link");

        let launcher_directory = temp.path().join("bin");
        fs::create_dir_all(&launcher_directory).expect("launcher directory");
        let launcher = launcher_directory.join("cleat");
        let encoded_root = fleet_root.to_string_lossy().replace(' ', "\\ ");
        fs::write(&launcher, format!("#!/usr/bin/env bash\n{FLEET_INSTALL_MARKER}\nexec {encoded_root}/current/bin/cleat \"$@\"\n"))
            .expect("fleet launcher");
        let search_path = env::join_paths([launcher_directory]).expect("search path");

        let resolved = resolve_cleat_binary_from(Path::new("cleat"), Path::new("/not-used"), Some(&search_path))
            .expect("resolve generation binary from launcher");
        let tool = CleatTool {
            binary_path: Ok(resolved),
            ghostty_library_path: OnceCell::new(),
            state_root: temp.path().join("state"),
            runner: None,
        }
        .prepare("contained-work", &TestToolContext)
        .await
        .expect("prepare cleat from fleet launcher");

        assert_eq!(tool.assets[0].host_path.as_path(), binary.canonicalize().expect("canonical generation binary"));
        assert_eq!(tool.assets[1].host_path.as_path(), library.canonicalize().expect("canonical generation library"));
    }

    #[test]
    fn rejects_a_fleet_cleat_launcher_with_a_relative_target() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = temp.path().join("cleat");
        fs::write(&launcher, format!("#!/usr/bin/env bash\n{FLEET_INSTALL_MARKER}\nexec relative/bin/cleat \"$@\"\n"))
            .expect("fleet launcher");

        let error = resolve_fleet_cleat_launcher(&launcher).expect_err("relative launcher target should fail");

        assert_eq!(error, "fleet cleat launcher target is not absolute: relative/bin/cleat");
    }

    #[test]
    fn rejects_unexpected_commands_in_a_fleet_cleat_launcher() {
        let temp = TempDir::new().expect("tempdir");
        let launcher = temp.path().join("cleat");
        fs::write(
            &launcher,
            format!("#!/usr/bin/env bash\n{FLEET_INSTALL_MARKER}\nexec /fleet/current/bin/cleat \"$@\"\necho unexpected\n"),
        )
        .expect("fleet launcher");

        let error = resolve_fleet_cleat_launcher(&launcher).expect_err("extra launcher command should fail");

        assert_eq!(error, format!("fleet cleat launcher has unexpected trailing content: {}", launcher.display()));
    }

    #[test]
    fn resolves_flotilla_binary_adjacent_to_daemon() {
        let temp = TempDir::new().expect("tempdir");
        let daemon_binary = temp.path().join("flotillad");
        let flotilla_binary = temp.path().join("flotilla");
        fs::write(&daemon_binary, b"daemon").expect("write daemon");
        fs::write(&flotilla_binary, b"cli").expect("write cli");
        fs::set_permissions(&flotilla_binary, fs::Permissions::from_mode(0o755)).expect("make cli executable");

        assert_eq!(adjacent_flotilla_binary(&daemon_binary).expect("adjacent flotilla binary"), DaemonHostPath::new(flotilla_binary),);
    }

    #[test]
    fn rejects_non_executable_flotilla_binary_adjacent_to_daemon() {
        let temp = TempDir::new().expect("tempdir");
        let daemon_binary = temp.path().join("flotillad");
        let flotilla_binary = temp.path().join("flotilla");
        fs::write(&daemon_binary, "").expect("daemon binary");
        fs::write(&flotilla_binary, "").expect("flotilla binary");

        let error = adjacent_flotilla_binary(&daemon_binary).expect_err("non-executable flotilla binary should be rejected");

        assert_eq!(error, format!("flotilla binary is not executable: {}", flotilla_binary.display()));
    }

    #[test]
    fn stages_only_the_flotilla_cli_for_contained_directory_mounts() {
        let temp = TempDir::new().expect("tempdir");
        let source_directory = temp.path().join("target/debug");
        let staging_directory = temp.path().join("state/environment-tools/flotilla-bin");
        fs::create_dir_all(&source_directory).expect("source directory");
        let source = source_directory.join("flotilla");
        fs::write(&source, b"old cli").expect("source CLI");
        fs::write(source_directory.join("unrelated-test-binary"), b"must not be exposed").expect("unrelated binary");

        let staged = stage_flotilla_binary(&DaemonHostPath::new(&source), &staging_directory).expect("stage CLI");

        assert_eq!(staged.as_path(), staging_directory.join("flotilla"));
        assert_eq!(fs::read(staged.as_path()).expect("staged CLI"), b"old cli");
        assert_eq!(fs::read_dir(&staging_directory).expect("staging directory").count(), 1);

        fs::write(&source, b"new cli").expect("replace source CLI");
        stage_flotilla_binary(&DaemonHostPath::new(source), &staging_directory).expect("restage CLI");
        assert_eq!(fs::read(staged.as_path()).expect("updated staged CLI"), b"new cli");
    }

    #[tokio::test]
    async fn flotilla_cli_uses_a_directory_mount_for_upgrade_visibility() {
        let temp = TempDir::new().expect("tempdir");
        let socket_path = temp.path().join("flotilla.sock");
        let tool = FlotillaCliTool {
            binary_path: Ok(DaemonHostPath::new("/opt/flotilla/bin/flotilla")),
            daemon_socket_path: Ok(DaemonHostPath::new(socket_path)),
        }
        .prepare("contained-work", &TestToolContext)
        .await
        .expect("prepare flotilla CLI");

        assert_eq!(tool.executable.as_path(), Path::new("/usr/local/bin/flotilla"));
        assert_eq!(tool.assets[0].host_path.as_path(), Path::new("/opt/flotilla/bin"));
        assert_eq!(tool.assets[0].environment_path.as_path(), Path::new("/opt/flotilla/bin"));
        assert_eq!(tool.assets[0].kind, EnvironmentToolAssetKind::Directory);
        assert_eq!(tool.assets[1].environment_path.as_path(), Path::new("/usr/local/bin/flotilla"));
        assert_eq!(fs::read_to_string(tool.assets[1].host_path.as_path()).expect("read launcher"), FLOTILLA_LAUNCHER);
    }

    #[tokio::test]
    async fn resolves_the_cleat_vt_library_from_the_host_asset_set() {
        let temp = TempDir::new().expect("tempdir");
        let binary = temp.path().join("direct/bin/cleat");
        let library = temp.path().join("runtime/libghostty-vt.so.0");
        let binary_arg = binary.to_string_lossy().into_owned();
        let library_arg = library.to_string_lossy().into_owned();
        let runner = DiscoveryMockRunner::builder()
            .on_run("ldd", &[&binary_arg], Ok(format!("\tlibghostty-vt.so.0 => {library_arg} (0x00007f)\n")))
            .build();

        let resolved = resolve_cleat_ghostty_library(Some(&runner), &DaemonHostPath::new(binary)).await.expect("resolve ghostty library");

        assert_eq!(resolved, DaemonHostPath::new(library));
    }

    #[tokio::test]
    async fn resolves_the_cleat_vt_library_from_the_generation_bundle() {
        let temp = TempDir::new().expect("tempdir");
        let binary = temp.path().join("generation/bin/cleat");
        let library = temp.path().join("generation/lib/libghostty-vt.so.0");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("binary directory");
        fs::create_dir_all(library.parent().expect("library parent")).expect("library directory");
        fs::write(&binary, b"cleat binary").expect("cleat binary");
        fs::write(&library, b"ghostty library").expect("ghostty library");
        let resolved = resolve_cleat_ghostty_library(None, &DaemonHostPath::new(binary)).await.expect("resolve bundled ghostty library");

        assert_eq!(resolved.as_path(), library.canonicalize().expect("canonical bundled library"));
    }

    struct TestToolContext;
    #[async_trait]
    impl EnvironmentToolContext for TestToolContext {
        async fn rust_build_jobs(&self) -> Result<usize, String> {
            Ok(2)
        }
    }

    #[derive(Default)]
    struct FourthTool {
        preparations: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl EnvironmentToolFactory for FourthTool {
        fn provider_kinds(&self) -> &[&str] {
            &[DOCKER_PROVIDER_KIND]
        }
        async fn prepare(&self, _name: &str, _context: &dyn EnvironmentToolContext) -> Result<EnvironmentTool, String> {
            self.preparations.fetch_add(1, Ordering::Relaxed);
            Ok(EnvironmentTool::new("fourth", "/usr/local/bin/fourth"))
        }
    }
    pub(crate) fn with_fourth_tool(mut provisioner: EnvironmentToolProvisioner) -> EnvironmentToolProvisioner {
        provisioner.factories.push(Arc::new(FourthTool::default()));
        provisioner
    }

    // The shim is production-private; existing runtime script tests share staging
    // through this test-only adapter.
    pub(crate) fn stage_cargo_shim(state_dir: &Path) -> Result<PathBuf, String> {
        stage_local_cargo_shim(state_dir)
    }

    // Behaviour: only applicable factories prepare, including failures. Empty
    // registries and duplicate enrollments preserve ordered delivery. Preparation stops at the first applicable failure.
    #[hegel::test]
    fn filters_registered_tools(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Cover empty through duplicate registration, supported and unsupported kinds,
        // and failure at every position (before, between, or after successful factories).
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let docker = tc.draw(gs::booleans());
        let fail = tc.draw(gs::booleans());
        let fail_at = tc.draw(gs::integers::<usize>().min_value(0).max_value(count));
        let preparations = Arc::new(AtomicUsize::new(0));
        let mut factories: Vec<Arc<dyn EnvironmentToolFactory>> = (0..count)
            .map(|_| Arc::new(FourthTool { preparations: Arc::clone(&preparations) }) as Arc<dyn EnvironmentToolFactory>)
            .collect();
        if fail {
            factories.insert(fail_at, Arc::new(FailingTool { name: "test", error: "unavailable".into() }));
        }
        static RUNTIME: LazyLock<tokio::runtime::Runtime> =
            LazyLock::new(|| tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime"));
        let result = RUNTIME.block_on(EnvironmentToolProvisioner::new(factories).prepare(
            if docker { DOCKER_PROVIDER_KIND } else { "other" },
            "work",
            &TestToolContext,
        ));
        if docker && fail {
            assert!(result.unwrap_err().contains("unavailable"));
        } else {
            assert_eq!(result.unwrap().iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), vec![
                "fourth";
                if docker { count } else { 0 }
            ]);
        }
        assert_eq!(
            preparations.load(Ordering::Relaxed),
            if !docker {
                0
            } else if fail {
                fail_at
            } else {
                count
            }
        );
    }
}
