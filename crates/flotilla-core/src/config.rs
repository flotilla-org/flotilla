use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use flotilla_protocol::NodeId;
use flotilla_resources::RepositoryVcsSpec;
use serde::{Deserialize, Serialize};

use crate::path_context::{canonical_or_original, DaemonHostPath, ExecutionEnvironmentPath};

/// Per-category provider preference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderPreference {
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChangeRequestConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    /// GitHub bot whose review comments should wake a crew (GraphQL login, without `[bot]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_bot_login: Option<String>,
    /// Login of the operator who owns merge decisions on the configured forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_login: Option<String>,
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
    #[serde(default = "default_checkout_path")]
    pub checkout_path: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self { checkout_path: default_checkout_path() }
    }
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
    /// Retired TUI binding section. Deserialize one generation of stored config; remove after the next fleet roll.
    #[serde(default, skip_serializing)]
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
    pub notifications: HashMap<String, String>,
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
    /// This host has no flotillad. The owning daemon may use SSH to actuate
    /// host-direct placements there, but it must not enter the peer mesh.
    pub agentless_ssh: bool,
}

/// The same login-address rule is used for daemon peers and direct SSH
/// environments. A configured user is the account that executes commands.
pub fn ssh_destination(hostname: &str, user: Option<&str>) -> String {
    match user {
        Some(user) if !user.is_empty() => format!("{user}@{hostname}"),
        _ => hostname.to_string(),
    }
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
    #[serde(default)]
    agentless_ssh: bool,
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
                    agentless_ssh: host.agentless_ssh,
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
#[derive(Debug, Deserialize, Serialize)]
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
    #[serde(default)]
    pub relay: Option<RelayConfig>,
    #[serde(default)]
    pub blob_stores: Vec<BlobStoreConfig>,
    /// Retention in days by artifact kind. Unknown kinds use 30 days.
    #[serde(default = "default_artifact_retention_days")]
    pub artifact_retention_days: BTreeMap<String, u64>,
    /// Retention for forced checkout archives, in days.
    #[serde(default = "default_checkout_archive_retention_days")]
    pub checkout_archive_retention_days: u64,
    /// Daemon-wide checkout removal concurrency, across execution environments.
    #[serde(default = "default_checkout_removal_concurrency", deserialize_with = "deserialize_checkout_removal_concurrency")]
    pub checkout_removal_concurrency: NonZeroUsize,
}

pub const DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(2).expect("default removal limit is positive");

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            machine_id: None,
            host_name: None,
            admission: AdmissionConfig::default(),
            credentials: CredentialHealthConfig::default(),
            logging: DaemonLoggingConfig::default(),
            environments: BTreeMap::new(),
            manifests: None,
            relay: None,
            blob_stores: Vec::new(),
            artifact_retention_days: default_artifact_retention_days(),
            checkout_archive_retention_days: default_checkout_archive_retention_days(),
            checkout_removal_concurrency: default_checkout_removal_concurrency(),
        }
    }
}

fn default_checkout_archive_retention_days() -> u64 {
    14
}

fn default_checkout_removal_concurrency() -> NonZeroUsize {
    DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY
}

fn deserialize_checkout_removal_concurrency<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<NonZeroUsize, D::Error> {
    let limit = NonZeroUsize::deserialize(deserializer)?;
    if limit.get() > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(serde::de::Error::custom("checkout_removal_concurrency exceeds semaphore capacity"));
    }
    Ok(limit)
}

fn default_artifact_retention_days() -> BTreeMap<String, u64> {
    [
        ("brief", 3650),
        ("decision-ledger", 3650),
        ("review-round", 3650),
        ("review-bundle", 3650),
        ("explainer", 3650),
        ("recording", 14),
        ("test-report", 14),
        ("raw-test-output", 7),
    ]
    .into_iter()
    .map(|(kind, days)| (kind.to_string(), days))
    .collect()
}

/// Host-local event relay settings. The token is read from a daemon-owned file;
/// this path is never included in a vessel's staged credential set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayConfig {
    pub endpoint: String,
    pub install_id: String,
    pub consumer_token_file: PathBuf,
}

/// Fleet blob storage is configured only on the daemon. The referenced file
/// contains S3 credentials and is never copied into a crew environment.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BlobStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_blob_store_region")]
    pub region: String,
    #[serde(default)]
    pub prefix: String,
    pub credential_file: PathBuf,
    #[serde(default)]
    pub allow_insecure_http: bool,
    /// Public viewer root, including the bucket and object prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_base_url: Option<String>,
}

