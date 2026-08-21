use std::{
    env,
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
const FLEET_INSTALL_MARKER: &str = "# managed by fleet-install";

#[async_trait]
pub(crate) trait EnvironmentToolFactory: Send + Sync {
    async fn prepare(&self, environment_name: &str) -> Result<EnvironmentTool, String>;
}

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
        let cleat_binary_path = daemon
            .local_environment_bag()
            .and_then(|bag| bag.find_binary("cleat").cloned())
            .map(|path| resolve_host_binary(path.as_path()))
            .transpose()
            .and_then(|path| path.ok_or_else(|| "binary unavailable for contained environment delivery".to_string()));
        let flotilla_binary_path = running_daemon_flotilla_binary()
            .and_then(|path| stage_flotilla_binary(&path, config.state_dir().join("environment-tools/flotilla-bin").as_path()));
        let runner = daemon.local_command_runner();
        Self::new(vec![
            Arc::new(FlotillaCliTool {
                binary_path: flotilla_binary_path,
                daemon_socket_path: daemon_socket_path.ok_or_else(|| "daemon socket path unavailable".to_string()),
            }),
            Arc::new(CleatTool {
                binary_path: cleat_binary_path,
                ghostty_library_path: OnceCell::new(),
                state_root: config.state_dir().join("contained-cleat").as_path().to_path_buf(),
                runner,
            }),
        ])
    }

    pub(crate) fn new(factories: Vec<Arc<dyn EnvironmentToolFactory>>) -> Self {
        Self { factories }
    }

    pub(crate) async fn prepare(&self, environment_name: &str) -> Result<Vec<EnvironmentTool>, String> {
        let mut tools = Vec::with_capacity(self.factories.len());
        for factory in &self.factories {
            tools.push(factory.prepare(environment_name).await?);
        }
        Ok(tools)
    }

    #[cfg(test)]
    pub(crate) fn fixed(
        flotilla_binary_path: DaemonHostPath,
        daemon_socket_path: DaemonHostPath,
        cleat_binary_path: DaemonHostPath,
        cleat_ghostty_library_path: DaemonHostPath,
        state_root: PathBuf,
    ) -> Self {
        Self::new(vec![
            Arc::new(FlotillaCliTool { binary_path: Ok(flotilla_binary_path), daemon_socket_path: Ok(daemon_socket_path) }),
            Arc::new(CleatTool {
                binary_path: Ok(cleat_binary_path),
                ghostty_library_path: OnceCell::new_with(Some(cleat_ghostty_library_path)),
                state_root,
                runner: None,
            }),
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
    async fn prepare(&self, _environment_name: &str) -> Result<EnvironmentTool, String> {
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
    async fn prepare(&self, environment_name: &str) -> Result<EnvironmentTool, String> {
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
    async fn prepare(&self, _environment_name: &str) -> Result<EnvironmentTool, String> {
        Err(format!("{} unavailable for environment provisioning: {}", self.name, self.error))
    }
}

fn resolve_host_binary_from(path: &Path, current_dir: &Path, search_path: Option<&std::ffi::OsStr>) -> Result<DaemonHostPath, String> {
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
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut lines = contents.lines();
    if lines.next() != Some("#!/usr/bin/env bash") || lines.next() != Some(FLEET_INSTALL_MARKER) {
        return Ok(None);
    }
    let command = lines.next().ok_or_else(|| format!("fleet cleat launcher has no exec command: {}", path.display()))?;
    if lines.next().is_some() {
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

fn resolve_host_binary(path: &Path) -> Result<DaemonHostPath, String> {
    let current_dir = env::current_dir().map_err(|error| format!("resolve current directory for {}: {error}", path.display()))?;
    let search_path = env::var_os("PATH");
    resolve_host_binary_from(path, &current_dir, search_path.as_deref())
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

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

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
            resolve_host_binary_from(Path::new("cleat"), Path::new("/not-used"), Some(&search_path)).expect("resolve detected binary");

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

        let resolved = resolve_host_binary_from(Path::new("cleat"), Path::new("/not-used"), Some(&search_path))
            .expect("resolve generation binary from launcher");
        let tool = CleatTool {
            binary_path: Ok(resolved),
            ghostty_library_path: OnceCell::new(),
            state_root: temp.path().join("state"),
            runner: None,
        }
        .prepare("contained-work")
        .await
        .expect("prepare cleat from fleet launcher");

        assert_eq!(tool.assets[0].host_path.as_path(), binary.canonicalize().expect("canonical generation binary"));
        assert_eq!(tool.assets[1].host_path.as_path(), library.canonicalize().expect("canonical generation library"));
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
        .prepare("contained-work")
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
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "ldd",
                &["/opt/flotilla/bin/cleat"],
                Ok("\tlibghostty-vt.so.0 => /opt/flotilla/lib/libghostty-vt.so.0 (0x00007f)\n".to_string()),
            )
            .build();

        let library = resolve_cleat_ghostty_library(Some(&runner), &DaemonHostPath::new("/opt/flotilla/bin/cleat"))
            .await
            .expect("resolve ghostty library");

        assert_eq!(library, DaemonHostPath::new("/opt/flotilla/lib/libghostty-vt.so.0"));
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
}
