use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use flotilla_protocol::NodeId;
use flotilla_resources::RepositorySpec;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgejo: Option<ForgejoIssueTrackerConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ForgejoIssueTrackerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_agent: Option<String>,
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
    #[serde(default)]
    pub convoy: ConvoyConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConvoyConfig {
    /// Override presence-aware auto-attach for convoy starts without an
    /// explicit `--attach` or `--no-attach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_attach: Option<bool>,
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
    pub dispatch_confirm: HashMap<String, String>,
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

/// Resolved checkout configuration from host defaults.
pub struct ResolvedCheckoutConfig {
    pub strategy: String,
    pub path: String,
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
    pub machine_id: Option<String>,
    pub host_name: Option<String>,
    #[serde(default)]
    pub admission: AdmissionConfig,
    #[serde(default)]
    pub credentials: CredentialHealthConfig,
    #[serde(default)]
    pub logging: DaemonLoggingConfig,
    #[serde(default)]
    pub environments: BTreeMap<String, StaticEnvironmentConfig>,
    #[serde(default)]
    pub manifests: Option<ResourceManifestsConfig>,
}

/// Host-local directory whose resource documents are continuously applied as
/// additive desired state.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceManifestsConfig {
    pub dir: PathBuf,
    /// Stable identity of the manifest tree (normally its forge repository URL).
    pub source: String,
    /// Stable Host resource name of the sole daemon allowed to reconcile it.
    pub reconciler_root: String,
}

/// Host-local structured daemon logging settings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct DaemonLoggingConfig {
    /// `RUST_LOG`-style directives, for example
    /// `info,flotilla_daemon::peer=debug`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default = "default_log_max_bytes")]
    pub max_bytes: u64,
    #[serde(default = "default_log_generations")]
    pub generations: usize,
}

impl Default for DaemonLoggingConfig {
    fn default() -> Self {
        Self { filter: None, max_bytes: default_log_max_bytes(), generations: default_log_generations() }
    }
}

const fn default_log_max_bytes() -> u64 {
    crate::log_file::DEFAULT_MAX_LOG_BYTES
}

const fn default_log_generations() -> usize {
    crate::log_file::DEFAULT_MAX_LOG_ARCHIVES
}

/// Deterministic admission limits enforced by this host.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// Refuse new convoy placement when the volume containing Flotilla's
    /// state directory has less than this many GiB available.
    #[serde(default = "default_free_space_floor_gib")]
    pub free_space_floor_gib: u64,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self { free_space_floor_gib: default_free_space_floor_gib() }
    }
}

/// Host-local credential health settings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CredentialHealthConfig {
    /// Days before expiry at which held credential material surfaces as
    /// near-expiry in `flotilla host list` and TUI attention.
    #[serde(default = "default_credential_warning_window_days")]
    pub warning_window_days: u32,
}

impl Default for CredentialHealthConfig {
    fn default() -> Self {
        Self { warning_window_days: default_credential_warning_window_days() }
    }
}

const fn default_credential_warning_window_days() -> u32 {
    7
}

