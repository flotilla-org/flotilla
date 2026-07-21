use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use flotilla_protocol::NodeId;
use serde::{Deserialize, Serialize};

use crate::path_context::{DaemonHostPath, ExecutionEnvironmentPath};

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
pub struct PresentationManagerConfig {
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
    pub presentation_manager: PresentationManagerConfig,
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
    pub tab_page: HashMap<String, String>,
    #[serde(default)]
    pub tab_shell: HashMap<String, String>,
    #[serde(default)]
    pub help: HashMap<String, String>,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub convoys: HashMap<String, String>,
    #[serde(default)]
    pub project: HashMap<String, String>,
    #[serde(default)]
    pub convoy_vessels: HashMap<String, String>,
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
    #[serde(default)]
    pub expected_node_id: Option<NodeId>,
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
    #[serde(default)]
    expected_node_id: Option<NodeId>,
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
                    expected_node_id: host.expected_node_id,
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
/// `daemon.toml` is the source of truth for execution environments.
/// Peer-daemon mesh config stays in `hosts.toml`.
/// Loaded from `~/.config/flotilla/daemon.toml`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub follower: bool,
    #[serde(default)]
    pub machine_id: Option<String>,
    pub host_name: Option<String>,
    #[serde(default)]
    pub environments: BTreeMap<String, StaticEnvironmentConfig>,
}

/// Static SSH-backed direct execution environment configured in `daemon.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StaticEnvironmentConfig {
    pub hostname: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub flotilla_command: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RepoConfig {
    path: String,
}

/// One persisted open View (ADR 0013). The address stays a raw string here:
/// an entry with an unknown or malformed address must degrade to that one
/// view rendering an error state, never invalidate the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenViewEntry {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OpenViewsFile {
    #[serde(default)]
    views: Vec<OpenViewEntry>,
}

fn repo_file_key(path: &Path) -> String {
    let key = urlencoding::encode(&path.to_string_lossy()).into_owned();
    if key.len() > 200 {
        tracing::warn!(key_len = key.len(), path = %path.display(), "repo config filename key is close to filesystem limit");
    }
    key
}

fn migrate_repo_file(source: &Path, canonical: &Path) {
    if source == canonical {
        return;
    }
    let canonical_exists = canonical.exists();
    let result = if canonical_exists { std::fs::remove_file(source) } else { std::fs::rename(source, canonical) };
    if let Err(err) = result {
        tracing::warn!(from = %source.display(), to = %canonical.display(), %err, "failed to migrate repo config filename");
    } else if canonical_exists {
        tracing::info!(legacy = %source.display(), canonical = %canonical.display(), "removed duplicate legacy repo config");
    }
}

/// Owns daemon-side paths and caches the global `FlotillaConfig`.
///
/// NOTE: This struct is accumulating path responsibilities beyond pure config.
/// A future refactor should split config, state, and data storage properly.
pub struct ConfigStore {
    base: DaemonHostPath,
    state_dir: DaemonHostPath,
    global_config: OnceLock<Mutex<FlotillaConfig>>,
}

impl ConfigStore {
    /// Create a ConfigStore with explicit config and state directories.
    /// Production callers should pass paths from `PathPolicy`.
    pub fn new(base: DaemonHostPath, state_dir: DaemonHostPath) -> Self {
        Self { base, state_dir, global_config: OnceLock::new() }
    }

    /// Test constructor — uses provided base path for both config and state.
    pub fn with_base(base: impl Into<PathBuf>) -> Self {
        let p = base.into();
        Self::new(DaemonHostPath::new(p.clone()), DaemonHostPath::new(p))
    }

    /// The runtime state directory (workspace state, shpool sockets, etc.).
    pub fn state_dir(&self) -> &DaemonHostPath {
        &self.state_dir
    }

    /// The base config directory path.
    pub fn base_path(&self) -> &DaemonHostPath {
        &self.base
    }

    fn repos_dir(&self) -> DaemonHostPath {
        self.base.join("repos")
    }

