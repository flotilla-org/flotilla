use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

/// Per-category provider preference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderPreference {
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChangeRequestConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IssueTrackerConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CloudAgentConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AiUtilityConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    pub claude: Option<ClaudeAiUtilityConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClaudeAiUtilityConfig {
    pub implementation: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WorkspaceManagerConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TerminalPoolConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

/// Global flotilla config from ~/.config/flotilla/config.toml
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FlotillaConfig {
    #[serde(default)]
    pub vcs: VcsConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub change_request: ChangeRequestConfig,
    #[serde(default)]
    pub issue_tracker: IssueTrackerConfig,
    #[serde(default)]
    pub cloud_agent: CloudAgentConfig,
    #[serde(default)]
    pub ai_utility: AiUtilityConfig,
    #[serde(default)]
    pub workspace_manager: WorkspaceManagerConfig,
    #[serde(default)]
    pub terminal_pool: TerminalPoolConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct VcsConfig {
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitConfig {
    #[serde(default = "default_checkout_strategy")]
    pub checkout_strategy: String,
    #[serde(default = "default_checkout_path")]
    pub checkout_path: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self { checkout_strategy: default_checkout_strategy(), checkout_path: default_checkout_path() }
    }
}

fn default_checkout_strategy() -> String {
    "auto".to_string()
}

pub fn default_checkout_path() -> String {
    "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}".to_string()
}

/// Raw key binding overrides from config.toml.
///
/// Keys are key combo strings (parsed by `crokey` in the TUI crate).
/// Values are action names (parsed by `Action::from_config_str`).
/// Empty maps mean "use defaults".
///
/// Text input modes (branch_input, issue_search) are excluded because they
/// capture all keys via `captures_raw_keys()`. Command palette and file picker
/// use `no_shared_fallback` to prevent shared bindings from intercepting typing,
/// so their navigation keys are configurable here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KeysConfig {
    #[serde(default)]
    pub shared: HashMap<String, String>,
    #[serde(default)]
    pub normal: HashMap<String, String>,
    #[serde(default)]
    pub help: HashMap<String, String>,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub action_menu: HashMap<String, String>,
    #[serde(default)]
    pub delete_confirm: HashMap<String, String>,
    #[serde(default)]
    pub close_confirm: HashMap<String, String>,
    #[serde(default)]
    pub command_palette: HashMap<String, String>,
    #[serde(default)]
    pub file_picker: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UiConfig {
    #[serde(default)]
    pub preview: PreviewConfig,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub keys: KeysConfig,
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

/// Resolved checkout configuration (strategy + path) after merging per-repo overrides with global.
pub struct ResolvedCheckoutConfig {
    pub strategy: String,
    pub path: String,
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

/// Per-repo git overrides. Fields are Option so we can distinguish
/// "not set" from "explicitly set to the default value."
#[derive(Debug, Default, Deserialize)]
pub struct RepoGitConfig {
    pub checkout_strategy: Option<String>,
    pub checkout_path: Option<String>,
}

/// Global SSH settings for remote host connections.
#[derive(Debug, Clone, Deserialize)]
pub struct SshConfig {
    #[serde(default = "default_true")]
    pub multiplex: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self { multiplex: true }
    }
}

fn default_true() -> bool {
    true
}

/// Remote host configuration for multi-host mode.
/// Loaded from `~/.config/flotilla/hosts.toml`.
#[derive(Debug, Default)]
pub struct HostsConfig {
    pub ssh: SshConfig,
    pub hosts: HashMap<String, RemoteHostConfig>,
}

/// Configuration for a single remote host.
#[derive(Debug, Deserialize)]
pub struct RemoteHostConfig {
    pub hostname: String,
    pub expected_host_name: String,
    pub user: Option<String>,
    pub daemon_socket: String,
    pub ssh_multiplex: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawHostsConfig {
    #[serde(default)]
    ssh: SshConfig,
    #[serde(default)]
    hosts: HashMap<String, RawRemoteHostConfig>,
}

#[derive(Debug, Deserialize)]
struct RawRemoteHostConfig {
    hostname: String,
    expected_host_name: Option<String>,
    user: Option<String>,
    daemon_socket: String,
    ssh_multiplex: Option<bool>,
}

impl<'de> Deserialize<'de> for HostsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawHostsConfig::deserialize(deserializer)?;
        let ssh = raw.ssh;
        let hosts = raw
            .hosts
            .into_iter()
            .map(|(label, host)| {
                let expected_host_name = host.expected_host_name.unwrap_or_else(|| label.clone());
                (label, RemoteHostConfig {
                    hostname: host.hostname,
                    expected_host_name,
                    user: host.user,
                    daemon_socket: host.daemon_socket,
                    ssh_multiplex: host.ssh_multiplex,
                })
            })
            .collect();
        Ok(Self { ssh, hosts })
    }
}

impl HostsConfig {
    /// Resolve SSH multiplex setting for a host label.
    /// Per-host `ssh_multiplex` overrides global `ssh.multiplex`.
    pub fn resolved_ssh_multiplex(&self, host_label: &str) -> bool {
        self.hosts.get(host_label).and_then(|h| h.ssh_multiplex).unwrap_or(self.ssh.multiplex)
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

    /// Resolve checkout config for a repo: per-repo override > global > defaults.
    pub fn resolve_checkout_config(&self, repo_root: &Path) -> ResolvedCheckoutConfig {
        let global = self.load_config();
        let slug = path_to_slug(repo_root);
        let repo_file = self.repos_dir().join(format!("{slug}.toml"));
        if let Ok(content) = std::fs::read_to_string(&repo_file) {
            match toml::from_str::<RepoFileConfig>(&content) {
                Ok(repo_cfg) => {
                    return ResolvedCheckoutConfig {
                        strategy: repo_cfg.vcs.git.checkout_strategy.unwrap_or_else(|| global.vcs.git.checkout_strategy.clone()),
                        path: repo_cfg.vcs.git.checkout_path.unwrap_or_else(|| global.vcs.git.checkout_path.clone()),
                    };
                }
                Err(e) => {
                    tracing::warn!(path = %repo_file.display(), err = %e, "failed to parse");
                }
            }
        }
        ResolvedCheckoutConfig { strategy: global.vcs.git.checkout_strategy.clone(), path: global.vcs.git.checkout_path.clone() }
    }
}

/// Collect repo roots: persisted (in saved tab order) first, then CLI args, then auto-detect from cwd.
/// Persists any new repos and saves tab order.
pub async fn resolve_repo_roots(cli_roots: &[PathBuf], config: &ConfigStore) -> Vec<PathBuf> {
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
        if let Some(repo_root) = git.resolve_repo_root(cwd).await {
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
mod tests;
