//! Path policy: resolves daemon-side directory locations from environment variables.
//!
//! Resolution order per category:
//! 1. `FLOTILLA_ROOT` → `<root>/<category>`
//! 2. Category-specific XDG env var (`XDG_CONFIG_HOME`, etc.)
//! 3. XDG default (`~/.config`, `~/.local/share`, `~/.local/state`, `~/.cache`)
//!
//! We always use XDG-style paths, even on macOS, to keep a developer tool's
//! config in predictable dotfile locations rather than `~/Library/...`.

use std::path::{Path, PathBuf};

use crate::path_context::DaemonHostPath;

pub const DAEMON_SOCKET_RELATIVE_PATH: &str = "run/flotilla.sock";

/// Return the only daemon socket owned by `config_dir`.
pub fn daemon_socket_path(config_dir: &Path) -> PathBuf {
    config_dir.join(DAEMON_SOCKET_RELATIVE_PATH)
}

/// Refuse a root/socket pairing that could expose one root through another root's socket.
pub fn ensure_daemon_socket_belongs_to_config(socket_path: &Path, config_dir: &Path) -> Result<(), String> {
    let expected = daemon_socket_path(config_dir);
    if socket_path == expected {
        return Ok(());
    }
    Err(format!(
        "daemon socket {} does not belong to config root {}; expected {} — refusing to mix daemon root and socket identities",
        socket_path.display(),
        config_dir.display(),
        expected.display()
    ))
}

/// Derive a root's config and state directories from its canonical scoped socket.
pub fn scoped_daemon_dirs(socket_path: &Path) -> Option<(PathBuf, PathBuf)> {
    let scoped_root = socket_path
        .file_name()
        .filter(|name| *name == "flotilla.sock")
        .and_then(|_| socket_path.parent())
        .filter(|run_dir| run_dir.file_name().is_some_and(|name| name == "run"))
        .and_then(Path::parent)
        .filter(|config_dir| config_dir.file_name().is_some_and(|name| name == "config"))
        .and_then(Path::parent);
    scoped_root.map(|root| (root.join("config"), root.join("state")))
}

/// Resolved daemon-side directory locations.
///
/// Internal to the daemon and its stores — not exposed to providers.
#[derive(Debug, Clone)]
pub struct PathPolicy {
    pub config_dir: DaemonHostPath,
    #[allow(dead_code)] // reserved for future use (durable app data)
    pub data_dir: DaemonHostPath,
    pub state_dir: DaemonHostPath,
    #[allow(dead_code)] // reserved for future use (disposable caches)
    pub cache_dir: DaemonHostPath,
}

impl PathPolicy {
    /// Resolve from the process environment.
    pub fn from_process_env() -> Self {
        Self::from_env(|key| std::env::var_os(key))
    }

