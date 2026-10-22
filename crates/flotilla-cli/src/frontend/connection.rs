//! Connection-only CLI plumbing shared with the contained executable. That
//! executable always requires the host daemon and never starts one in a vessel.
use crate::cli::args::{Cli, CliPaths};
use color_eyre::Result;
use flotilla_client::{DaemonEndpoint, SshEndpoint};
use flotilla_daemon_api::daemon::DaemonHandle;
use std::{path::Path, sync::Arc};

pub fn host_daemon_socket_required(marker: Option<&std::ffi::OsStr>) -> bool {
    marker.is_some()
}
pub fn cli_surface_from(role: Option<&str>, namespace: Option<&str>) -> flotilla_protocol::SurfaceDeclaration {
    let namespace = namespace.unwrap_or("flotilla");
    let principal_ref = match role.filter(|role| !role.trim().is_empty()) {
        Some(role) => flotilla_protocol::PrincipalRef { namespace: namespace.into(), name: format!("{role} agent") },
        None => flotilla_protocol::PrincipalRef::implicit_for_namespace(namespace),
    };
    flotilla_protocol::SurfaceDeclaration { principal_ref, character: flotilla_protocol::SurfaceCharacter::Focal }
}
#[cfg(unix)]
pub fn reset_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
#[cfg(not(unix))]
pub fn reset_sigpipe() {}

pub async fn connect_cli_socket(
    remote: Option<&SshEndpoint>,
    socket: &Path,
    config: &Path,
    state: &Path,
    require: bool,
) -> Result<Arc<flotilla_client::SocketDaemon>, String> {
    let namespace = std::env::var("FLOTILLA_NAMESPACE").unwrap_or_else(|_| "flotilla".into());
    let principal_ref = match std::env::var("FLOTILLA_CREW_ROLE").ok().filter(|role| !role.trim().is_empty()) {
        Some(role) => flotilla_protocol::PrincipalRef { namespace, name: format!("{role} agent") },
        None => flotilla_protocol::PrincipalRef::implicit_for_namespace(&namespace),
    };
    let endpoint = remote.cloned().map(DaemonEndpoint::Ssh).unwrap_or_else(|| DaemonEndpoint::Local(socket.into()));
    flotilla_client::connect_endpoint_or_spawn_with_surface(
        &endpoint,
        config,
        state,
        require,
        flotilla_protocol::SurfaceDeclaration { principal_ref, character: flotilla_protocol::SurfaceCharacter::Focal },
    )
    .await
}
pub async fn connect_daemon(cli: &Cli) -> Result<Arc<dyn DaemonHandle>> {
    let CliPaths { config_dir, state_dir, socket_path } = cli.client_paths().map_err(color_eyre::eyre::Report::msg)?;
    let remote = cli.remote_daemon().map_err(color_eyre::eyre::Report::msg)?;
    Ok(connect_cli_socket(
        remote.as_ref(),
        &socket_path,
        &config_dir,
        &state_dir,
        host_daemon_socket_required(std::env::var_os("FLOTILLA_CONTAINED_HOST_DAEMON").as_deref()),
    )
    .await
    .map_err(color_eyre::eyre::Report::msg)?)
}