const fn default_free_space_floor_gib() -> u64 {
    20
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

/// Host-local observation seed. Intentionally incapable of carrying per-path
/// configuration: unknown fields fail the entire file.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationRootsFile {
    #[serde(default)]
    paths: Vec<PathBuf>,
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

/// Owns daemon-side paths and caches the global `FlotillaConfig`.
///
/// NOTE: This struct is accumulating path responsibilities beyond pure config.
/// A future refactor should split config, state, and data storage properly.
pub struct ConfigStore {
    base: DaemonHostPath,
    state_dir: DaemonHostPath,
    global_config: OnceLock<Mutex<FlotillaConfig>>,
    observation_roots: Mutex<()>,
    repository_specs: Mutex<HashMap<PathBuf, RepositorySpec>>,
}

impl ConfigStore {
    /// Create a ConfigStore with explicit config and state directories.
    /// Production callers should pass paths from `PathPolicy`.
    pub fn new(base: DaemonHostPath, state_dir: DaemonHostPath) -> Self {
        Self {
            base,
            state_dir,
            global_config: OnceLock::new(),
            observation_roots: Mutex::new(()),
            repository_specs: Mutex::new(HashMap::new()),
        }
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

    fn observation_roots_file(&self) -> DaemonHostPath {
        self.base.join("observation-roots.toml")
    }

    pub fn load_observation_roots(&self) -> Result<Vec<ExecutionEnvironmentPath>, String> {
        let file = self.observation_roots_file();
        let content = match std::fs::read_to_string(file.as_path()) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("failed to read {file}: {error}")),
        };
        let roots: ObservationRootsFile = toml::from_str(&content).map_err(|error| format!("failed to parse {file}: {error}"))?;
        let mut paths = roots.paths;
        paths.sort();
        paths.dedup();
        Ok(paths.into_iter().map(ExecutionEnvironmentPath::new).collect())
    }

    pub fn add_observation_root(&self, path: &ExecutionEnvironmentPath) -> Result<(), String> {
        let _guard = self.observation_roots.lock().expect("observation roots mutex poisoned");
        let mut paths = self.load_observation_roots()?.into_iter().map(ExecutionEnvironmentPath::into_path_buf).collect::<Vec<_>>();
        paths.push(path.as_path().to_path_buf());
        self.save_observation_roots(paths)
    }

    pub fn remove_observation_root(&self, path: &ExecutionEnvironmentPath) -> Result<(), String> {
        let _guard = self.observation_roots.lock().expect("observation roots mutex poisoned");
        let paths = self
            .load_observation_roots()?
            .into_iter()
            .map(ExecutionEnvironmentPath::into_path_buf)
            .filter(|candidate| candidate != path.as_path())
            .collect();
        self.save_observation_roots(paths)
    }

    fn save_observation_roots(&self, mut paths: Vec<PathBuf>) -> Result<(), String> {
        paths.sort();
        paths.dedup();
        std::fs::create_dir_all(self.base.as_path()).map_err(|error| format!("failed to create {}: {error}", self.base))?;
        let file = self.observation_roots_file();
        let content =
            toml::to_string_pretty(&ObservationRootsFile { paths }).map_err(|error| format!("failed to encode {file}: {error}"))?;
        let temporary = file.as_path().with_extension(format!("toml.tmp-{}", uuid::Uuid::new_v4()));
        std::fs::write(&temporary, content).map_err(|error| format!("failed to write temporary observation roots file: {error}"))?;
        if let Err(error) = std::fs::rename(&temporary, file.as_path()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("failed to replace {file}: {error}"));
        }
        Ok(())
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

    pub fn set_repository_spec(&self, repo_root: &ExecutionEnvironmentPath, spec: RepositorySpec) {
        self.repository_specs.lock().expect("repository specs mutex poisoned").insert(repo_root.as_path().to_path_buf(), spec);
    }

    pub fn remove_repository_spec(&self, repo_root: &ExecutionEnvironmentPath) {
        self.repository_specs.lock().expect("repository specs mutex poisoned").remove(repo_root.as_path());
    }

    pub fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        let global = self.load_config();
        let specs = self.repository_specs.lock().expect("repository specs mutex poisoned");
        let git = specs.get(repo_root.as_path()).map(RepositorySpec::vcs).map(|vcs| &vcs.git);
        ResolvedCheckoutConfig {
            strategy: git.and_then(|git| git.checkout_strategy.clone()).unwrap_or_else(|| global.vcs.git.checkout_strategy.clone()),
            path: git.and_then(|git| git.checkout_path.clone()).unwrap_or_else(|| global.vcs.git.checkout_path.clone()),
        }
    }

    pub fn resolve_change_request_backend(&self, repo_root: &ExecutionEnvironmentPath) -> Option<String> {
        self.repository_specs
            .lock()
            .expect("repository specs mutex poisoned")
            .get(repo_root.as_path())
            .and_then(|spec| spec.change_request().backend.clone())
            .or_else(|| self.load_config().change_request.preference.backend)
    }
}

#[cfg(test)]
mod tests;