    /// Load all persisted repo paths, migrating noncanonical filenames and sorting by path.
    pub fn load_and_migrate_repos(&self) -> Vec<ExecutionEnvironmentPath> {
        let dir = self.repos_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let files: Vec<PathBuf> =
            entries.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|ext| ext == "toml")).map(|e| e.path()).collect();
        let mut repos: Vec<(String, ExecutionEnvironmentPath)> = files
            .into_iter()
            .filter_map(|source| {
                let content = std::fs::read_to_string(&source).ok()?;
                let config: RepoConfig = toml::from_str(&content).ok()?;
                let path = PathBuf::from(&config.path);
                let canonical = dir.join(format!("{}.toml", repo_file_key(&path)));
                migrate_repo_file(&source, canonical.as_path());
                if path.is_dir() {
                    Some((config.path, ExecutionEnvironmentPath::new(path)))
                } else {
                    None
                }
            })
            .collect();
        repos.sort_by(|a, b| a.0.cmp(&b.0));
        repos.dedup_by(|a, b| a.0 == b.0);
        repos.into_iter().map(|(_, path)| path).collect()
    }

    /// Persist a repo path to config. No-op if already persisted.
    pub fn save_repo(&self, path: &ExecutionEnvironmentPath) {
        let dir = self.repos_dir();
        let _ = std::fs::create_dir_all(&dir);
        let key = repo_file_key(path.as_path());
        let file = dir.join(format!("{key}.toml"));
        if file.as_path().exists() {
            return;
        }
        let config = RepoConfig { path: path.as_path().to_string_lossy().to_string() };
        if let Ok(content) = toml::to_string(&config) {
            let _ = std::fs::write(file.as_path(), content);
        }
    }

    /// Remove a repo's config file.
    pub fn remove_repo(&self, path: &ExecutionEnvironmentPath) {
        let dir = self.repos_dir();
        let key = repo_file_key(path.as_path());
        let file = dir.join(format!("{key}.toml"));
        let _ = std::fs::remove_file(file.as_path());
    }

    fn open_views_file(&self) -> DaemonHostPath {
        self.base.join("open-views.toml")
    }

    /// Load the persisted open-view set. Returns None if the file doesn't
    /// exist or is invalid — the caller seeds a default set (ADR 0013).
    pub fn load_open_views(&self) -> Option<Vec<OpenViewEntry>> {
        let content = std::fs::read_to_string(self.open_views_file().as_path()).ok()?;
        let file: OpenViewsFile = toml::from_str(&content).map_err(|e| tracing::warn!(err = %e, "failed to parse open-views.toml")).ok()?;
        Some(file.views)
    }

    /// Save the open-view set (ordered; index 0 is the pinned overview).
    pub fn save_open_views(&self, views: &[OpenViewEntry]) {
        let _ = std::fs::create_dir_all(self.base.as_path());
        let file = OpenViewsFile { views: views.to_vec() };
        if let Ok(content) = toml::to_string(&file) {
            let _ = std::fs::write(self.open_views_file().as_path(), content);
        }
    }

    /// Load global flotilla config (cached for the lifetime of the store).
    pub fn load_config(&self) -> FlotillaConfig {
        self.global_config
            .get_or_init(|| {
                Mutex::new({
                    let path = self.base.join("config.toml");
                    std::fs::read_to_string(path.as_path())
                        .ok()
                        .and_then(|content| toml::from_str(&content).map_err(|e| tracing::warn!(%path, err = %e, "failed to parse")).ok())
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

        if let Err(err) = std::fs::create_dir_all(self.base.as_path()) {
            tracing::warn!(base = %self.base, err = %err, "failed to create config dir");
            return;
        }

        let content = match toml::to_string_pretty(&config) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(%path, err = %err, "failed to serialize config");
                return;
            }
        };

        if let Err(err) = std::fs::write(path.as_path(), content) {
            tracing::warn!(%path, err = %err, "failed to write config");
            return;
        }

        if let Some(cached) = self.global_config.get() {
            *cached.lock().expect("config cache mutex poisoned") = config;
        }
    }

    /// Load remote hosts config from `~/.config/flotilla/hosts.toml`.
    pub fn load_hosts(&self) -> Result<HostsConfig, String> {
        let path = self.base_path().join("hosts.toml");
        if path.as_path().exists() {
            let content = std::fs::read_to_string(path.as_path()).map_err(|err| format!("failed to read {path}: {err}"))?;
            toml::from_str(&content).map_err(|err| format!("failed to parse {path}: {err}"))
        } else {
            Ok(HostsConfig::default())
        }
    }

    /// Load daemon config from `~/.config/flotilla/daemon.toml`.
    pub fn load_daemon_config(&self) -> Result<DaemonConfig, String> {
        let path = self.base_path().join("daemon.toml");
        if path.as_path().exists() {
            let content = std::fs::read_to_string(path.as_path()).map_err(|err| format!("failed to read {path}: {err}"))?;
            toml::from_str(&content).map_err(|err| format!("failed to parse {path}: {err}"))
        } else {
            Ok(DaemonConfig::default())
        }
    }

    /// Resolve checkout config for a repo: per-repo override > global > defaults.
    pub fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        let global = self.load_config();
        let key = repo_file_key(repo_root.as_path());
        let repo_file = self.repos_dir().join(format!("{key}.toml"));
        if let Ok(content) = std::fs::read_to_string(repo_file.as_path()) {
            match toml::from_str::<RepoFileConfig>(&content) {
                Ok(repo_cfg) => {
                    if repo_cfg.path != repo_root.as_path().to_string_lossy() {
                        tracing::warn!(path = %repo_file, expected = %repo_root.as_path().display(), actual = %repo_cfg.path, "repo config path mismatch");
                        return ResolvedCheckoutConfig {
                            strategy: global.vcs.git.checkout_strategy.clone(),
                            path: global.vcs.git.checkout_path.clone(),
                        };
                    }
                    return ResolvedCheckoutConfig {
                        strategy: repo_cfg.vcs.git.checkout_strategy.unwrap_or_else(|| global.vcs.git.checkout_strategy.clone()),
                        path: repo_cfg.vcs.git.checkout_path.unwrap_or_else(|| global.vcs.git.checkout_path.clone()),
                    };
                }
                Err(e) => {
                    tracing::warn!(path = %repo_file, err = %e, "failed to parse");
                }
            }
        }
        ResolvedCheckoutConfig { strategy: global.vcs.git.checkout_strategy.clone(), path: global.vcs.git.checkout_path.clone() }
    }
}

#[cfg(test)]
mod tests;
