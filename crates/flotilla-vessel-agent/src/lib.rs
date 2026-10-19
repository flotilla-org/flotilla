//! Resident vessel-agent startup. Transport listeners are added in the next slice.
//!
//! The agent deliberately has no store, reconciliation or provider dependencies.

use std::{ffi::OsString, future::Future, path::PathBuf};

/// Identity and parent link injected by the environment provider.
#[derive(Debug, PartialEq, Eq)]
pub struct Config {
    pub environment_id: String,
    pub daemon_socket: PathBuf,
}

impl Config {
    /// Read the existing vessel environment contract without opening the link.
    /// The injected lookup keeps configuration tests independent of process globals.
    pub fn from_env(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, String> {
        let environment_id = lookup("FLOTILLA_ENVIRONMENT_ID")
            .ok_or("FLOTILLA_ENVIRONMENT_ID is required")?
            .into_string()
            .map_err(|_| "FLOTILLA_ENVIRONMENT_ID must be UTF-8")?;
        if environment_id.trim().is_empty() {
            return Err("FLOTILLA_ENVIRONMENT_ID must not be empty".into());
        }
        let daemon_socket = PathBuf::from(lookup("FLOTILLA_DAEMON_SOCKET").ok_or("FLOTILLA_DAEMON_SOCKET is required")?);
        if !daemon_socket.is_absolute() {
            return Err("FLOTILLA_DAEMON_SOCKET must be an absolute path".into());
        }
        Ok(Self { environment_id, daemon_socket })
    }
}

/// Start the skeleton, log to stderr, and stay resident until interrupted.
/// No readiness/hello is advertised until listeners exist in the next slice.
pub async fn run() -> Result<(), String> {
    let config = Config::from_env(|key| std::env::var_os(key))?;
    let directives = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "info".into(),
        Err(error) => return Err(format!("invalid RUST_LOG: {error}")),
    };
    let filter = tracing_subscriber::EnvFilter::try_new(directives).map_err(|error| format!("invalid RUST_LOG: {error}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init()
        .map_err(|error| format!("initialize vessel-agent logging: {error}"))?;
    let shutdown = shutdown_signal()?;
    tracing::info!(environment_id = %config.environment_id, daemon_socket = %config.daemon_socket.display(), "vessel agent started (skeleton; no listeners)");
    shutdown.await?;
    tracing::info!("vessel agent stopped");
    Ok(())
}

fn shutdown_signal() -> Result<impl Future<Output = Result<(), String>>, String> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // Install both handlers before logging startup: a supervisor may signal
        // immediately. Installation failure must fail startup rather than leave
        // an apparently healthy resident that cannot honor shutdown.
        let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
        let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => Ok(()),
                _ = terminate.recv() => Ok(()),
            }
        })
    }
    #[cfg(not(unix))]
    // A Ctrl-C handler installation failure propagates as a process failure.
    Ok(async { tokio::signal::ctrl_c().await.map_err(|error| error.to_string()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR 0069 startup uses the provider-injected identity and parent socket.
    // Exhaust the finite present/missing/empty/invalid cases; no process globals.
    #[test]
    fn configuration_requires_identity_and_absolute_parent_link() {
        let absolute = if cfg!(windows) { "C:/run/flotilla/parent.sock" } else { "/run/flotilla/parent.sock" };
        for identity in [None, Some(""), Some("  "), Some("env-vessel")] {
            for socket in [None, Some(""), Some("relative.sock"), Some(absolute)] {
                let result = Config::from_env(|key| match key {
                    "FLOTILLA_ENVIRONMENT_ID" => identity.map(OsString::from),
                    "FLOTILLA_DAEMON_SOCKET" => socket.map(OsString::from),
                    _ => None,
                });
                if identity == Some("env-vessel") && socket == Some(absolute) {
                    assert_eq!(
                        result.expect("valid vessel config"),
                        Config { environment_id: "env-vessel".into(), daemon_socket: absolute.into() }
                    );
                } else {
                    assert!(result.is_err(), "identity={identity:?}, socket={socket:?}");
                }
            }
        }
    }

    // A Unix socket path may contain non-UTF-8 bytes; the logged identity may not.
    #[cfg(unix)]
    #[test]
    fn configuration_preserves_native_socket_paths_and_rejects_non_utf8_identity() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/run/parent-\xff.sock".to_vec());
        let config = Config::from_env(|key| Some(if key == "FLOTILLA_ENVIRONMENT_ID" { "env".into() } else { path.clone() }))
            .expect("native socket path");
        assert_eq!(config.daemon_socket, PathBuf::from(path));
        assert!(Config::from_env(|_| Some(OsString::from_vec(vec![0xff]))).is_err());
    }
}
