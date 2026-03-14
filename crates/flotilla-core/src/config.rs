use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

/// Global flotilla config from ~/.config/flotilla/config.toml
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FlotillaConfig {
    #[serde(default)]
    pub vcs: VcsConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VcsConfig {
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GitConfig {
    #[serde(default)]
    pub checkouts: CheckoutsConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckoutsConfig {
    #[serde(default = "CheckoutsConfig::default_path")]
    pub path: String,
    #[serde(default = "CheckoutsConfig::default_provider")]
    pub provider: String,
}

impl Default for CheckoutsConfig {
    fn default() -> Self {
        Self { path: Self::default_path(), provider: Self::default_provider() }
    }
}

impl CheckoutsConfig {
    fn default_path() -> String {
        "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}".to_string()
    }
    fn default_provider() -> String {
        "auto".to_string()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UiConfig {
    #[serde(default)]
    pub preview: PreviewConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PreviewConfig {
    #[serde(default)]
    pub layout: RepoViewLayoutConfig,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepoViewLayoutConfig {
    #[default]
    Auto,
    Zoom,
    Right,
    Below,
}

/// Full repo config file including optional overrides.
#[derive(Debug, Default, Deserialize)]
pub struct RepoFileConfig {
    #[allow(dead_code)] // Required field so TOML parsing accepts existing repo files
    pub path: String,
    #[serde(default)]
    pub vcs: RepoVcsConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoVcsConfig {
    #[serde(default)]
    pub git: RepoGitConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoGitConfig {
    #[serde(default)]
    pub checkouts: RepoCheckoutsOverride,
}

/// Per-repo checkout overrides. Fields are Option so we can distinguish
/// "not set" from "explicitly set to the default value."
#[derive(Debug, Default, Deserialize)]
pub struct RepoCheckoutsOverride {
    pub path: Option<String>,
    pub provider: Option<String>,
}

/// Remote host configuration for multi-host mode.
/// Loaded from `~/.config/flotilla/hosts.toml`.
#[derive(Debug, Default)]
pub struct HostsConfig {
    pub hosts: HashMap<String, RemoteHostConfig>,
}

/// Configuration for a single remote host.
#[derive(Debug, Deserialize)]
pub struct RemoteHostConfig {
    pub hostname: String,
    pub expected_host_name: String,
    pub user: Option<String>,
    pub daemon_socket: String,
}

#[derive(Debug, Deserialize)]
struct RawHostsConfig {
    #[serde(default)]
    hosts: HashMap<String, RawRemoteHostConfig>,
}

#[derive(Debug, Deserialize)]
struct RawRemoteHostConfig {
    hostname: String,
    expected_host_name: Option<String>,
    user: Option<String>,
    daemon_socket: String,
}

impl<'de> Deserialize<'de> for HostsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHostsConfig::deserialize(deserializer)?;
        let hosts = raw
            .hosts
            .into_iter()
            .map(|(label, host)| {
                let expected_host_name = host.expected_host_name.unwrap_or_else(|| label.clone());
                (
                    label,
                    RemoteHostConfig { hostname: host.hostname, expected_host_name, user: host.user, daemon_socket: host.daemon_socket },
                )
            })
            .collect();
        Ok(Self { hosts })
    }
}

/// Daemon-level configuration.
/// Loaded from `~/.config/flotilla/daemon.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub follower: bool,
    pub host_name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RepoConfig {
    path: String,
}

/// Default flotilla config directory (used for socket path defaults etc.)
pub fn flotilla_config_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")).join(".config/flotilla")
}

/// Convert "/Users/robert/dev/scratch" → "users-robert-dev-scratch"
pub fn path_to_slug(path: &Path) -> String {
    let raw = path.to_string_lossy().to_lowercase();
    let mut prev_hyphen = false;
    let slug: String = raw
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                prev_hyphen = false;
                Some(c)
            } else if !prev_hyphen {
                prev_hyphen = true;
                Some('-')
            } else {
                None
            }
        })
        .collect();
    slug.trim_matches('-').to_string()
}

/// Owns the config base path and caches the global `FlotillaConfig`.
pub struct ConfigStore {
    base: PathBuf,
    global_config: OnceLock<Mutex<FlotillaConfig>>,
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore {
    /// Production constructor — uses ~/.config/flotilla/
    pub fn new() -> Self {
        Self { base: dirs::home_dir().unwrap_or_else(|| PathBuf::from("~")).join(".config/flotilla"), global_config: OnceLock::new() }
    }

    /// Test constructor — uses provided base path
    pub fn with_base(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into(), global_config: OnceLock::new() }
    }

    /// The base config directory path.
    pub fn base_path(&self) -> &Path {
        &self.base
    }

    fn repos_dir(&self) -> PathBuf {
        self.base.join("repos")
    }

    fn tab_order_file(&self) -> PathBuf {
        self.base.join("tab-order.json")
    }

    /// Load all persisted repo paths from config dir, sorted alphabetically by slug.
    pub fn load_repos(&self) -> Vec<PathBuf> {
        let dir = self.repos_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut repos: Vec<(String, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
            .filter_map(|e| {
                let content = std::fs::read_to_string(e.path()).ok()?;
                let config: RepoConfig = toml::from_str(&content).ok()?;
                let path = PathBuf::from(&config.path);
                if path.is_dir() {
                    Some((e.file_name().to_string_lossy().to_string(), path))
                } else {
                    None
                }
            })
            .collect();
        repos.sort_by(|a, b| a.0.cmp(&b.0));
        repos.into_iter().map(|(_, path)| path).collect()
    }

    /// Persist a repo path to config. No-op if already persisted.
    pub fn save_repo(&self, path: &Path) {
        let dir = self.repos_dir();
        let _ = std::fs::create_dir_all(&dir);
        let slug = path_to_slug(path);
        let file = dir.join(format!("{slug}.toml"));
        if file.exists() {
            return;
        }
        let config = RepoConfig { path: path.to_string_lossy().to_string() };
        if let Ok(content) = toml::to_string(&config) {
            let _ = std::fs::write(file, content);
        }
    }

    /// Remove a repo's config file.
    pub fn remove_repo(&self, path: &Path) {
        let dir = self.repos_dir();
        let slug = path_to_slug(path);
        let file = dir.join(format!("{slug}.toml"));
        let _ = std::fs::remove_file(file);
    }

    /// Load persisted tab order. Returns None if file doesn't exist or is invalid.
    pub fn load_tab_order(&self) -> Option<Vec<PathBuf>> {
        let content = std::fs::read_to_string(self.tab_order_file()).ok()?;
        let paths: Vec<String> = serde_json::from_str(&content).ok()?;
        Some(paths.into_iter().map(PathBuf::from).collect())
    }

    /// Save tab order to disk.
    pub fn save_tab_order(&self, order: &[PathBuf]) {
        let _ = std::fs::create_dir_all(&self.base);
        let paths: Vec<&str> = order.iter().filter_map(|p| p.to_str()).collect();
        if let Ok(content) = serde_json::to_string_pretty(&paths) {
            let _ = std::fs::write(self.tab_order_file(), content);
        }
    }

    /// Load global flotilla config (cached for the lifetime of the store).
    pub fn load_config(&self) -> FlotillaConfig {
        self.global_config
            .get_or_init(|| {
                Mutex::new({
                    let path = self.base.join("config.toml");
                    std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|content| {
                            toml::from_str(&content).map_err(|e| tracing::warn!(path = %path.display(), err = %e, "failed to parse")).ok()
                        })
                        .unwrap_or_default()
                })
            })
            .lock()
            .expect("config cache mutex poisoned")
            .clone()
    }

    pub fn save_layout(&self, layout: RepoViewLayoutConfig) {
        let path = self.base.join("config.toml");
        let mut config = self.load_config();
        config.ui.preview.layout = layout;

        if let Err(err) = std::fs::create_dir_all(&self.base) {
            tracing::warn!(path = %self.base.display(), err = %err, "failed to create config dir");
            return;
        }

        let content = match toml::to_string_pretty(&config) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(path = %path.display(), err = %err, "failed to serialize config");
                return;
            }
        };

        if let Err(err) = std::fs::write(&path, content) {
            tracing::warn!(path = %path.display(), err = %err, "failed to write config");
            return;
        }

        if let Some(cached) = self.global_config.get() {
            *cached.lock().expect("config cache mutex poisoned") = config;
        }
    }

    /// Load remote hosts config from `~/.config/flotilla/hosts.toml`.
    pub fn load_hosts(&self) -> Result<HostsConfig, String> {
        let path = self.base_path().join("hosts.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            toml::from_str(&content).map_err(|err| format!("failed to parse {}: {err}", path.display()))
        } else {
            Ok(HostsConfig::default())
        }
    }

    /// Load daemon config from `~/.config/flotilla/daemon.toml`.
    pub fn load_daemon_config(&self) -> DaemonConfig {
        let path = self.base_path().join("daemon.toml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            toml::from_str(&content).unwrap_or_default()
        } else {
            DaemonConfig::default()
        }
    }

    /// Resolve checkouts config for a repo: per-repo override > global > defaults.
    pub fn resolve_checkouts_config(&self, repo_root: &Path) -> CheckoutsConfig {
        let global = self.load_config();
        let slug = path_to_slug(repo_root);
        let repo_file = self.repos_dir().join(format!("{slug}.toml"));
        if let Ok(content) = std::fs::read_to_string(&repo_file) {
            match toml::from_str::<RepoFileConfig>(&content) {
                Ok(repo_cfg) => {
                    let repo_co = &repo_cfg.vcs.git.checkouts;
                    return CheckoutsConfig {
                        path: repo_co.path.clone().unwrap_or_else(|| global.vcs.git.checkouts.path.clone()),
                        provider: repo_co.provider.clone().unwrap_or_else(|| global.vcs.git.checkouts.provider.clone()),
                    };
                }
                Err(e) => {
                    tracing::warn!(path = %repo_file.display(), err = %e, "failed to parse");
                }
            }
        }
        global.vcs.git.checkouts.clone()
    }
}

/// Collect repo roots: persisted (in saved tab order) first, then CLI args, then auto-detect from cwd.
/// Persists any new repos and saves tab order.
pub fn resolve_repo_roots(cli_roots: &[PathBuf], config: &ConfigStore) -> Vec<PathBuf> {
    use crate::providers::{
        vcs::{git::GitVcs, Vcs},
        ProcessCommandRunner,
    };

    let mut repo_roots: Vec<PathBuf> = Vec::new();

    // 1. Persisted repos in saved tab order
    let persisted = config.load_repos();
    let tab_order = config.load_tab_order();
    if let Some(order) = tab_order {
        for path in &order {
            if persisted.contains(path) && !repo_roots.contains(path) {
                repo_roots.push(path.clone());
            }
        }
        // Any persisted repos not in the order file go at the end
        for path in &persisted {
            if !repo_roots.contains(path) {
                repo_roots.push(path.clone());
            }
        }
    } else {
        repo_roots.extend(persisted);
    }

    // 2. CLI args (appended after persisted)
    for root in cli_roots {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if !repo_roots.contains(&canonical) {
            repo_roots.push(canonical);
        }
    }

    // 3. Auto-detect from cwd — resolve to main repo root (not worktree)
    let cwd = std::env::current_dir().ok();
    if let Some(ref cwd) = cwd {
        let git = GitVcs::new(Arc::new(ProcessCommandRunner));
        if let Some(repo_root) = git.resolve_repo_root(cwd) {
            if !repo_roots.contains(&repo_root) {
                repo_roots.push(repo_root);
            }
        }
    }

    // Persist any new repos and save tab order
    for path in &repo_roots {
        config.save_repo(path);
    }
    config.save_tab_order(&repo_roots);

    repo_roots
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn make_dir(base: &Path, name: &str) -> PathBuf {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_repo_file(base: &Path, filename: &str, content: &str) {
        let repos_dir = base.join("repos");
        std::fs::create_dir_all(&repos_dir).unwrap();
        std::fs::write(repos_dir.join(filename), content).unwrap();
    }

    #[test]
    fn path_to_slug_covers_core_shapes() {
        let cases = [
            ("/Users/alice/dev/myrepo", "users-alice-dev-myrepo"),
            ("relative/path", "relative-path"),
            ("/Users/Bob Smith/my repo", "users-bob-smith-my-repo"),
            ("/opt/my-project_v2.0", "opt-my-project_v2.0"),
            ("/tmp/my__project", "tmp-my__project"),
            ("/", ""),
            (".", "."),
        ];
        for (input, expected) in cases {
            assert_eq!(path_to_slug(Path::new(input)), expected, "unexpected slug for input: {input}");
        }
    }

    #[test]
    fn save_repo_roundtrip_is_idempotent_and_removable() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo = make_dir(base, "repo");

        let store = ConfigStore::with_base(base);
        store.save_repo(&repo);
        store.save_repo(&repo);
        assert_eq!(store.load_repos(), vec![repo.clone()]);

        store.remove_repo(&repo);
        assert!(store.load_repos().is_empty());
    }

    #[test]
    fn save_repo_creates_repos_dir_if_missing() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("deep/nested/config");
        let repo = make_dir(dir.path(), "myrepo");

        let store = ConfigStore::with_base(&base);
        store.save_repo(&repo);

        assert!(base.join("repos").exists());
        assert_eq!(store.load_repos(), vec![repo]);
    }

    #[test]
    fn load_repos_sorts_and_skips_invalid_entries() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo_a = make_dir(base, "alpha");
        let repo_b = make_dir(base, "bravo");

        let store = ConfigStore::with_base(base);
        store.save_repo(&repo_b);
        store.save_repo(&repo_a);

        std::fs::write(base.join("repos").join("notes.txt"), "ignore me").unwrap();
        write_repo_file(base, "broken.toml", "not valid toml");
        write_repo_file(base, "missing-path.toml", "[section]\nkey = \"value\"\n");
        write_repo_file(base, "ghost.toml", "path = \"/nonexistent/ghost\"\n");

        assert_eq!(store.load_repos(), vec![repo_a, repo_b]);
    }

    #[test]
    fn load_repos_returns_empty_when_dir_missing() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());
        assert!(store.load_repos().is_empty());
    }

    #[test]
    fn tab_order_roundtrip_and_parse_failures() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let store = ConfigStore::with_base(base);

        assert!(store.load_tab_order().is_none());

        let order = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        store.save_tab_order(&order);
        assert_eq!(store.load_tab_order(), Some(order));

        std::fs::write(base.join("tab-order.json"), "not json {{{").unwrap();
        assert!(store.load_tab_order().is_none());

        std::fs::write(base.join("tab-order.json"), r#"{"k":"v"}"#).unwrap();
        assert!(store.load_tab_order().is_none());

        std::fs::write(base.join("tab-order.json"), "[]").unwrap();
        assert_eq!(store.load_tab_order(), Some(Vec::new()));
    }

    #[test]
    fn save_tab_order_creates_base_dir() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("new/config/dir");
        let store = ConfigStore::with_base(&base);

        store.save_tab_order(&[PathBuf::from("/a")]);
        assert!(base.join("tab-order.json").exists());
    }

    #[test]
    fn load_config_missing_or_invalid_returns_defaults() {
        let root = tempdir().unwrap();

        let missing_store = ConfigStore::with_base(root.path().join("missing"));
        assert_eq!(missing_store.load_config().vcs.git.checkouts.provider, "auto");

        let invalid_base = root.path().join("invalid");
        std::fs::create_dir_all(&invalid_base).unwrap();
        std::fs::write(invalid_base.join("config.toml"), "this is not valid {{toml").unwrap();
        let invalid_store = ConfigStore::with_base(&invalid_base);
        assert_eq!(invalid_store.load_config().vcs.git.checkouts.provider, "auto");
    }

    #[test]
    fn load_config_parses_full_overrides() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[vcs.git.checkouts]\npath = \"/custom/{{ branch }}\"\nprovider = \"worktree\"\n")
            .unwrap();
        let store = ConfigStore::with_base(dir.path());
        let cfg = store.load_config();
        assert_eq!(cfg.vcs.git.checkouts.path, "/custom/{{ branch }}");
        assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
    }

    #[test]
    fn load_config_partial_override_keeps_defaults() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[vcs.git.checkouts]\nprovider = \"worktree\"\n").unwrap();
        let store = ConfigStore::with_base(dir.path());
        let cfg = store.load_config();
        assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
        assert_eq!(cfg.vcs.git.checkouts.path, CheckoutsConfig::default_path());
    }

    #[test]
    fn load_config_parses_layout() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[ui.preview]\nlayout = \"zoom\"\n").unwrap();

        let store = ConfigStore::with_base(dir.path());
        let cfg = store.load_config();
        assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Zoom);
    }

    #[test]
    fn save_layout_writes_global_config() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[vcs.git.checkouts]\nprovider = \"worktree\"\n").unwrap();

        let store = ConfigStore::with_base(dir.path());
        store.save_layout(RepoViewLayoutConfig::Right);

        let reloaded = ConfigStore::with_base(dir.path());
        let cfg = reloaded.load_config();
        assert_eq!(cfg.vcs.git.checkouts.provider, "worktree");
        assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Right);
    }

    #[test]
    fn save_layout_updates_same_store_cache() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());

        assert_eq!(store.load_config().ui.preview.layout, RepoViewLayoutConfig::Auto);

        store.save_layout(RepoViewLayoutConfig::Below);

        let cfg = store.load_config();
        assert_eq!(cfg.ui.preview.layout, RepoViewLayoutConfig::Below);
    }

    #[test]
    fn load_config_is_cached() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("config.toml"), "[vcs.git.checkouts]\nprovider = \"first\"\n").unwrap();

        let store = ConfigStore::with_base(base);
        assert_eq!(store.load_config().vcs.git.checkouts.provider, "first");

        std::fs::write(base.join("config.toml"), "[vcs.git.checkouts]\nprovider = \"second\"\n").unwrap();
        assert_eq!(store.load_config().vcs.git.checkouts.provider, "first");
    }

    #[test]
    fn resolve_checkouts_config_uses_global_when_repo_file_missing_or_invalid() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("config.toml"), "[vcs.git.checkouts]\npath = \"/global/path\"\nprovider = \"global-prov\"\n").unwrap();

        let repo = make_dir(base, "repo");
        let store = ConfigStore::with_base(base);

        let from_global = store.resolve_checkouts_config(&repo);
        assert_eq!(from_global.path, "/global/path");
        assert_eq!(from_global.provider, "global-prov");

        let slug = path_to_slug(&repo);
        write_repo_file(base, &format!("{slug}.toml"), "{{invalid toml!!!");
        let from_invalid = store.resolve_checkouts_config(&repo);
        assert_eq!(from_invalid.path, "/global/path");
        assert_eq!(from_invalid.provider, "global-prov");
    }

    #[test]
    fn resolve_checkouts_config_repo_override_merges_with_global() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("config.toml"), "[vcs.git.checkouts]\npath = \"/global/path\"\nprovider = \"global-prov\"\n").unwrap();

        let repo = make_dir(base, "repo");
        let store = ConfigStore::with_base(base);
        let slug = path_to_slug(&repo);

        let cases = [
            ("[vcs.git.checkouts]\npath = \"/repo/path\"\nprovider = \"repo-prov\"\n", "/repo/path", "repo-prov"),
            ("[vcs.git.checkouts]\npath = \"/repo/path-only\"\n", "/repo/path-only", "global-prov"),
            ("[vcs.git.checkouts]\nprovider = \"repo-only\"\n", "/global/path", "repo-only"),
        ];

        for (override_toml, expected_path, expected_provider) in cases {
            let repo_toml = format!("path = \"{}\"\n{override_toml}", repo.display());
            write_repo_file(base, &format!("{slug}.toml"), &repo_toml);

            let resolved = store.resolve_checkouts_config(&repo);
            assert_eq!(resolved.path, expected_path);
            assert_eq!(resolved.provider, expected_provider);
        }
    }

    #[test]
    fn defaults_have_expected_values_and_base_path_roundtrips() {
        let checkouts = CheckoutsConfig::default();
        assert_eq!(checkouts.path, "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}");
        assert_eq!(checkouts.provider, "auto");

        let repo_override = RepoCheckoutsOverride::default();
        assert!(repo_override.path.is_none());
        assert!(repo_override.provider.is_none());

        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());
        assert_eq!(store.base_path(), dir.path());
    }

    #[test]
    fn parse_hosts_config() {
        let toml = r#"
[hosts.desktop]
hostname = "desktop.local"
expected_host_name = "desktop"
user = "robert"
daemon_socket = "/run/user/1000/flotilla/daemon.sock"

[hosts.cloud]
hostname = "10.0.1.50"
expected_host_name = "cloud"
daemon_socket = "/home/robert/.config/flotilla/daemon.sock"
"#;
        let config: HostsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
        assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
        assert_eq!(config.hosts["desktop"].user, Some("robert".into()));
        assert_eq!(config.hosts["cloud"].expected_host_name, "cloud");
        assert_eq!(config.hosts["cloud"].user, None);
    }

    #[test]
    fn parse_hosts_config_defaults_expected_host_name_to_table_key() {
        let toml = r#"
[hosts.desktop]
hostname = "desktop.local"
daemon_socket = "/run/user/1000/flotilla/daemon.sock"
"#;
        let config: HostsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
        assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    }

    #[test]
    fn parse_daemon_config_follower() {
        let toml = r#"
follower = true
host_name = "my-desktop"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!(config.follower);
        assert_eq!(config.host_name, Some("my-desktop".into()));
    }

    #[test]
    fn parse_daemon_config_defaults() {
        let config: DaemonConfig = toml::from_str("").unwrap();
        assert!(!config.follower);
        assert_eq!(config.host_name, None);
    }

    #[test]
    fn load_hosts_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());
        let config = store.load_hosts().unwrap();
        assert!(config.hosts.is_empty());
    }

    #[test]
    fn load_hosts_from_file() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/d.sock\"\n",
        )
        .unwrap();
        let store = ConfigStore::with_base(base);
        let config = store.load_hosts().unwrap();
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts["desktop"].hostname, "desktop.local");
        assert_eq!(config.hosts["desktop"].expected_host_name, "desktop");
    }

    #[test]
    fn load_hosts_invalid_file_returns_error() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(
            base.join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = [\ndaemon_socket = \"/tmp/d.sock\"\n",
        )
        .unwrap();
        let store = ConfigStore::with_base(base);
        let err = store.load_hosts().expect_err("invalid hosts config should error");
        assert!(err.contains("failed to parse"));
    }

    #[test]
    fn load_daemon_config_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let store = ConfigStore::with_base(dir.path());
        let config = store.load_daemon_config();
        assert!(!config.follower);
        assert_eq!(config.host_name, None);
    }

    #[test]
    fn load_daemon_config_from_file() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("daemon.toml"), "follower = true\nhost_name = \"my-host\"\n").unwrap();
        let store = ConfigStore::with_base(base);
        let config = store.load_daemon_config();
        assert!(config.follower);
        assert_eq!(config.host_name, Some("my-host".into()));
    }
}