/// The external form of an artifact. Keep URL construction here so callers do
/// not depend on the current S3 object layout.
pub fn artifact_view_url(artifact: &flotilla_resources::ArtifactSpec, stores: &[BlobStoreConfig]) -> Option<String> {
    artifact_view_url_for_digest(&artifact.digest, stores)
}

/// Used when a caller has a digest reference before its artifact envelope arrives.
pub fn artifact_view_url_for_digest(digest: &str, stores: &[BlobStoreConfig]) -> Option<String> {
    stores.iter().find_map(|store| {
        let base = store.view_base_url.as_deref()?;
        let url = url::Url::parse(base).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.query().is_some() || url.fragment().is_some() {
            return None;
        }
        Some(format!("{}/{}", base.trim_end_matches('/'), digest))
    })
}

#[cfg(test)]
mod artifact_view_url_tests {
    use super::*;

    #[test]
    fn configured_view_url_and_absent_url() {
        let artifact = flotilla_resources::ArtifactSpec::builder()
            .convoy("convoy".into())
            .producer("coder".into())
            .kind("explainer".into())
            .subject("head".into())
            .digest("abc123".into())
            .size(1)
            .media_type("text/markdown".into())
            .expires_at(chrono::Utc::now())
            .build();
        let mut store = BlobStoreConfig {
            endpoint: "https://storage.example.test".into(),
            bucket: "artifacts".into(),
            region: "us-east-1".into(),
            prefix: "fleet".into(),
            credential_file: "credentials.json".into(),
            allow_insecure_http: false,
            view_base_url: None,
        };
        assert_eq!(artifact_view_url(&artifact, &[store.clone()]), None);
        store.view_base_url = Some("https://artifacts.example.test/artifacts/fleet/".into());
        assert_eq!(artifact_view_url(&artifact, &[store]), Some("https://artifacts.example.test/artifacts/fleet/abc123".into()));
    }
}

fn default_blob_store_region() -> String {
    "us-east-1".into()
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

/// Host-local credential settings: health warning windows and explicit daemon identities.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CredentialHealthConfig {
    /// Explicit host-daemon identities, keyed by Forge ID. Grant-delivered
    /// credentials are never selected implicitly for repository observation.
    #[serde(default)]
    pub forgejo: BTreeMap<String, String>,
    /// Days before expiry at which held credential material surfaces as
    /// near-expiry in `flotilla host list` and TUI attention.
    #[serde(default = "default_credential_warning_window_days")]
    pub warning_window_days: u32,
}

impl Default for CredentialHealthConfig {
    fn default() -> Self {
        Self { forgejo: BTreeMap::new(), warning_window_days: default_credential_warning_window_days() }
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
    pub user: Option<String>,
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
    /// Root-first addresses left by in-place navigation. Added after the
    /// original two-field record; default keeps previous files decodable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<String>,
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
    checkout_configs: Mutex<HashMap<PathBuf, RepositoryVcsSpec>>,
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
            checkout_configs: Mutex::new(HashMap::new()),
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
        let physical = canonical_or_original(path.as_path());
        // Keep the first configured spelling while avoiding equivalent roots.
        if paths.iter().any(|root| canonical_or_original(root) == physical) {
            return Ok(());
        }
        paths.push(path.as_path().to_path_buf());
        self.save_observation_roots(paths)
    }

    pub fn remove_observation_root(&self, path: &ExecutionEnvironmentPath) -> Result<(), String> {
        let _guard = self.observation_roots.lock().expect("observation roots mutex poisoned");
        let physical = canonical_or_original(path.as_path());
        let paths = self
            .load_observation_roots()?
            .into_iter()
            .map(ExecutionEnvironmentPath::into_path_buf)
            .filter(|candidate| canonical_or_original(candidate) != physical)
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

    pub fn set_checkout_config(&self, repo_root: &ExecutionEnvironmentPath, vcs: RepositoryVcsSpec) {
        self.checkout_configs.lock().expect("checkout configs mutex poisoned").insert(repo_root.as_path().to_path_buf(), vcs);
    }

    pub fn remove_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) {
        self.checkout_configs.lock().expect("checkout configs mutex poisoned").remove(repo_root.as_path());
    }

    pub fn resolve_checkout_config(&self, repo_root: &ExecutionEnvironmentPath) -> ResolvedCheckoutConfig {
        let global = self.load_config();
        let specs = self.checkout_configs.lock().expect("checkout configs mutex poisoned");
        let git = specs.get(repo_root.as_path()).map(|vcs| &vcs.git);
        ResolvedCheckoutConfig {
            path: git.and_then(|git| git.checkout_path.clone()).unwrap_or_else(|| global.vcs.git.checkout_path.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