    /// Resolve from an arbitrary env-var lookup function.
    /// Useful for testing and for constructing from container env vars.
    pub fn from_env(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        if let Some(root) = get("FLOTILLA_ROOT").map(PathBuf::from) {
            return Self {
                config_dir: DaemonHostPath::new(root.join("config")),
                data_dir: DaemonHostPath::new(root.join("data")),
                state_dir: DaemonHostPath::new(root.join("state")),
                cache_dir: DaemonHostPath::new(root.join("cache")),
            };
        }

        let home = get("HOME").map(PathBuf::from).or_else(dirs::home_dir).expect("cannot determine home directory");

        let config_dir = get("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join("flotilla")).unwrap_or_else(|| home.join(".config/flotilla"));

        let data_dir =
            get("XDG_DATA_HOME").map(|p| PathBuf::from(p).join("flotilla")).unwrap_or_else(|| home.join(".local/share/flotilla"));

        let state_dir =
            get("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("flotilla")).unwrap_or_else(|| home.join(".local/state/flotilla"));

        let cache_dir = get("XDG_CACHE_HOME").map(|p| PathBuf::from(p).join("flotilla")).unwrap_or_else(|| home.join(".cache/flotilla"));

        Self {
            config_dir: DaemonHostPath::new(config_dir),
            data_dir: DaemonHostPath::new(data_dir),
            state_dir: DaemonHostPath::new(state_dir),
            cache_dir: DaemonHostPath::new(cache_dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flotilla_root_override_sets_all_dirs() {
        let policy = PathPolicy::from_env(|key| match key {
            "FLOTILLA_ROOT" => Some("/test/root".into()),
            _ => None,
        });
        assert_eq!(policy.config_dir.as_path(), std::path::Path::new("/test/root/config"));
        assert_eq!(policy.data_dir.as_path(), std::path::Path::new("/test/root/data"));
        assert_eq!(policy.state_dir.as_path(), std::path::Path::new("/test/root/state"));
        assert_eq!(policy.cache_dir.as_path(), std::path::Path::new("/test/root/cache"));
    }

    #[test]
    fn xdg_vars_override_defaults() {
        let policy = PathPolicy::from_env(|key| match key {
            "HOME" => Some("/home/test".into()),
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            "XDG_DATA_HOME" => Some("/xdg/data".into()),
            "XDG_STATE_HOME" => Some("/xdg/state".into()),
            "XDG_CACHE_HOME" => Some("/xdg/cache".into()),
            _ => None,
        });
        assert_eq!(policy.config_dir.as_path(), std::path::Path::new("/xdg/config/flotilla"));
        assert_eq!(policy.data_dir.as_path(), std::path::Path::new("/xdg/data/flotilla"));
        assert_eq!(policy.state_dir.as_path(), std::path::Path::new("/xdg/state/flotilla"));
        assert_eq!(policy.cache_dir.as_path(), std::path::Path::new("/xdg/cache/flotilla"));
    }

    #[test]
    fn defaults_to_xdg_under_home() {
        let policy = PathPolicy::from_env(|key| match key {
            "HOME" => Some("/home/test".into()),
            _ => None,
        });
        assert_eq!(policy.config_dir.as_path(), std::path::Path::new("/home/test/.config/flotilla"));
        assert_eq!(policy.data_dir.as_path(), std::path::Path::new("/home/test/.local/share/flotilla"));
        assert_eq!(policy.state_dir.as_path(), std::path::Path::new("/home/test/.local/state/flotilla"));
        assert_eq!(policy.cache_dir.as_path(), std::path::Path::new("/home/test/.cache/flotilla"));
    }

    #[test]
    fn flotilla_root_takes_precedence_over_xdg() {
        let policy = PathPolicy::from_env(|key| match key {
            "FLOTILLA_ROOT" => Some("/root".into()),
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            _ => None,
        });
        assert_eq!(policy.config_dir.as_path(), std::path::Path::new("/root/config"));
    }

    #[test]
    fn scoped_root_rejects_the_default_fleet_socket() {
        let policy = PathPolicy::from_env(|key| match key {
            "FLOTILLA_ROOT" => Some("/work/live-session".into()),
            "HOME" => Some("/home/test".into()),
            _ => None,
        });
        let default_policy = PathPolicy::from_env(|key| match key {
            "HOME" => Some("/home/test".into()),
            _ => None,
        });
        let default_socket = daemon_socket_path(default_policy.config_dir.as_path());

        let error = ensure_daemon_socket_belongs_to_config(&default_socket, policy.config_dir.as_path())
            .expect_err("a root-scoped daemon must not bind the default fleet socket");

        assert!(error.contains("does not belong to config root"), "unexpected error: {error}");
        assert!(error.contains("/work/live-session/config/run/flotilla.sock"), "unexpected error: {error}");
    }

    #[test]
    fn scoped_socket_supplies_the_root_for_a_managed_terminal() {
        let (config_dir, state_dir) = scoped_daemon_dirs(Path::new("/work/live-session/config/run/flotilla.sock"))
            .expect("canonical scoped socket should supply its root");

        assert_eq!(config_dir, Path::new("/work/live-session/config"));
        assert_eq!(state_dir, Path::new("/work/live-session/state"));
    }

    #[test]
    fn default_xdg_socket_does_not_claim_to_encode_a_root() {
        assert!(scoped_daemon_dirs(Path::new("/home/test/.config/flotilla/run/flotilla.sock")).is_none());
    }
}
