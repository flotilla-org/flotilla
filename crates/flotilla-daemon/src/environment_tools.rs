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
            EnvironmentVariableUpdate,
        },
        ChannelLabel, CommandRunner,
    },
};
use tokio::sync::OnceCell;

pub(crate) const ENVIRONMENT_FLOTILLA_PATH: &str = "/usr/local/bin/flotilla";
#[cfg(test)]
pub(crate) const ENVIRONMENT_DAEMON_SOCKET_PATH: &str = "/run/flotilla-daemon/flotilla.sock";
pub(crate) const ENVIRONMENT_CLEAT_PATH: &str = "/usr/local/bin/cleat";
pub(crate) const ENVIRONMENT_CLEAT_LIBRARY_DIR: &str = "/usr/local/lib/flotilla";
pub(crate) const ENVIRONMENT_CLEAT_GHOSTTY_LIBRARY_PATH: &str = "/usr/local/lib/flotilla/libghostty-vt.so.0";
pub(crate) const ENVIRONMENT_CLEAT_RUNTIME_DIR: &str = "/var/lib/flotilla/cleat";
const CLEAT_GHOSTTY_LIBRARY: &str = "libghostty-vt.so.0";

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
        let runner = daemon.local_command_runner();
        Self::new(vec![
            Arc::new(FlotillaCliTool {
                binary_path: running_daemon_flotilla_binary(),
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
        let environment_socket_path = contained_daemon_socket_path(daemon_socket_path.as_path());
        Ok(EnvironmentTool::new("flotilla", ENVIRONMENT_FLOTILLA_PATH)
            .with_asset(EnvironmentToolAsset::new(
                binary_path.as_path().to_path_buf(),
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
            )))
    }
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
            .get_or_try_init(|| async {
                let runner =
                    self.runner.as_ref().ok_or_else(|| "local command runner unavailable for cleat asset discovery".to_string())?;
                resolve_cleat_ghostty_library(&**runner, binary_path).await
            })
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
    Ok(DaemonHostPath::new(canonical))
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

async fn resolve_cleat_ghostty_library(runner: &dyn CommandRunner, cleat_binary_path: &DaemonHostPath) -> Result<DaemonHostPath, String> {
    let binary = cleat_binary_path.as_path().to_string_lossy().into_owned();
    let output = runner.run_output("ldd", &[&binary], Path::new("/"), &ChannelLabel::Noop).await?;
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

    #[tokio::test]
    async fn resolves_the_cleat_vt_library_from_the_host_asset_set() {
        let runner = DiscoveryMockRunner::builder()
            .on_run(
                "ldd",
                &["/opt/flotilla/bin/cleat"],
                Ok("\tlibghostty-vt.so.0 => /opt/flotilla/lib/libghostty-vt.so.0 (0x00007f)\n".to_string()),
            )
            .build();

        let library =
            resolve_cleat_ghostty_library(&runner, &DaemonHostPath::new("/opt/flotilla/bin/cleat")).await.expect("resolve ghostty library");

        assert_eq!(library, DaemonHostPath::new("/opt/flotilla/lib/libghostty-vt.so.0"));
    }
}
