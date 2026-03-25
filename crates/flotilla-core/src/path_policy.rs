//! Path policy: resolves daemon-side directory locations from environment variables.
//!
//! Resolution order per category:
//! 1. `FLOTILLA_ROOT` → `<root>/<category>`
//! 2. Category-specific XDG env var (`XDG_CONFIG_HOME`, etc.)
//! 3. `dirs::` fallback (platform default)
//! 4. Hardcoded fallback only if all else fails

use std::path::PathBuf;

use crate::path_context::DaemonHostPath;

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

        let config_dir = get("XDG_CONFIG_HOME")
            .map(|p| PathBuf::from(p).join("flotilla"))
            .or_else(|| dirs::config_dir().map(|p| p.join("flotilla")))
            // Relative fallback — unreachable on supported platforms (dirs:: always succeeds on macOS/Linux)
            .unwrap_or_else(|| PathBuf::from(".config/flotilla"));

        let data_dir = get("XDG_DATA_HOME")
            .map(|p| PathBuf::from(p).join("flotilla"))
            .or_else(|| dirs::data_dir().map(|p| p.join("flotilla")))
            .unwrap_or_else(|| PathBuf::from(".local/share/flotilla")); // unreachable on supported platforms

        let state_dir = get("XDG_STATE_HOME")
            .map(|p| PathBuf::from(p).join("flotilla"))
            .or_else(|| dirs::state_dir().map(|p| p.join("flotilla")))
            .unwrap_or_else(|| PathBuf::from(".local/state/flotilla")); // unreachable on supported platforms

        let cache_dir = get("XDG_CACHE_HOME")
            .map(|p| PathBuf::from(p).join("flotilla"))
            .or_else(|| dirs::cache_dir().map(|p| p.join("flotilla")))
            .unwrap_or_else(|| PathBuf::from(".cache/flotilla")); // unreachable on supported platforms

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
    fn flotilla_root_takes_precedence_over_xdg() {
        let policy = PathPolicy::from_env(|key| match key {
            "FLOTILLA_ROOT" => Some("/root".into()),
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            _ => None,
        });
        assert_eq!(policy.config_dir.as_path(), std::path::Path::new("/root/config"));
    }
}
