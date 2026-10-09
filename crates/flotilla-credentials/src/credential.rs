use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_core::providers::{
    discovery::{EnvVars, EnvironmentBag},
    environment::PreparedEnvironmentAuth,
    ChannelLabel, CommandRunner, HttpClient, ReqwestHttpClient,
};
use flotilla_protocol::DaemonHostPath;
use flotilla_resources::{
    capped_github_app_permissions, Clock, CredentialConsumer, CredentialExpiry, CredentialLifecycle, CredentialSource, CredentialSpec,
    CredentialSpecSpec, Forge, ForgeKind, Project, Repository, RepositoryIdentity, RepositoryKey, ResourceBackend, ResourceError,
    SystemClock, AMBIENT_CLAUDE_CREDENTIAL_SCOPE,
};
use futures::future::BoxFuture;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OnceCell, RwLock};
use url::Url;

use crate::vessel_config::{
    agent_environment_fragment, compose, crew_gitconfig_fragments, Fragment, GitConfigKey, Merge, Provenance, TargetId, TargetKey,
};

/// Shared, side-effect-free authority check for skill staging and the candidate
/// pre-roll probe. Returns the repository name used to narrow the App token.
pub(crate) fn skill_source_repository(credential_name: &str, spec: &CredentialSpecSpec, repository: &str) -> Result<String, String> {
    let (CredentialConsumer::GithubApp { installation_id, installation_repository, .. }, CredentialSource::GithubApp { .. }) =
        (&spec.consumer, &spec.source)
    else {
        return Err(bounded_adapter_error(
            credential_name,
            spec.consumer.adapter_name(),
            "skill-source credentials must use the github-app adapter and source",
        ));
    };
    if spec.lifecycle != CredentialLifecycle::Refreshable {
        return Err(bounded_adapter_error(credential_name, "github-app", "GitHub App credentials must use the refreshable lifecycle"));
    }
    match (installation_id, installation_repository) {
        (Some(_), None) | (None, Some(_)) => {}
        (Some(_), Some(_)) => return Err("declare either `installation_id` or `installation_repository`, not both".into()),
        (None, None) => return Err("declare either `installation_id` or `installation_repository`".into()),
    }
    let parsed = Url::parse(repository)
        .map_err(|error| bounded_adapter_error(credential_name, "github-app", &format!("invalid repository URL: {error}")))?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("github.com") {
        return Err(bounded_adapter_error(credential_name, "github-app", "skill source repository must be an HTTPS github.com URL"));
    }
    let path = parsed.path().trim_matches('/');
    let components = path.strip_suffix(".git").unwrap_or(path).split('/').collect::<Vec<_>>();
    if components.len() != 2 || components.iter().any(|component| component.is_empty()) {
        return Err(bounded_adapter_error(credential_name, "github-app", "skill source repository must identify one owner/repository"));
    }
    Ok(components[1].to_string())
}

async fn prune_abandoned_skill_source_tokens(runner: &dyn CommandRunner, root: &Path) -> Result<(), String> {
    const SCRIPT: &str = r#"[ -d "$1" ] || exit 0
find "$1" -type f -name 'token-*' ! -name '*.in-use.*' -exec sh -c '
  for token do
    active=false
    dead_owner=false
    for marker in "$token".in-use.*; do
      [ -f "$marker" ] || continue
      pid=${marker##*.}
      case "$pid" in *[!0-9]*|"") active=true; continue ;; esac
      if signal_error=$(kill -0 "$pid" 2>&1); then active=true; continue; fi
      case "$signal_error" in *[Pp]ermiss*|*permitted*) active=true; continue ;; esac
      dead_owner=true
      rm -f -- "$marker"
    done
    # A stager creates its marker before reading the token. Marker-less tokens
    # may have just been minted, so only age can make them eligible for cleanup.
    if [ "$active" = false ] && { [ "$dead_owner" = true ] || [ -n "$(find "$token" -prune -mtime +6 -print)" ]; }; then
      rm -f -- "$token"
    fi
  done
' _ {} +"#;
    let path = root.to_string_lossy();
    runner.run("sh", &["-c", SCRIPT, "flotilla-prune-skill-tokens", &path], Path::new("/"), &ChannelLabel::Default).await.map(|_| ())
}

#[derive(Serialize)]
struct GithubAppJwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Serialize)]
struct GithubAppTokenRequest {
    repositories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct GithubAppTokenResponse {
    #[serde(default)]
    permissions: Option<BTreeMap<String, String>>,
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct GithubAppInstallationResponse {
    id: u64,
}

#[derive(Clone, Debug)]
struct GithubAppToken {
    permissions: Option<BTreeMap<String, String>>,
    value: String,
    expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct GithubAppMintRequest {
    installation_id: u64,
    app_id_path: String,
    private_key_path: String,
    repositories: Vec<String>,
    permissions: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GithubAppInstallationRequest {
    repository: String,
    app_id_path: String,
    private_key_path: String,
}

#[derive(Debug)]
enum GithubAppMintError {
    InstallationNotFound(String),
    Transient(String),
    Other(String),
}

impl std::fmt::Display for GithubAppMintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstallationNotFound(message) | Self::Transient(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

#[async_trait]
trait GithubAppTokenMinter: Send + Sync {
    async fn resolve_installation(&self, request: &GithubAppInstallationRequest) -> Result<u64, String>;
    async fn mint(&self, request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError>;
}

struct RealGithubAppTokenMinter {
    env: Arc<dyn EnvVars>,
    http: Arc<dyn HttpClient>,
    clock: Arc<dyn Clock>,
}

struct GithubAppMinting {
    clock: Arc<dyn Clock>,
    minter: Arc<dyn GithubAppTokenMinter>,
}

#[async_trait]
impl GithubAppTokenMinter for RealGithubAppTokenMinter {
    async fn resolve_installation(&self, request: &GithubAppInstallationRequest) -> Result<u64, String> {
        let jwt = self.jwt(&request.app_id_path, &request.private_key_path).await?;
        let url = format!("https://api.github.com/repos/{}/installation", request.repository);
        let http_request = flotilla_resources::tls::client()
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .build()
            .map_err(|error| format!("build installation resolution request: {error}"))?;
        let label = ChannelLabel::http_from_url(&url);
        let response = self.http.execute(http_request, &label).await.map_err(|error| format!("resolve installation: {error}"))?;
        if !response.status().is_success() {
            let detail = String::from_utf8_lossy(response.body());
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Err(format!(
                    "GitHub App is not installed on repository `{}` (HTTP {}): {detail}",
                    request.repository,
                    response.status()
                ));
            }
            return Err(format!(
                "failed to resolve GitHub App installation for repository `{}` (HTTP {}): {detail}",
                request.repository,
                response.status()
            ));
        }
        serde_json::from_slice::<GithubAppInstallationResponse>(response.body())
            .map(|response| response.id)
            .map_err(|error| format!("decode installation resolution response for `{}`: {error}", request.repository))
    }

    async fn mint(&self, request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
        let jwt = self.jwt(&request.app_id_path, &request.private_key_path).await.map_err(GithubAppMintError::Other)?;
        let url = format!("https://api.github.com/app/installations/{}/access_tokens", request.installation_id);
        let http_request = flotilla_resources::tls::client()
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&GithubAppTokenRequest { repositories: request.repositories.clone(), permissions: request.permissions.clone() })
            .build()
            .map_err(|error| GithubAppMintError::Other(format!("build installation token request: {error}")))?;
        let label = ChannelLabel::http_from_url(&url);
        let response = self
            .http
            .execute(http_request, &label)
            .await
            .map_err(|error| GithubAppMintError::Other(format!("mint installation token: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            let detail = String::from_utf8_lossy(response.body());
            return Err(GithubAppMintError::InstallationNotFound(format!(
                "mint installation token: GitHub returned HTTP {}: {detail}",
                response.status()
            )));
        }
        if !response.status().is_success() {
            let detail = String::from_utf8_lossy(response.body());
            if response.status().is_server_error() {
                return Err(GithubAppMintError::Transient(format!(
                    "mint installation token: GitHub returned HTTP {}: {detail}",
                    response.status()
                )));
            }
            return Err(GithubAppMintError::Other(format!(
                "mint installation token: GitHub returned HTTP {}: {detail}",
                response.status()
            )));
        }
        let response: GithubAppTokenResponse = serde_json::from_slice(response.body())
            .map_err(|error| GithubAppMintError::Other(format!("decode installation token response: {error}")))?;
        if response.token.trim().is_empty() {
            return Err(GithubAppMintError::Other("installation token response was empty".to_string()));
        }
        Ok(GithubAppToken { value: response.token, expires_at: response.expires_at, permissions: response.permissions })
    }
}

impl RealGithubAppTokenMinter {
    async fn jwt(&self, app_id_path: &str, private_key_path: &str) -> Result<String, String> {
        let app_id = tokio::fs::read_to_string(expand_path(&*self.env, app_id_path))
            .await
            .map_err(|error| format!("read host-local App id: {error}"))?;
        let private_key = tokio::fs::read(expand_path(&*self.env, private_key_path))
            .await
            .map_err(|error| format!("read host-local private key: {error}"))?;
        let now = self.clock.now().timestamp();
        let claims = GithubAppJwtClaims { iat: now - 60, exp: now + 9 * 60, iss: app_id.trim().to_string() };
        if claims.iss.is_empty() {
            return Err("host-local App id is empty".to_string());
        }
        let key = EncodingKey::from_rsa_pem(&private_key).map_err(|error| format!("decode host-local private key: {error}"))?;
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|error| format!("sign GitHub App JWT: {error}"))
    }
}

/// Metadata-only view of the ambient claude credentials file. Deliberately
/// captures nothing but expiry timestamps — the token fields never
/// deserialize into daemon memory.
#[derive(Deserialize)]
struct AmbientClaudeCredentialsMetadata {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<AmbientClaudeOauthMetadata>,
}

#[derive(Deserialize)]
struct AmbientClaudeOauthMetadata {
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "refreshTokenExpiresAt")]
    refresh_token_expires_at: Option<i64>,
}

type LedgerDeliveryRecord = BTreeMap<String, BTreeMap<String, String>>;
type GithubAppDeliveryLocks = BTreeMap<(String, String), Weak<Mutex<()>>>;

pub struct CredentialStore {
    backend: ResourceBackend,
    namespace: String,
    env: Arc<dyn EnvVars>,
    host_bag: EnvironmentBag,
    host_runner: Arc<dyn CommandRunner>,
    clock: Arc<dyn Clock>,
    github_app_minter: Arc<dyn GithubAppTokenMinter>,
    state_dir: PathBuf,
    prepared: Mutex<BTreeSet<(String, String)>>,
    work_deliveries: Mutex<BTreeMap<String, BTreeSet<String>>>,
    capability_scopes: Mutex<BTreeMap<(String, String), Vec<String>>>,
    capability_endpoints: Mutex<BTreeMap<(String, String), BTreeMap<String, String>>>,
    ledger_delivery_environment: Mutex<BTreeMap<String, LedgerDeliveryRecord>>,
    materials: Mutex<BTreeMap<(String, String), String>>,
    git_config_fragments: Mutex<BTreeMap<String, BTreeMap<String, Fragment>>>,
    registry_configs: Mutex<BTreeMap<String, PathBuf>>,
    registry_cache_maintenance: RwLock<()>,
    github_app_deliveries: Mutex<BTreeMap<(String, String), GithubAppDelivery>>,
    github_app_delivery_locks: Mutex<GithubAppDeliveryLocks>,
    github_app_adoption_failures: Mutex<BTreeMap<String, usize>>,
    github_app_installations: Mutex<BTreeMap<GithubAppInstallationRequest, u64>>,
    cleaned_delivery_environments: Mutex<BTreeMap<String, Arc<OnceCell<()>>>>,
}

const GITHUB_APP_REFRESH_MARGIN: Duration = Duration::minutes(5);
const GITHUB_APP_MIN_REFRESH_LEAD: Duration = Duration::minutes(15);
const GITHUB_APP_INITIAL_REFRESH_BACKOFF: Duration = Duration::seconds(30);
const GITHUB_APP_MAX_REFRESH_BACKOFF: Duration = Duration::minutes(5);

fn github_app_refresh_at(issued_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> DateTime<Utc> {
    let half_life = issued_at + (expires_at - issued_at) / 2;
    half_life.min(expires_at - GITHUB_APP_MIN_REFRESH_LEAD)
}

#[derive(Clone)]
struct GithubAppDelivery {
    generation: uuid::Uuid,
    request: GithubAppMintRequest,
    effective_permissions: Option<BTreeMap<String, String>>,
    runner: Arc<dyn CommandRunner>,
    token_file: PathBuf,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    refresh_failures: usize,
    next_refresh_attempt_at: Option<DateTime<Utc>>,
    installation_repository: Option<String>,
    scope: Option<GithubAppScope>,
}

impl GithubAppDelivery {
    fn record_refresh_failure(&mut self, now: DateTime<Utc>) -> bool {
        self.refresh_failures += 1;
        let inside_margin = now + GITHUB_APP_REFRESH_MARGIN >= self.expires_at;
        if inside_margin {
            self.next_refresh_attempt_at = None;
        } else {
            let mut delay = GITHUB_APP_INITIAL_REFRESH_BACKOFF.min(GITHUB_APP_MAX_REFRESH_BACKOFF);
            for _ in 1..self.refresh_failures {
                if delay >= GITHUB_APP_MAX_REFRESH_BACKOFF {
                    break;
                }
                delay = (delay * 2).min(GITHUB_APP_MAX_REFRESH_BACKOFF);
            }
            self.next_refresh_attempt_at = Some(now + delay);
        }
        self.refresh_failures >= GITHUB_APP_REFRESH_FAILURE_THRESHOLD || inside_margin
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GithubAppScope {
    pub fixed_repositories: BTreeSet<RepositoryKey>,
    pub projects: BTreeSet<String>,
    pub permissions: Option<BTreeMap<String, String>>,
}

#[derive(Debug)]
pub struct CredentialRefreshError {
    pub environment_ref: String,
    pub credential_name: Option<String>,
    pub message: String,
    pub should_surface: bool,
}

const GITHUB_APP_REFRESH_FAILURE_THRESHOLD: usize = 3;

type ResolvedGithubApp = (GithubAppMintRequest, DateTime<Utc>, Option<BTreeMap<String, String>>);

#[derive(Debug)]
struct ResolvedMaterial {
    value: String,
    github_app: Option<ResolvedGithubApp>,
}

#[derive(Default)]
struct AdapterDelivery {
    env: BTreeMap<String, String>,
    git_credential: Option<GitCredentialContribution>,
}

struct GitCredentialContribution {
    fragment: Fragment,
    preflight: Option<GitCredentialPreflight>,
}

struct CredentialDeliveryPaths {
    base: PathBuf,
    git_config: PathBuf,
}

impl CredentialDeliveryPaths {
    fn new(base: PathBuf) -> Self {
        let git_config = base.join("credentials/gitconfig");
        Self { base, git_config }
    }

    fn credential_dir(&self, name: &str) -> PathBuf {
        self.base.join("credentials").join(safe_component(name))
    }
}

#[derive(bon::Builder)]
struct PendingGitPreflight {
    credential_name: String,
    adapter: String,
    material: String,
    cache_key: (String, String),
    preflight: GitCredentialPreflight,
}

enum GitCredentialPreflight {
    Gh,
    GithubApp { token_file: String },
    Forgejo { host: String, token_file: String, username: String },
    GitHttpToken { host: String },
}

impl GitCredentialPreflight {
    async fn run(&self, runner: &dyn CommandRunner, material: &str, git_config_path: &Path) -> Result<(), String> {
        let git_config_path = git_config_path.to_string_lossy();
        match self {
            Self::Gh => {
                runner
                    .run_with_input(
                        "sh",
                        &[
                            "-c",
                            "IFS= read -r token; export GH_TOKEN=\"$token\" GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill >/dev/null",
                            "flotilla-gh-git-preflight",
                            &git_config_path,
                        ],
                        Path::new("/"),
                        &ChannelLabel::Default,
                        material.as_bytes(),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("Git credential preflight failed: {error}"))
            }
            Self::GithubApp { token_file } => runner
                .run(
                    "sh",
                    &[
                        "-c",
                        "unset GH_TOKEN GITHUB_TOKEN; export GITHUB_TOKEN_FILE=\"$1\" GIT_CONFIG_GLOBAL=\"$2\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill >/dev/null",
                        "flotilla-github-app-git-preflight",
                        token_file,
                        &git_config_path,
                    ],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("Git credential preflight failed: {error}")),
            Self::Forgejo { host, token_file, username } => runner
                .run(
                    "sh",
                    &[
                        "-c",
                        "export GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0 FORGEJO_TOKEN_FILE=\"$2\" FORGEJO_USERNAME=\"$3\"; printf 'protocol=https\\nhost=%s\\n\\n' \"$4\" | git credential fill >/dev/null",
                        "flotilla-forgejo-git-preflight",
                        &git_config_path,
                        token_file,
                        username,
                        host,
                    ],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("Git credential preflight failed: {error}")),
            Self::GitHttpToken { host } => runner
                .run(
                    "sh",
                    &[
                        "-c",
                        "export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=%s\\n\\n' \"$2\" | git credential fill >/dev/null",
                        "flotilla-git-http-token-preflight",
                        &git_config_path,
                        host,
                    ],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await
                .map(|_| ())
                .map_err(|error| format!("Git credential preflight failed: {error}")),
        }
    }
}

impl CredentialStore {
    /// Remove staging files left by a previous daemon process. The current
    /// config base can differ from the previous one when XDG_RUNTIME_DIR
    /// becomes available, so inspect the state fallback as well.
    pub fn cleanup_stale_github_app_token_files(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let mut errors = Vec::new();
            if let Err(error) = cleanup_stale_github_app_token_files_in(&self.state_dir).await {
                errors.push(error);
            }
            match self.delivery_paths(&*self.host_runner).await {
                Ok(paths) if paths.base != self.state_dir => {
                    if let Err(error) = cleanup_stale_github_app_token_files_in(&paths.base).await {
                        errors.push(error);
                    }
                }
                Err(error) => errors.push(error),
                Ok(_) => {}
            }
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            Ok(())
        })
    }

    pub fn new(
        backend: ResourceBackend,
        namespace: &str,
        env: Arc<dyn EnvVars>,
        host_bag: EnvironmentBag,
        host_runner: Arc<dyn CommandRunner>,
        state_dir: PathBuf,
    ) -> Self {
        Self::new_with_http(backend, namespace, env, host_bag, host_runner, Arc::new(ReqwestHttpClient::new()), state_dir)
    }

    pub fn new_with_http(
        backend: ResourceBackend,
        namespace: &str,
        env: Arc<dyn EnvVars>,
        host_bag: EnvironmentBag,
        host_runner: Arc<dyn CommandRunner>,
        http: Arc<dyn HttpClient>,
        state_dir: PathBuf,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let github_app_minter = Arc::new(RealGithubAppTokenMinter { env: Arc::clone(&env), http, clock: Arc::clone(&clock) });
        Self::new_with_github_app_minter(
            backend,
            namespace,
            env,
            host_bag,
            host_runner,
            GithubAppMinting { clock, minter: github_app_minter },
            state_dir,
        )
    }

    fn new_with_github_app_minter(
        backend: ResourceBackend,
        namespace: &str,
        env: Arc<dyn EnvVars>,
        host_bag: EnvironmentBag,
        host_runner: Arc<dyn CommandRunner>,
        github_app: GithubAppMinting,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            backend,
            namespace: namespace.to_string(),
            env,
            host_bag,
            host_runner,
            clock: github_app.clock,
            github_app_minter: github_app.minter,
            state_dir,
            prepared: Mutex::new(BTreeSet::new()),
            work_deliveries: Mutex::new(BTreeMap::new()),
            capability_scopes: Mutex::new(BTreeMap::new()),
            capability_endpoints: Mutex::new(BTreeMap::new()),
            ledger_delivery_environment: Mutex::new(BTreeMap::new()),
            materials: Mutex::new(BTreeMap::new()),
            git_config_fragments: Mutex::new(BTreeMap::new()),
            registry_configs: Mutex::new(BTreeMap::new()),
            registry_cache_maintenance: RwLock::new(()),
            github_app_deliveries: Mutex::new(BTreeMap::new()),
            github_app_delivery_locks: Mutex::new(BTreeMap::new()),
            github_app_adoption_failures: Mutex::new(BTreeMap::new()),
            github_app_installations: Mutex::new(BTreeMap::new()),
            cleaned_delivery_environments: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn vessel_config_fragments<'a>(
        &'a self,
        credential_refs: &'a BTreeSet<String>,
        environment: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Vec<Fragment>, String>> {
        Box::pin(async move {
            let mut fragments = Vec::new();
            for name in credential_refs {
                let spec = self.spec(name).await?;
                match spec.consumer {
                    CredentialConsumer::Codex if !environment.contains_key("CODEX_HOME") => {
                        fragments.push(codex_home_fragment(name, format!("credential-delivery-pending:{name}")))
                    }
                    _ => {}
                }
            }
            Ok(fragments)
        })
    }

    pub fn vessel_config_fragments_for_runner<'a>(
        &'a self,
        credential_refs: &'a BTreeSet<String>,
        environment: &'a BTreeMap<String, String>,
        runner: &'a dyn CommandRunner,
    ) -> BoxFuture<'a, Result<Vec<Fragment>, String>> {
        Box::pin(async move {
            let mut codex_credentials = Vec::new();
            for name in credential_refs {
                let spec = self.spec(name).await?;
                if matches!(spec.consumer, CredentialConsumer::Codex) && !environment.contains_key("CODEX_HOME") {
                    codex_credentials.push(name);
                }
            }
            if codex_credentials.is_empty() {
                return Ok(Vec::new());
            }
            let paths = self.delivery_paths(runner).await?;
            Ok(codex_credentials
                .into_iter()
                .map(|name| codex_home_fragment(name, paths.credential_dir(name).join("codex").to_string_lossy()))
                .collect())
        })
    }

    pub fn held_credentials(&self) -> BoxFuture<'_, Result<BTreeSet<String>, String>> {
        Box::pin(async move {
            let specs = self
                .backend
                .clone()
                .definitions::<CredentialSpec>(&self.namespace)
                .list()
                .await
                .map_err(|error| format!("list credential declarations: {error}"))?;
            let mut held = BTreeSet::new();
            for spec in specs {
                if self.source_is_available(&spec.spec).await {
                    held.insert(spec.metadata.name);
                }
            }
            Ok(held)
        })
    }

    /// Expiry metadata for held material, keyed by scope name. Timestamps
    /// only — material is never read into the result. Declared
    /// `CredentialSpec`s contribute here once an adapter can express expiry
    /// without touching material; today none of the declared sources carry
    /// such metadata, so the map holds only the ambient claude login.
    pub fn credential_expiry(&self) -> BoxFuture<'_, BTreeMap<String, CredentialExpiry>> {
        Box::pin(async move {
            let mut expiry = BTreeMap::new();
            if let Some(ambient) = self.ambient_claude_expiry().await {
                expiry.insert(AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), ambient);
            }
            expiry
        })
    }

    async fn ambient_claude_expiry(&self) -> Option<CredentialExpiry> {
        let path = match self.env.get("CLAUDE_CONFIG_DIR").filter(|dir| !dir.trim().is_empty()) {
            Some(dir) => PathBuf::from(dir).join(".credentials.json"),
            None => self.expand_path("~/.claude/.credentials.json"),
        };
        let contents = tokio::fs::read(&path).await.ok()?;
        parse_ambient_claude_expiry(&contents, &path)
    }

    pub fn remote_ambient_claude_expiry<'a>(
        host_bag: &'a EnvironmentBag,
        runner: &'a dyn CommandRunner,
    ) -> BoxFuture<'a, Option<CredentialExpiry>> {
        Box::pin(async move {
            let path = match host_bag.find_env_var("CLAUDE_CONFIG_DIR").filter(|dir| !dir.trim().is_empty()) {
                Some(dir) => PathBuf::from(dir).join(".credentials.json"),
                None => PathBuf::from(host_bag.find_env_var("HOME")?).join(".claude/.credentials.json"),
            };
            let contents = runner.run("cat", &[path.to_str()?], Path::new("/"), &ChannelLabel::Default).await.ok()?;
            parse_ambient_claude_expiry(contents.as_bytes(), &path)
        })
    }
}

fn parse_ambient_claude_expiry(contents: &[u8], path: &Path) -> Option<CredentialExpiry> {
    let metadata: AmbientClaudeCredentialsMetadata = match serde_json::from_slice(contents) {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::debug!(path = %path.display(), line = error.line(), "ambient claude credentials file is not readable as JSON");
            return None;
        }
    };
    let oauth = metadata.claude_ai_oauth?;
    let expires_at = oauth.expires_at.and_then(epoch_to_datetime);
    let refresh_expires_at = oauth.refresh_token_expires_at.and_then(epoch_to_datetime);
    if expires_at.is_none() && refresh_expires_at.is_none() {
        return None;
    }
    Some(CredentialExpiry::builder().maybe_expires_at(expires_at).maybe_refresh_expires_at(refresh_expires_at).build())
}

impl CredentialStore {
    async fn github_app_delivery_lock(&self, key: &(String, String)) -> Arc<Mutex<()>> {
        let mut locks = self.github_app_delivery_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.clone(), Arc::downgrade(&lock));
        lock
    }

    #[cfg(test)]
    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        self.prepare_scoped(environment_ref, credential_refs, &BTreeMap::new(), runner).await
    }

    #[cfg(test)]
    pub(crate) async fn prepare_scoped(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        self.prepare_scoped_with_permissions(environment_ref, credential_refs, credential_scopes, &BTreeMap::new(), runner).await
    }

    pub fn prepare_scoped_with_permissions<'a>(
        &'a self,
        environment_ref: &'a str,
        credential_refs: &'a BTreeSet<String>,
        credential_scopes: &'a BTreeMap<String, BTreeSet<RepositoryKey>>,
        credential_permissions: &'a BTreeMap<String, BTreeMap<String, String>>,
        runner: Arc<dyn CommandRunner>,
    ) -> BoxFuture<'a, Result<Vec<(String, String)>, String>> {
        Box::pin(async move {
            self.prepare_scoped_inner(environment_ref, credential_refs, credential_scopes, credential_permissions, runner).await
        })
    }

    async fn prepare_scoped_inner(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        credential_permissions: &BTreeMap<String, BTreeMap<String, String>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut specs = Vec::new();
        for name in credential_refs {
            let spec = self.spec(name).await?;
            if matches!(spec.consumer, CredentialConsumer::DockerRegistry { .. }) {
                continue;
            }
            specs.push((name.clone(), spec));
        }
        // Each adapter delivers fixed env-var names, so a second credential on
        // the same adapter would silently overwrite the first's wiring. Fail
        // loudly instead (registry credentials multiplex per image in
        // prepare_registry_pull and are exempt).
        let mut seen_adapters = BTreeSet::new();
        let mut seen_git_http_hosts = BTreeSet::new();
        for (name, spec) in &specs {
            let git_http_host = match &spec.consumer {
                CredentialConsumer::GitHttpToken { host, .. } => Some(canonical_git_http_host(host)),
                CredentialConsumer::Forgejo { forge_ref, .. } => {
                    Some(self.forgejo_server_url(forge_ref).await.and_then(|url| forgejo_git_host(&url)))
                }
                CredentialConsumer::Gh | CredentialConsumer::GithubApp { .. } => Some(Ok("github.com".to_string())),
                _ => None,
            };
            if let Some(host) = git_http_host {
                let host = host.map_err(|error| bounded_adapter_error(name, spec.consumer.adapter_name(), &error))?;
                if !seen_git_http_hosts.insert(host) {
                    return Err(bounded_adapter_error(
                        name,
                        spec.consumer.adapter_name(),
                        "multiple granted credentials target the same Git HTTPS host",
                    ));
                }
                if matches!(spec.consumer, CredentialConsumer::GitHttpToken { .. }) {
                    continue;
                }
            }
            if !seen_adapters.insert(spec.consumer.delivery_slot()) {
                return Err(bounded_adapter_error(
                    name,
                    spec.consumer.adapter_name(),
                    "multiple granted credentials use this adapter for one environment",
                ));
            }
        }
        if specs.is_empty() {
            return Ok(Vec::new());
        }
        let delivery_paths = if specs.iter().any(|(_, spec)| {
            matches!(
                spec.consumer,
                CredentialConsumer::Gh
                    | CredentialConsumer::GithubApp { .. }
                    | CredentialConsumer::Forgejo { .. }
                    | CredentialConsumer::GitHttpToken { .. }
                    | CredentialConsumer::ClaudeOauth { .. }
                    | CredentialConsumer::Codex
                    | CredentialConsumer::ReviewBundleStore { .. }
            )
        }) {
            Some(self.delivery_paths(&*runner).await?)
        } else {
            None
        };
        if specs.iter().any(|(_, spec)| matches!(spec.consumer, CredentialConsumer::GithubApp { .. })) {
            let paths = delivery_paths.as_ref().expect("GitHub App adapter resolves delivery paths");
            let cleanup = {
                let mut cleanups = self.cleaned_delivery_environments.lock().await;
                Arc::clone(cleanups.entry(environment_ref.to_string()).or_insert_with(|| Arc::new(OnceCell::new())))
            };
            if let Err(error) = cleanup.get_or_try_init(|| cleanup_stale_github_app_token_files_with_runner(&*runner, &paths.base)).await {
                tracing::warn!(%environment_ref, %error, "failed to clean up delivered GitHub App token staging files");
            }
        }
        let mut env = BTreeMap::new();
        let mut ledger_deliveries = BTreeMap::new();
        let mut new_git_config_fragments = BTreeMap::new();
        let mut git_config_owner = None;
        let mut pending_git_preflights = Vec::new();
        let mut prepared_cache_keys = Vec::new();
        for (name, spec) in &specs {
            let cache_key = (environment_ref.to_string(), name.clone());
            let _delivery_guard = if matches!(spec.consumer, CredentialConsumer::GithubApp { .. }) {
                Some(self.github_app_delivery_lock(&cache_key).await.lock_owned().await)
            } else {
                None
            };
            if let CredentialConsumer::GithubApp { permissions: declaration, .. } = &spec.consumer {
                let requested = capped_github_app_permissions(credential_permissions.get(name), declaration.as_ref())?;
                if self.github_app_deliveries.lock().await.get(&cache_key).is_some_and(|existing| existing.request.permissions != requested)
                {
                    return Err(bounded_adapter_error(
                        name,
                        "github-app",
                        "crews sharing one environment require different minted permissions; use separate credential environments",
                    ));
                }
            }
            let cached_material = {
                let materials = self.materials.lock().await;
                materials.get(&cache_key).cloned()
            };
            // Issued material is minted once per environment and evicted with
            // that environment. Refreshable material is resolved for every
            // preparation; static material follows the same environment cache.
            let resolved = if spec.lifecycle == CredentialLifecycle::Refreshable {
                self.resolve_for_adapter(name, spec, credential_scopes.get(name), credential_permissions.get(name)).await?
            } else if let Some(material) = cached_material {
                ResolvedMaterial { value: material, github_app: None }
            } else {
                let material = self.resolve_for_adapter(name, spec, credential_scopes.get(name), credential_permissions.get(name)).await?;
                self.materials.lock().await.insert(cache_key.clone(), material.value.clone());
                material
            };
            let already_prepared = spec.lifecycle != CredentialLifecycle::Refreshable && self.prepared.lock().await.contains(&cache_key);
            let material = resolved.value.trim_end();
            if let Err(error) = validate_scalar_material(name, spec.consumer.adapter_name(), material) {
                self.materials.lock().await.remove(&cache_key);
                return Err(error);
            }
            if let Some((request, _, _)) = &resolved.github_app {
                let deliveries = self.github_app_deliveries.lock().await;
                if deliveries.get(&cache_key).is_some_and(|existing| existing.request.permissions != request.permissions) {
                    return Err(bounded_adapter_error(
                        name,
                        "github-app",
                        "crews sharing one environment require different minted permissions; use separate credential environments",
                    ));
                }
            }
            let delivered =
                match self.prepare_adapter(name, spec, material, Arc::clone(&runner), already_prepared, delivery_paths.as_ref()).await {
                    Ok(delivered) => delivered,
                    Err(message) => {
                        self.materials.lock().await.remove(&cache_key);
                        return Err(bounded_adapter_error(name, spec.consumer.adapter_name(), &message.replace(material, "[redacted]")));
                    }
                };
            ledger_deliveries.insert(
                name.clone(),
                delivered
                    .env
                    .iter()
                    .filter(|(key, _)| matches!(key.as_str(), "GITHUB_TOKEN_FILE" | "FORGEJO_TOKEN_FILE" | "FORGEJO_API_URL"))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<BTreeMap<_, _>>(),
            );
            env.extend(delivered.env);
            if let Some((request, expires_at, effective_permissions)) = resolved.github_app {
                let paths = delivery_paths.as_ref().expect("GitHub App adapter resolves delivery paths");
                self.github_app_deliveries.lock().await.insert(
                    cache_key.clone(),
                    GithubAppDelivery {
                        generation: uuid::Uuid::new_v4(),
                        // A successful mint accepts the explicit requested permission ceiling.
                        // Without a response or an explicit request, permissions stay unknown.
                        effective_permissions: effective_permissions.or_else(|| request.permissions.clone()),
                        request,
                        runner: Arc::clone(&runner),
                        token_file: github_app_token_file(paths, name),
                        issued_at: self.clock.now(),
                        expires_at,
                        refresh_failures: 0,
                        next_refresh_attempt_at: None,
                        installation_repository: match &spec.consumer {
                            CredentialConsumer::GithubApp { installation_repository, .. } => installation_repository.clone(),
                            _ => None,
                        },
                        scope: None,
                    },
                );
            }
            if let Some(git_credential) = delivered.git_credential {
                git_config_owner.get_or_insert_with(|| (name.clone(), spec.consumer.adapter_name().to_string(), cache_key.clone()));
                new_git_config_fragments.insert(name.clone(), git_credential.fragment);
                if let Some(preflight) = git_credential.preflight {
                    pending_git_preflights.push(
                        PendingGitPreflight::builder()
                            .credential_name(name.clone())
                            .adapter(spec.consumer.adapter_name().to_string())
                            .material(material.to_string())
                            .cache_key(cache_key.clone())
                            .preflight(preflight)
                            .build(),
                    );
                }
            }
            prepared_cache_keys.push(cache_key);
        }
        if !new_git_config_fragments.is_empty() {
            let mut fragments_by_environment = self.git_config_fragments.lock().await;
            let mut composed_fragments = fragments_by_environment.get(environment_ref).cloned().unwrap_or_default();
            composed_fragments.extend(new_git_config_fragments);
            let gitconfig =
                match compose(TargetId::GitConfig, crew_gitconfig_fragments().into_iter().chain(composed_fragments.values().cloned())) {
                    Ok(gitconfig) => gitconfig,
                    Err(error) => return Err(format!("compose shared Git config: {error}")),
                };
            let delivery_paths = delivery_paths.as_ref().expect("Git credential adapters resolve delivery paths");
            if let Err(error) = runner.write_file(&delivery_paths.git_config, &gitconfig.contents).await {
                let (name, adapter, cache_key) = git_config_owner.expect("Git config fragments have an owner");
                self.materials.lock().await.remove(&cache_key);
                return Err(bounded_adapter_error(&name, &adapter, &format!("write shared Git config: {error}")));
            }
            env.insert("GIT_CONFIG_GLOBAL".to_string(), delivery_paths.git_config.to_string_lossy().into_owned());
            env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
            for pending in pending_git_preflights {
                if let Err(message) = pending.preflight.run(&*runner, &pending.material, &delivery_paths.git_config).await {
                    self.materials.lock().await.remove(&pending.cache_key);
                    return Err(bounded_adapter_error(
                        &pending.credential_name,
                        &pending.adapter,
                        &message.replace(&pending.material, "[redacted]"),
                    ));
                }
            }
            fragments_by_environment.insert(environment_ref.to_string(), composed_fragments);
        }
        // Retain only paths and endpoint metadata, keyed by credential so a
        // redelivery can replace its own path without disturbing another grant.
        self.ledger_delivery_environment.lock().await.entry(environment_ref.to_string()).or_default().extend(ledger_deliveries);
        let mut scopes = self.capability_scopes.lock().await;
        for key in &prepared_cache_keys {
            let repositories =
                credential_scopes.get(&key.1).map(|scope| scope.iter().map(ToString::to_string).collect()).unwrap_or_default();
            scopes.insert(key.clone(), repositories);
        }
        drop(scopes);
        self.prepared.lock().await.extend(prepared_cache_keys);
        Ok(env.into_iter().collect())
    }

    pub fn ledger_delivery_environment<'a>(&'a self, environment_ref: &'a str) -> BoxFuture<'a, Result<BTreeMap<String, String>, String>> {
        Box::pin(async move {
            let records = self.ledger_delivery_environment.lock().await;
            let mut env = BTreeMap::new();
            if let Some(deliveries) = records.get(environment_ref) {
                for (name, delivery) in deliveries {
                    for (key, value) in delivery {
                        if env.insert(key.clone(), value.clone()).is_some() {
                            return Err(format!("multiple credentials supplied {key} for environment {environment_ref}, including {name}"));
                        }
                    }
                }
            }
            Ok(env)
        })
    }

    /// Rebuild refresh registrations for an already-running environment from
    /// its durable credential requirements. Reconciliation calls this on every
    /// pass, so a live registration makes the operation a no-op.
    #[cfg(test)]
    pub(crate) async fn adopt_github_app_deliveries(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<(), CredentialRefreshError> {
        self.adopt_github_app_deliveries_with_permissions(environment_ref, credential_refs, credential_scopes, &BTreeMap::new(), runner)
            .await
    }

    pub fn adopt_github_app_deliveries_with_permissions<'a>(
        &'a self,
        environment_ref: &'a str,
        credential_refs: &'a BTreeSet<String>,
        credential_scopes: &'a BTreeMap<String, BTreeSet<RepositoryKey>>,
        credential_permissions: &'a BTreeMap<String, BTreeMap<String, String>>,
        runner: Arc<dyn CommandRunner>,
    ) -> BoxFuture<'a, Result<(), CredentialRefreshError>> {
        Box::pin(async move {
            let mut github_app_refs = BTreeSet::new();
            for name in credential_refs {
                let spec = match self.spec(name).await {
                    Ok(spec) => spec,
                    Err(message) => return Err(self.record_adoption_failure(environment_ref, message).await),
                };
                if matches!(spec.consumer, CredentialConsumer::GithubApp { .. }) {
                    github_app_refs.insert(name.clone());
                }
            }
            let deliveries = self.github_app_deliveries.lock().await;
            let already_adopted = github_app_refs.iter().all(|name| deliveries.contains_key(&(environment_ref.to_string(), name.clone())));
            drop(deliveries);
            if github_app_refs.is_empty() || already_adopted {
                self.github_app_adoption_failures.lock().await.remove(environment_ref);
                return Ok(());
            }
            let github_app_scopes = credential_scopes
                .iter()
                .filter(|(name, _)| github_app_refs.contains(*name))
                .map(|(name, scopes)| (name.clone(), scopes.clone()))
                .collect();
            if let Err(message) = self
                .prepare_scoped_with_permissions(environment_ref, &github_app_refs, &github_app_scopes, credential_permissions, runner)
                .await
            {
                return Err(self.record_adoption_failure(environment_ref, message).await);
            }
            self.github_app_adoption_failures.lock().await.remove(environment_ref);
            Ok(())
        })
    }

    async fn record_adoption_failure(&self, environment_ref: &str, message: String) -> CredentialRefreshError {
        let mut failures = self.github_app_adoption_failures.lock().await;
        let failures = failures.entry(environment_ref.to_string()).or_default();
        *failures += 1;
        CredentialRefreshError {
            environment_ref: environment_ref.to_string(),
            credential_name: None,
            message,
            should_surface: *failures >= GITHUB_APP_REFRESH_FAILURE_THRESHOLD,
        }
    }

    /// Registry host actions resolve only declared material and own their private
    /// Docker configuration for this operation, including error/cancellation cleanup.
    pub fn image_registry_operation<'a>(
        &'a self,
        host: &'a str,
        action: flotilla_resources::HostImageAction,
        credential: &'a str,
        repository: &'a str,
        arguments: &'a [&'a str],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            use flotilla_resources::{CredentialGrant, Host, HostImageAction, ImageBuildCapacity};
            let verb = match action {
                HostImageAction::ImagePush => "push",
                HostImageAction::ImagePull => "pull",
            };
            if arguments.len() != 2
                || arguments[0] != verb
                || !image_registry_matches(arguments[1], repository.split('/').next().unwrap_or(""))
            {
                return Err("host image action requires its declared registry operation".into());
            }
            if !arguments[1].strip_prefix(repository).is_some_and(|suffix| suffix.starts_with('@') || suffix.starts_with(':')) {
                return Err("host image operation does not target the declared repository".into());
            }
            if action == HostImageAction::ImagePull
                && !arguments[1].rsplit_once('@').is_some_and(|(_, digest)| flotilla_resources::is_image_digest(digest))
            {
                return Err("host image pull requires a manifest digest".into());
            }
            let hosts = self.backend.including_replicas::<Host>(&self.namespace).list().await.map_err(|error| error.to_string())?;
            let declared_builder = hosts.items.iter().any(|source| {
                source.object.metadata.name == host
                    && matches!(source.object.spec.image_build_capacity, Some(ImageBuildCapacity::Builder { slots, .. }) if slots > 0)
            });
            let grants = self.backend.definitions::<CredentialGrant>(&self.namespace).list().await.map_err(|error| error.to_string())?;
            if !grants.iter().any(|grant| {
                grant.spec.credentials.contains(credential) && grant.spec.selector.matches_host_action(host, action, declared_builder)
            }) {
                return Err(format!("host {host} has no {action:?} grant for credential {credential}"));
            }
            let spec = self.spec(credential).await?;
            let CredentialConsumer::DockerRegistry { registry, username } = &spec.consumer else {
                return Err("image cache credential must use the docker-registry adapter".into());
            };
            if !image_registry_matches(repository, registry) {
                return Err("image cache credential does not match declared registry".into());
            }
            let material = self.resolve_for_adapter(credential, &spec, None, None).await?;
            let material = material.value.trim_end();
            validate_scalar_material(credential, "docker-registry", material)?;
            let root = self.state_dir.join("image-registry-operations");
            tokio::fs::create_dir_all(&root).await.map_err(|error| error.to_string())?;
            // TempDir removes the config on cancellation as well as every return path.
            let config = tempfile::Builder::new().prefix("operation-").tempdir_in(root).map_err(|error| error.to_string())?;
            tokio::fs::set_permissions(config.path(), std::fs::Permissions::from_mode(0o700)).await.map_err(|error| error.to_string())?;
            let directory = config.path().to_string_lossy();
            let operation = async {
                self.host_runner
                    .run_with_input(
                        "docker",
                        &["--config", &directory, "login", "--username", username, "--password-stdin", registry],
                        Path::new("/"),
                        &ChannelLabel::Default,
                        material.as_bytes(),
                    )
                    .await?;
                let mut args = vec!["--config", directory.as_ref()];
                args.extend_from_slice(arguments);
                self.host_runner.run("docker", &args, Path::new("/"), &ChannelLabel::Default).await
            };
            let output = tokio::time::timeout(std::time::Duration::from_secs(30 * 60), operation)
                .await
                .map_err(|_| "image registry operation timed out".to_string())?
                .map_err(|error| bounded_adapter_error(credential, "docker-registry", &error.replace(material, "[redacted]")))?;
            // Return only Docker's non-secret operation output, never login output.
            Ok(output.replace(material, "[redacted]"))
        })
    }

    pub fn prepare_registry_pull<'a>(
        &'a self,
        environment_ref: &'a str,
        credential_refs: &'a BTreeSet<String>,
        image: &'a str,
    ) -> BoxFuture<'a, Result<PreparedEnvironmentAuth, String>> {
        Box::pin(async move {
            let mut matching = Vec::new();
            for name in credential_refs {
                let spec = self.spec(name).await?;
                let CredentialConsumer::DockerRegistry { registry, .. } = &spec.consumer else {
                    continue;
                };
                if image_registry_matches(image, registry) {
                    matching.push((name.clone(), spec));
                }
            }
            let Some((name, spec)) = matching.pop() else {
                return Ok(PreparedEnvironmentAuth::NoRegistryCredential);
            };
            if !matching.is_empty() {
                return Err(bounded_adapter_error(&name, "docker-registry", "multiple granted credentials match the image registry"));
            }
            let CredentialConsumer::DockerRegistry { registry, username } = &spec.consumer else {
                unreachable!("matching credentials are docker-registry consumers");
            };
            // Keep creation, preflight, and registration in the same critical section
            // as sweeping: a fresh cache must not look orphaned before it is indexed.
            let _maintenance = self.registry_cache_maintenance.read().await;
            let previous = self.registry_configs.lock().await.remove(environment_ref);
            if let Some(previous) = previous {
                remove_registry_config(&previous)
                    .await
                    .map_err(|error| bounded_adapter_error(&name, "docker-registry", &format!("remove stale writable cache: {error}")))?;
            }
            let material = self.resolve_for_adapter(&name, &spec, None, None).await?;
            let material = material.value.trim_end();
            validate_scalar_material(&name, "docker-registry", material)?;
            let config_dir = self
                .state_dir
                .join("credential-runtime")
                .join(registry_environment_dir(environment_ref))
                .join(uuid::Uuid::new_v4().to_string());
            tokio::fs::create_dir_all(&config_dir)
                .await
                .map_err(|error| bounded_adapter_error(&name, "docker-registry", &format!("create cache directory: {error}")))?;
            tokio::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|error| bounded_adapter_error(&name, "docker-registry", &format!("protect cache directory: {error}")))?;
            let config = config_dir.to_string_lossy();
            let operation = async {
                self.host_runner
                    .run_with_input(
                        "docker",
                        &["--config", &config, "login", "--username", username, "--password-stdin", registry],
                        Path::new("/"),
                        &ChannelLabel::Default,
                        material.as_bytes(),
                    )
                    .await
                    .map_err(|error| format!("login preflight failed: {}", error.replace(material, "[redacted]")))?;
                self.host_runner
                    .run("docker", &["--config", &config, "pull", image], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|error| format!("pull preflight failed: {}", error.replace(material, "[redacted]")))
            }
            .await;
            if let Err(operation_error) = operation {
                let cleanup_result = remove_registry_config(&config_dir).await;
                let detail = match cleanup_result {
                    Ok(()) => operation_error,
                    Err(cleanup_error) => format!("{operation_error}; additionally failed to remove writable cache: {cleanup_error}"),
                };
                return Err(bounded_adapter_error(&name, "docker-registry", &detail));
            }
            self.registry_configs.lock().await.insert(environment_ref.to_string(), config_dir.clone());
            Ok(PreparedEnvironmentAuth::RegistryConfig { directory: DaemonHostPath::new(config_dir) })
        })
    }

    pub fn prepare_skill_source<'a>(
        &'a self,
        credential_name: &'a str,
        repository: &'a str,
        runner: &'a dyn CommandRunner,
    ) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let spec = self.spec(credential_name).await?;
            let repository_name = skill_source_repository(credential_name, &spec, repository)?;
            let (
                CredentialConsumer::GithubApp { installation_id, installation_repository, permissions, .. },
                CredentialSource::GithubApp { app_id_path, private_key_path },
            ) = (&spec.consumer, &spec.source)
            else {
                return Err(bounded_adapter_error(credential_name, "github-app", "skill source requires a GitHub App consumer and source"));
            };
            let installation_id = match (installation_id, installation_repository) {
                (Some(id), None) => *id,
                (None, Some(installation_repository)) => {
                    self.resolve_github_app_installation(installation_repository, app_id_path, private_key_path).await?
                }
                (Some(_), Some(_)) => return Err("declare either `installation_id` or `installation_repository`, not both".to_string()),
                (None, None) => return Err("declare either `installation_id` or `installation_repository`".to_string()),
            };
            let mut request = GithubAppMintRequest {
                installation_id,
                app_id_path: app_id_path.clone(),
                private_key_path: private_key_path.clone(),
                repositories: vec![repository_name],
                permissions: permissions.clone(),
            };
            let token = self
                .mint_github_app(&mut request, installation_repository.as_deref())
                .await
                .map_err(|error| bounded_adapter_error(credential_name, "github-app", &error.to_string()))?;
            if token.value.trim().is_empty() {
                return Err(bounded_adapter_error(credential_name, "github-app", "installation token response was empty"));
            }
            let paths = self.delivery_paths(runner).await?;
            prune_abandoned_skill_source_tokens(runner, &paths.base.join("skill-sources")).await?;
            // Each staging owns its token. A concurrent staging must not replace or
            // delete a token while another Git process is still reading it.
            let token_file =
                paths.base.join("skill-sources").join(safe_component(credential_name)).join(format!("token-{}", uuid::Uuid::new_v4()));
            write_github_app_token_file(runner, &token_file, token.value.trim_end()).await?;
            Ok(token_file)
        })
    }

    pub fn forget_environment<'a>(&'a self, environment_ref: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.work_deliveries.lock().await.remove(environment_ref);
            self.ledger_delivery_environment.lock().await.remove(environment_ref);
            self.capability_endpoints.lock().await.retain(|(environment, _), _| environment != environment_ref);
            self.capability_scopes.lock().await.retain(|(environment, _), _| environment != environment_ref);
            self.cleaned_delivery_environments.lock().await.remove(environment_ref);
            self.prepared.lock().await.retain(|(cached_environment, _)| cached_environment != environment_ref);
            self.materials.lock().await.retain(|(cached_environment, _), _| cached_environment != environment_ref);
            self.git_config_fragments.lock().await.remove(environment_ref);
            self.github_app_deliveries.lock().await.retain(|(cached_environment, _), _| cached_environment != environment_ref);
            self.github_app_adoption_failures.lock().await.remove(environment_ref);
            let config_dir = self.registry_configs.lock().await.remove(environment_ref);
            if let Some(config_dir) = config_dir {
                remove_registry_config(&config_dir).await.map_err(|error| format!("remove Docker credential cache: {error}"))?;
            }
            Ok(())
        })
    }

    /// Remove Docker login caches left by environments which no longer have
    /// either a resource record or a running backing. Legacy flat directories
    /// have no environment identity, so they are removed only when no live
    /// environment or backing could own one.
    pub fn sweep_orphaned_registry_configs<'a>(
        &'a self,
        live_environments: &'a BTreeSet<String>,
        running_backings: &'a BTreeSet<String>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Clients may begin preparing caches after the live/backing snapshots.
            // Holding this through deletion serializes sweeping with preparation,
            // including the interval before Docker preflight registers its cache.
            let _maintenance = self.registry_cache_maintenance.write().await;
            let root = self.state_dir.join("credential-runtime");
            let mut entries = match tokio::fs::read_dir(&root).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(format!("list Docker credential caches {}: {error}", root.display())),
            };
            let protected =
                live_environments.iter().chain(running_backings).map(|name| registry_environment_dir(name)).collect::<BTreeSet<_>>();
            let active_paths = self.registry_configs.lock().await.values().cloned().collect::<BTreeSet<_>>();
            while let Some(entry) = entries.next_entry().await.map_err(|error| format!("list Docker credential cache: {error}"))? {
                let kind = entry.file_type().await.map_err(|error| format!("inspect Docker credential cache: {error}"))?;
                if !kind.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let owned = name
                    .strip_prefix("env-")
                    .is_some_and(|hex| !hex.is_empty() && hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
                let legacy =
                    name.len() > 37 && name.as_bytes()[name.len() - 37] == b'-' && uuid::Uuid::parse_str(&name[name.len() - 36..]).is_ok();
                if owned && !protected.contains(&name) && !active_paths.iter().any(|path| path.starts_with(entry.path())) {
                    remove_registry_config(&entry.path())
                        .await
                        .map_err(|error| format!("remove orphaned Docker credential cache {}: {error}", entry.path().display()))?;
                    tracing::info!(path = %entry.path().display(), "removed orphaned Docker credential cache");
                } else if legacy && live_environments.is_empty() && running_backings.is_empty() && !active_paths.contains(&entry.path()) {
                    remove_registry_config(&entry.path())
                        .await
                        .map_err(|error| format!("remove legacy Docker credential cache {}: {error}", entry.path().display()))?;
                    tracing::info!(path = %entry.path().display(), "removed legacy orphaned Docker credential cache");
                }
            }
            Ok(())
        })
    }

    pub fn record_capability_endpoints<'a>(
        &'a self,
        environment: &'a str,
        session: &'a str,
        env: &'a [(String, String)],
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let endpoints = env.iter().filter_map(|(key, value)| flotilla_core::crew_capabilities::endpoint_for_env(key, value)).collect();
            self.capability_endpoints.lock().await.insert((environment.to_string(), session.to_string()), endpoints);
        })
    }

    pub fn tracked_work_deliveries(&self) -> BoxFuture<'_, BTreeMap<String, BTreeSet<String>>> {
        Box::pin(async move { self.work_deliveries.lock().await.clone() })
    }

    /// Reconcile the files and cached material for one work environment. The
    /// caller supplies every grant for that environment, including settled
    /// work, so a restarted daemon can remove files it did not prepare itself.
    #[cfg(test)]
    pub(crate) async fn reconcile_work_delivery(
        &self,
        environment_ref: &str,
        granted: &BTreeSet<String>,
        running: &BTreeSet<String>,
        scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<(), String> {
        self.reconcile_work_delivery_with_permissions(environment_ref, granted, running, scopes, &BTreeMap::new(), runner).await
    }

    pub fn reconcile_work_delivery_with_permissions<'a>(
        &'a self,
        environment_ref: &'a str,
        granted: &'a BTreeSet<String>,
        running: &'a BTreeSet<String>,
        scopes: &'a BTreeMap<String, BTreeSet<RepositoryKey>>,
        permissions: &'a BTreeMap<String, BTreeMap<String, String>>,
        runner: Arc<dyn CommandRunner>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut delivered = BTreeSet::new();
            for name in granted {
                if !matches!(self.spec(name).await?.consumer, CredentialConsumer::DockerRegistry { .. }) {
                    delivered.insert(name.clone());
                }
            }
            if delivered.is_empty() {
                self.work_deliveries.lock().await.remove(environment_ref);
                self.ledger_delivery_environment.lock().await.remove(environment_ref);
                return Ok(());
            }
            let paths = self.delivery_paths(&*runner).await?;
            for name in delivered.difference(running) {
                let directory = paths.credential_dir(name);
                runner
                    .run("rm", &["-rf", "--", &directory.to_string_lossy()], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|error| format!("revoke credential `{name}`: {error}"))?;
                if let Some(record) = self.ledger_delivery_environment.lock().await.get_mut(environment_ref) {
                    record.remove(name);
                }
                let key = (environment_ref.to_string(), name.clone());
                self.prepared.lock().await.remove(&key);
                self.materials.lock().await.remove(&key);
                self.github_app_deliveries.lock().await.remove(&key);
                self.git_config_fragments.lock().await.entry(environment_ref.to_string()).and_modify(|fragments| {
                    fragments.remove(name);
                });
            }
            let missing = {
                let prepared = self.prepared.lock().await;
                running
                    .iter()
                    .filter(|name| delivered.contains(*name) && !prepared.contains(&(environment_ref.to_string(), (*name).clone())))
                    .cloned()
                    .collect::<BTreeSet<_>>()
            };
            if !missing.is_empty() {
                let missing_scopes =
                    scopes.iter().filter(|(name, _)| missing.contains(*name)).map(|(name, scope)| (name.clone(), scope.clone())).collect();
                self.prepare_scoped_with_permissions(environment_ref, &missing, &missing_scopes, permissions, runner.clone()).await?;
            }
            let fragments = self.git_config_fragments.lock().await.get(environment_ref).cloned().unwrap_or_default();
            if fragments.is_empty() {
                runner
                    .run("rm", &["-f", "--", &paths.git_config.to_string_lossy()], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|error| format!("remove settled Git credential configuration: {error}"))?;
            } else {
                let gitconfig = compose(TargetId::GitConfig, crew_gitconfig_fragments().into_iter().chain(fragments.values().cloned()))
                    .map_err(|error| format!("compose active Git credential configuration: {error}"))?;
                runner
                    .write_file(&paths.git_config, &gitconfig.contents)
                    .await
                    .map_err(|error| format!("stage active Git credential configuration: {error}"))?;
            }
            let mut tracked = self.work_deliveries.lock().await;
            let running_delivered = running.intersection(&delivered).cloned().collect::<BTreeSet<_>>();
            if running_delivered.is_empty() {
                tracked.remove(environment_ref);
            } else {
                tracked.insert(environment_ref.to_string(), running_delivered);
            }
            Ok(())
        })
    }

    /// Re-mint and atomically replace GitHub App files that are approaching
    /// expiry. The daemon calls this from its host-side periodic loop; vessels
    /// receive only the resulting file and never the App signing material.
    pub fn refresh_due_github_app_tokens(&self) -> BoxFuture<'_, Vec<CredentialRefreshError>> {
        Box::pin(async move {
            let now = self.clock.now();
            let deliveries =
                self.github_app_deliveries.lock().await.iter().map(|(key, delivery)| (key.clone(), delivery.clone())).collect::<Vec<_>>();
            let mut errors = Vec::new();
            for (key, delivery) in deliveries {
                let mut request = delivery.request.clone();
                if let Some(scope) = &delivery.scope {
                    let repositories = match self.resolve_github_app_scope(scope).await {
                        Ok(repositories) => repositories,
                        Err(error) => {
                            let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                            errors.push(CredentialRefreshError {
                                environment_ref: key.0.clone(),
                                credential_name: Some(key.1.clone()),
                                message: self.refresh_failure_message(&key.1, delivery.expires_at, &error),
                                should_surface,
                            });
                            continue;
                        }
                    };
                    request.repositories = repositories;
                    request.permissions = scope.permissions.clone().or_else(|| delivery.request.permissions.clone());
                }
                let refresh_at = github_app_refresh_at(delivery.issued_at, delivery.expires_at);
                if now < refresh_at
                    && request.repositories == delivery.request.repositories
                    && request.permissions == delivery.request.permissions
                {
                    continue;
                }
                if request.repositories == delivery.request.repositories
                    && request.permissions == delivery.request.permissions
                    && now + GITHUB_APP_REFRESH_MARGIN < delivery.expires_at
                    && delivery.next_refresh_attempt_at.is_some_and(|next| now < next)
                {
                    continue;
                }
                let token = match self.mint_github_app(&mut request, delivery.installation_repository.as_deref()).await {
                    Ok(token) => token,
                    Err(error) => {
                        let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                        errors.push(CredentialRefreshError {
                            environment_ref: key.0.clone(),
                            credential_name: Some(key.1.clone()),
                            message: self.refresh_failure_message(&key.1, delivery.expires_at, &error),
                            should_surface,
                        });
                        continue;
                    }
                };
                if let Err(error) = validate_scalar_material(&key.1, "github-app", token.value.trim_end()) {
                    let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                    errors.push(CredentialRefreshError {
                        environment_ref: key.0.clone(),
                        credential_name: Some(key.1.clone()),
                        message: self.refresh_failure_message(&key.1, delivery.expires_at, &error),
                        should_surface,
                    });
                    continue;
                }
                let _delivery_guard = self.github_app_delivery_lock(&key).await.lock_owned().await;
                let current = {
                    self.github_app_deliveries.lock().await.get(&key).filter(|current| current.generation == delivery.generation).cloned()
                };
                let Some(current) = current else {
                    continue;
                };
                if let Err(error) = replace_github_app_token_file(&*current.runner, &current.token_file, token.value.trim_end()).await {
                    let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                    errors.push(CredentialRefreshError {
                        environment_ref: key.0.clone(),
                        credential_name: Some(key.1.clone()),
                        message: self.refresh_failure_message(&key.1, current.expires_at, &error),
                        should_surface,
                    });
                    continue;
                }
                if self.clock.now() + GITHUB_APP_REFRESH_MARGIN >= current.expires_at {
                    errors.push(CredentialRefreshError {
                        environment_ref: key.0.clone(),
                        credential_name: Some(key.1.clone()),
                        message: self.refresh_failure_message(&key.1, current.expires_at, "refresh started inside the expiry margin"),
                        should_surface: true,
                    });
                }
                let mut deliveries = self.github_app_deliveries.lock().await;
                if let Some(current) = deliveries.get_mut(&key).filter(|current| current.generation == delivery.generation) {
                    current.expires_at = token.expires_at;
                    current.issued_at = self.clock.now();
                    current.refresh_failures = 0;
                    current.next_refresh_attempt_at = None;
                    // Successful remints accept the explicit request when the response omits it;
                    // no request and no response still means unknown.
                    current.effective_permissions = token.permissions.or_else(|| request.permissions.clone());
                    current.request = request;
                }
            }
            errors
        })
    }

    fn refresh_failure_message(&self, name: &str, expires_at: DateTime<Utc>, error: &str) -> String {
        let remaining = expires_at - self.clock.now();
        let expired = remaining <= Duration::zero();
        let seconds = if expired { -remaining.num_seconds() } else { remaining.num_seconds() };
        let minutes = seconds.saturating_add(59) / 60;
        let unit = if minutes == 1 { "minute" } else { "minutes" };
        let expiry = if expired { format!("expired {minutes} {unit} ago") } else { format!("expires in {minutes} {unit}") };
        let prefix = format!("credential `{name}` adapter `github-app`: ");
        let detail = error.strip_prefix(&prefix).unwrap_or(error);
        format!("{}; {expiry}", bounded_adapter_error(name, "github-app", detail))
    }

    pub fn set_github_app_scopes<'a>(
        &'a self,
        environment_ref: &'a str,
        scopes: &'a BTreeMap<String, GithubAppScope>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut deliveries = self.github_app_deliveries.lock().await;
            for (name, scope) in scopes {
                if let Some(delivery) = deliveries.get_mut(&(environment_ref.to_string(), name.clone())) {
                    delivery.scope = Some(scope.clone());
                }
            }
        })
    }

    async fn resolve_github_app_scope(&self, scope: &GithubAppScope) -> Result<Vec<String>, String> {
        let mut repositories = scope.fixed_repositories.clone();
        let projects = self.backend.including_replicas::<Project>(&self.namespace);
        for name in &scope.projects {
            let project = projects.get(name).await.map_err(|error| format!("project `{name}` unavailable: {error}"))?;
            repositories.extend(project.object.spec.repositories.iter().map(|repository| repository.repo.clone()));
        }
        if repositories.is_empty() {
            return Err("grant resolved to an empty repository scope".to_string());
        }
        self.github_repository_names(&repositories).await
    }

    async fn record_refresh_failure(&self, key: &(String, String), generation: uuid::Uuid) -> bool {
        let mut deliveries = self.github_app_deliveries.lock().await;
        let Some(current) = deliveries.get_mut(key).filter(|current| current.generation == generation) else {
            return false;
        };
        current.record_refresh_failure(self.clock.now())
    }

    async fn spec(&self, name: &str) -> Result<CredentialSpecSpec, String> {
        self.backend.clone().definitions::<CredentialSpec>(&self.namespace).get(name).await.map(|object| object.spec).map_err(|error| {
            match error {
                ResourceError::NotFound { .. } => format!("credential `{name}` declaration not found"),
                error => format!("credential `{name}` declaration unavailable: {error}"),
            }
        })
    }

    async fn forgejo_server_url(&self, forge_ref: &str) -> Result<String, String> {
        let forge = self
            .backend
            .definitions::<Forge>(&self.namespace)
            .get(forge_ref)
            .await
            .map_err(|error| format!("Forge `{forge_ref}` unavailable: {error}"))?;
        if forge.spec.kind != ForgeKind::Forgejo {
            return Err(format!("Forge `{forge_ref}` is not a Forgejo forge"));
        }
        Ok(forge.spec.https_url)
    }

    async fn source_is_available(&self, spec: &CredentialSpecSpec) -> bool {
        if spec.placement.binaries.iter().any(|binary| self.host_bag.find_binary(binary).is_none()) {
            return false;
        }
        match &spec.source {
            CredentialSource::File { path } => {
                tokio::fs::metadata(self.expand_path(path)).await.is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
            }
            CredentialSource::Env { name } => self.env.get(name).is_some_and(|value| !value.trim().is_empty()),
            CredentialSource::IssueCommand { command, .. } => self.host_bag.find_binary(command).is_some(),
            CredentialSource::GithubApp { app_id_path, private_key_path } => {
                let app_id = tokio::fs::metadata(self.expand_path(app_id_path)).await;
                let private_key = tokio::fs::metadata(self.expand_path(private_key_path)).await;
                [app_id, private_key].into_iter().all(|metadata| metadata.is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0))
            }
        }
    }

    async fn resolve(&self, name: &str, spec: &CredentialSpecSpec) -> Result<String, String> {
        let material = match &spec.source {
            CredentialSource::File { path } => {
                tokio::fs::read_to_string(self.expand_path(path)).await.map_err(|error| format!("read host-local source: {error}"))?
            }
            CredentialSource::Env { name: env_name } => {
                self.env.get(env_name).ok_or_else(|| format!("host-local environment variable `{env_name}` is not set"))?
            }
            CredentialSource::IssueCommand { command, args } => {
                let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                self.host_runner
                    .run(command, &args, Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|_| "issue command failed".to_string())?
            }
            CredentialSource::GithubApp { .. } => return Err("GitHub App material must be resolved by the github-app adapter".to_string()),
        };
        if material.trim().is_empty() {
            return Err(format!("credential `{name}` source produced empty material"));
        }
        Ok(material)
    }

    async fn resolve_for_adapter(
        &self,
        name: &str,
        spec: &CredentialSpecSpec,
        repository_scope: Option<&BTreeSet<RepositoryKey>>,
        granted_permissions: Option<&BTreeMap<String, String>>,
    ) -> Result<ResolvedMaterial, String> {
        let result = match (&spec.consumer, &spec.source) {
            (
                CredentialConsumer::GithubApp { installation_id, installation_repository, permissions, .. },
                CredentialSource::GithubApp { app_id_path, private_key_path },
            ) => {
                if spec.lifecycle != CredentialLifecycle::Refreshable {
                    return Err(bounded_adapter_error(
                        name,
                        spec.consumer.adapter_name(),
                        "GitHub App credentials must use the refreshable lifecycle",
                    ));
                }
                let repository_scope = repository_scope
                    .filter(|scope| !scope.is_empty())
                    .ok_or_else(|| "grant resolved to an empty repository scope".to_string())?;
                let mut repositories = self.github_repository_names(repository_scope).await?;
                repositories.sort();
                repositories.dedup();
                let installation_id = match (installation_id, installation_repository) {
                    (Some(id), None) => *id,
                    (None, Some(repository)) => self.resolve_github_app_installation(repository, app_id_path, private_key_path).await?,
                    (Some(_), Some(_)) => return Err("declare either `installation_id` or `installation_repository`, not both".to_string()),
                    (None, None) => return Err("declare either `installation_id` or `installation_repository`".to_string()),
                };
                let mut request = GithubAppMintRequest {
                    installation_id,
                    app_id_path: app_id_path.clone(),
                    private_key_path: private_key_path.clone(),
                    repositories,
                    permissions: capped_github_app_permissions(granted_permissions, permissions.as_ref())?,
                };
                self.mint_github_app(&mut request, installation_repository.as_deref())
                    .await
                    .map(|token| ResolvedMaterial { value: token.value, github_app: Some((request, token.expires_at, token.permissions)) })
            }
            (CredentialConsumer::GithubApp { .. }, _) => Err("github-app consumer requires a github-app source".to_string()),
            (_, CredentialSource::GithubApp { .. }) => Err("github-app source requires a github-app consumer".to_string()),
            _ => self.resolve(name, spec).await.map(|value| ResolvedMaterial { value, github_app: None }),
        };
        result.map_err(|error| bounded_adapter_error(name, spec.consumer.adapter_name(), &error))
    }

    async fn resolve_github_app_installation(&self, repository: &str, app_id_path: &str, private_key_path: &str) -> Result<u64, String> {
        let request = GithubAppInstallationRequest {
            repository: repository.to_string(),
            app_id_path: app_id_path.to_string(),
            private_key_path: private_key_path.to_string(),
        };
        if let Some(id) = self.github_app_installations.lock().await.get(&request).copied() {
            return Ok(id);
        }
        let id = self.github_app_minter.resolve_installation(&request).await?;
        self.github_app_installations.lock().await.insert(request, id);
        Ok(id)
    }

    async fn mint_github_app(
        &self,
        request: &mut GithubAppMintRequest,
        installation_repository: Option<&str>,
    ) -> Result<GithubAppToken, String> {
        match self.mint_github_app_with_retry(request).await {
            Ok(token) => Ok(token),
            Err(GithubAppMintError::InstallationNotFound(_)) if installation_repository.is_some() => {
                let repository = installation_repository.expect("guarded by is_some");
                self.github_app_installations.lock().await.remove(&GithubAppInstallationRequest {
                    repository: repository.to_string(),
                    app_id_path: request.app_id_path.clone(),
                    private_key_path: request.private_key_path.clone(),
                });
                request.installation_id =
                    self.resolve_github_app_installation(repository, &request.app_id_path, &request.private_key_path).await?;
                self.mint_github_app_with_retry(request).await.map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn mint_github_app_with_retry(&self, request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
        for attempt in 1..=3 {
            match self.github_app_minter.mint(request).await {
                Err(GithubAppMintError::Transient(_)) if attempt < 3 => {
                    tokio::time::sleep(std::time::Duration::from_secs(attempt)).await;
                }
                result => return result,
            }
        }
        unreachable!("the last mint attempt returns")
    }

    async fn github_repository_names(&self, repository_scope: &BTreeSet<RepositoryKey>) -> Result<Vec<String>, String> {
        let forges =
            self.backend.definitions::<Forge>(&self.namespace).list().await.map_err(|error| format!("list Forge definitions: {error}"))?;
        let repositories = self
            .backend
            .including_replicas::<Repository>(&self.namespace)
            .list()
            .await
            .map_err(|error| format!("list repository identities: {error}"))?;
        let mut names = Vec::new();
        for key in repository_scope {
            let repository = repositories
                .items
                .iter()
                .find(|repository| repository.object.spec.key() == *key)
                .ok_or_else(|| format!("repository scope references missing repository `{key}`"))?;
            let forge = repository.object.spec.forge().ok_or_else(|| format!("repository `{key}` has no forge identity"))?;
            let service = Url::parse(&forge.service_url).map_err(|error| format!("repository `{key}` has invalid forge URL: {error}"))?;
            let matching = match repository.object.spec.identity() {
                RepositoryIdentity::Forge { forge_ref, .. } => {
                    forges.iter().filter(|candidate| &candidate.spec.forge_id == forge_ref).collect::<Vec<_>>()
                }
                _ => {
                    let remote = repository.object.spec.live_remote().ok_or_else(|| format!("repository `{key}` has no remote"))?;
                    let mut matching = Vec::new();
                    for candidate in &forges {
                        if candidate.spec.repository_path(remote)?.is_some() {
                            matching.push(candidate);
                        }
                    }
                    matching
                }
            };
            if matching.len() > 1 {
                return Err(format!("repository `{key}` matches multiple Forge definitions"));
            }
            let is_github = matching
                .first()
                .map_or_else(|| service.host_str() == Some("github.com"), |candidate| candidate.spec.kind == ForgeKind::Github);
            if !is_github {
                continue;
            }
            let (_, name) = forge
                .repository
                .rsplit_once('/')
                .ok_or_else(|| format!("repository `{key}` has invalid GitHub identity `{}`", forge.repository))?;
            names.push(name.to_string());
        }
        if names.is_empty() {
            return Err("grant resolved to an empty GitHub repository scope".to_string());
        }
        if names.len() > 500 {
            return Err("GitHub App repository scope exceeds the 500-repository API limit".to_string());
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn expand_path(&self, path: &str) -> PathBuf {
        expand_path(&*self.env, path)
    }

    // The preflight scratch dir must keep the `claude -p` probe blind to
    // ambient authentication (`apiKeyHelper`, stored logins) — an empty
    // per-credential dir. The daemon runs as an unprivileged user with no
    // systemd `RuntimeDirectory=`, so absolute `/run` is unwritable; derive
    // the dir from a daemon-owned writable base instead: the user runtime dir
    // when present and writable, the daemon state dir otherwise (#1498,
    // #1508). The runner owns the choice because it may target a different
    // filesystem world, such as a provisioned container. Resolve on every
    // preflight because logind can remove an inherited runtime dir after
    // daemon startup.
    async fn claude_preflight_config_dir(&self, credential_name: &str, runner: &dyn CommandRunner) -> Result<String, String> {
        let runtime_dir =
            self.env.get("XDG_RUNTIME_DIR").map(|value| PathBuf::from(value.trim())).filter(|value| !value.as_os_str().is_empty());
        let base = runner.writable_scratch_base(runtime_dir.as_deref(), &self.state_dir).await?;
        Ok(base.join("credentials").join(safe_component(credential_name)).join("claude-preflight").to_string_lossy().into_owned())
    }

    async fn delivery_paths(&self, runner: &dyn CommandRunner) -> Result<CredentialDeliveryPaths, String> {
        let runtime_dir =
            self.env.get("XDG_RUNTIME_DIR").map(|value| PathBuf::from(value.trim())).filter(|value| !value.as_os_str().is_empty());
        let base = runner.writable_config_base(runtime_dir.as_deref(), &self.state_dir).await?;
        Ok(CredentialDeliveryPaths::new(base))
    }

    async fn prepare_adapter(
        &self,
        name: &str,
        spec: &CredentialSpecSpec,
        material: &str,
        runner: Arc<dyn CommandRunner>,
        already_prepared: bool,
        delivery_paths: Option<&CredentialDeliveryPaths>,
    ) -> Result<AdapterDelivery, String> {
        let mut env = BTreeMap::new();
        let mut git_credential = None;
        match &spec.consumer {
            CredentialConsumer::Gh => {
                if !already_prepared {
                    runner
                        .run_with_input(
                            "sh",
                            &["-c", "IFS= read -r token; GH_TOKEN=\"$token\" gh api user --silent"],
                            Path::new("/"),
                            &ChannelLabel::Default,
                            material.as_bytes(),
                        )
                        .await
                        .map_err(|error| format!("authentication preflight failed: {error}"))?;
                }
                env.insert("GH_TOKEN".to_string(), material.to_string());
                git_credential = Some(GitCredentialContribution {
                    fragment: git_credential_fragment(name, "gh", "https://github.com", "!gh auth git-credential"),
                    preflight: (!already_prepared).then_some(GitCredentialPreflight::Gh),
                });
            }
            CredentialConsumer::GithubApp { .. } => {
                let delivery_paths = delivery_paths.expect("GitHub App adapter resolves delivery paths");
                let credential_dir = delivery_paths.credential_dir(name);
                let token_file = github_app_token_file(delivery_paths, name);
                write_github_app_token_file(&*runner, &token_file, material).await?;
                let token_file = token_file.to_string_lossy().into_owned();
                let gh_path = runner
                    .run("sh", &["-c", "command -v gh"], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|error| format!("locate gh binary: {error}"))?;
                let gh_path = gh_path.trim();
                if gh_path.is_empty() {
                    return Err("locate gh binary: command returned an empty path".to_string());
                }
                let path = runner
                    .run("sh", &["-c", "printf '%s' \"$PATH\""], Path::new("/"), &ChannelLabel::Default)
                    .await
                    .map_err(|error| format!("read executable search path: {error}"))?;
                let gh_wrapper = credential_dir.join("gh");
                let gh_wrapper_contents = format!(
                    "#!/bin/sh\nGH_TOKEN=$(cat \"$GITHUB_TOKEN_FILE\") || exit $?\nexport GH_TOKEN\nexec {} \"$@\"\n",
                    shell_single_quote(gh_path)
                );
                write_executable(&*runner, &gh_wrapper, &gh_wrapper_contents, "gh token-file wrapper").await?;
                let git_helper = credential_dir.join("git-credential-github-app");
                let git_helper_contents = "#!/bin/sh\n[ \"$1\" = get ] || exit 0\nprotocol=\nhost=\nwhile IFS='=' read -r key value; do\n  case \"$key\" in\n    protocol) protocol=$value ;;\n    host) host=$value ;;\n  esac\ndone\n[ \"$protocol\" = https ] || exit 0\n[ \"$host\" = github.com ] || exit 0\nprintf 'username=x-access-token\\n'\nprintf 'password='\ncat \"$GITHUB_TOKEN_FILE\"\nprintf '\\n'\n";
                write_executable(&*runner, &git_helper, git_helper_contents, "GitHub App Git credential helper").await?;
                let gh_wrapper_path = gh_wrapper.to_string_lossy().into_owned();
                runner
                    .run(
                        "sh",
                        &[
                            "-c",
                            "unset GH_TOKEN GITHUB_TOKEN; GITHUB_TOKEN_FILE=\"$1\" \"$2\" api installation/repositories --silent",
                            "flotilla-github-app-preflight",
                            &token_file,
                            &gh_wrapper_path,
                        ],
                        Path::new("/"),
                        &ChannelLabel::Default,
                    )
                    .await
                    .map_err(|error| format!("installation authentication preflight failed: {error}"))?;
                env.insert("GITHUB_TOKEN_FILE".to_string(), token_file.clone());
                if let Some(actor_login) = spec.consumer.github_actor_login() {
                    env.insert("PR_SHEPHERD_AS".to_string(), actor_login.to_string());
                }
                env.insert("PATH".to_string(), format!("{}:{path}", credential_dir.to_string_lossy()));
                git_credential = Some(GitCredentialContribution {
                    fragment: git_credential_fragment(
                        name,
                        "github-app",
                        "https://github.com",
                        format!("!{}", git_helper.to_string_lossy()),
                    ),
                    preflight: Some(GitCredentialPreflight::GithubApp { token_file }),
                });
            }
            CredentialConsumer::Forgejo { forge_ref, username } => {
                let delivery_paths = delivery_paths.expect("Forgejo adapter resolves delivery paths");
                let forgejo_url = self.forgejo_server_url(forge_ref).await?;
                let server_url = forgejo_url.trim_end_matches('/');
                let parsed_url = Url::parse(server_url).map_err(|error| format!("invalid Forgejo server URL: {error}"))?;
                if parsed_url.scheme() != "https" {
                    return Err("Forgejo server URL must use HTTPS".to_string());
                }
                let host = parsed_url.host_str().ok_or_else(|| "Forgejo server URL has no host".to_string())?;
                let credential_url = match parsed_url.port() {
                    Some(port) => format!("https://{host}:{port}"),
                    None => format!("https://{host}"),
                };
                let credential_dir = delivery_paths.credential_dir(name);
                let path = credential_dir.join("token").to_string_lossy().into_owned();
                let helper_path = credential_dir.join("git-credential-forgejo").to_string_lossy().into_owned();
                if !already_prepared {
                    runner
                        .write_file_with_mode(Path::new(&path), material, 0o600)
                        .await
                        .map_err(|error| format!("write token file: {error}"))?;
                    let helper = format!(
                        "#!/bin/sh\n[ \"$1\" = get ] || exit 0\nprotocol=\nhost=\nwhile IFS='=' read -r key value; do\n  case \"$key\" in\n    protocol) protocol=$value ;;\n    host) host=$value ;;\n  esac\ndone\n[ \"$protocol\" = https ] || exit 0\n[ \"$host\" = {host}{} ] || exit 0\nprintf 'username=%s\\n' \"$FORGEJO_USERNAME\"\nprintf 'password='\ncat \"$FORGEJO_TOKEN_FILE\"\nprintf '\\n'\n",
                        parsed_url.port().map(|port| format!(":{port}")).unwrap_or_default()
                    );
                    runner
                        .write_file_with_mode(Path::new(&helper_path), &helper, 0o700)
                        .await
                        .map_err(|error| format!("write Git credential helper: {error}"))?;
                    let url = format!("{server_url}/api/v1/user");
                    let curl_config = format!(
                        "silent\nshow-error\nfail\nheader = \"Authorization: token {}\"\nurl = \"{}\"\n",
                        sanitize_curl_config(material),
                        sanitize_curl_config(&url)
                    );
                    runner
                        .run_with_input("curl", &["--config", "-"], Path::new("/"), &ChannelLabel::Default, curl_config.as_bytes())
                        .await
                        .map_err(|error| format!("authentication preflight failed: {error}"))?;
                }
                env.insert("FORGEJO_TOKEN_FILE".to_string(), path.clone());
                env.insert("FORGEJO_SERVER_URL".to_string(), server_url.to_string());
                env.insert("FORGEJO_API_URL".to_string(), format!("{server_url}/api/v1"));
                env.insert("FORGEJO_USERNAME".to_string(), username.to_string());
                git_credential = Some(GitCredentialContribution {
                    fragment: git_credential_fragment(name, "forgejo", credential_url, format!("!{helper_path}")),
                    preflight: (!already_prepared).then(|| GitCredentialPreflight::Forgejo {
                        host: match parsed_url.port() {
                            Some(port) => format!("{host}:{port}"),
                            None => host.to_string(),
                        },
                        token_file: path,
                        username: username.to_string(),
                    }),
                });
            }
            CredentialConsumer::GitHttpToken { host, username } => {
                let delivery_paths = delivery_paths.expect("git-http-token adapter resolves delivery paths");
                let host = canonical_git_http_host(host)?;
                let credential_url = format!("https://{host}");
                if username.is_empty() || username.contains(['\n', '\r']) {
                    return Err("Git HTTPS username must be non-empty and single-line".to_string());
                }
                let credential_dir = delivery_paths.credential_dir(name);
                let path = credential_dir.join("token").to_string_lossy().into_owned();
                let helper_path = credential_dir.join("git-credential-http-token").to_string_lossy().into_owned();
                if !already_prepared {
                    runner
                        .write_file_with_mode(Path::new(&path), material, 0o600)
                        .await
                        .map_err(|error| format!("write token file: {error}"))?;
                    let helper = format!(
                        "#!/bin/sh\n[ \"$1\" = get ] || exit 0\nprotocol=\nrequest_host=\nwhile IFS='=' read -r key value; do\n  case \"$key\" in\n    protocol) protocol=$value ;;\n    host) request_host=$value ;;\n  esac\ndone\n[ \"$protocol\" = https ] || exit 0\n[ \"$request_host\" = {} ] || exit 0\nprintf 'username=%s\\n' {}\nprintf 'password='\ncat {}\nprintf '\\n'\n",
                        shell_single_quote(&host),
                        shell_single_quote(username),
                        shell_single_quote(&path),
                    );
                    runner
                        .write_file_with_mode(Path::new(&helper_path), &helper, 0o700)
                        .await
                        .map_err(|error| format!("write Git credential helper: {error}"))?;
                }
                git_credential = Some(GitCredentialContribution {
                    fragment: git_credential_fragment(name, "git-http-token", credential_url, format!("!{helper_path}")),
                    preflight: (!already_prepared).then_some(GitCredentialPreflight::GitHttpToken { host }),
                });
            }
            CredentialConsumer::Claude => {
                if !runner.exists("claude", &["--version"]).await {
                    return Err("consumer binary is unavailable".to_string());
                }
                if !already_prepared {
                    api_key_preflight(
                        &*runner,
                        "https://api.anthropic.com/v1/models?limit=1",
                        &[("x-api-key", material), ("anthropic-version", "2023-06-01")],
                    )
                    .await?;
                }
                env.insert("ANTHROPIC_API_KEY".to_string(), material.to_string());
            }
            // Subscription OAuth material (a `claude setup-token` long-lived
            // token) is delivered per-process through `CLAUDE_CODE_OAUTH_TOKEN`
            // — no login transformation exists or is needed, and one mint
            // serves N crews (ADR 0022 amendment). The credential-specific
            // preflight config directory isolates its probe from ambient
            // authentication. Delivery also offers a credential-specific
            // mutable config path for contained crews; the trusted Claude
            // adapter removes that override so host settings, skills, and MCP
            // configuration remain ambient. See
            // docs/research/2026-07-28-multi-crew-agent-config-seeding.md.
            CredentialConsumer::ClaudeOauth { .. } => {
                let delivery_paths = delivery_paths.expect("Claude OAuth adapter resolves delivery paths");
                if !runner.exists("claude", &["--version"]).await {
                    return Err("consumer binary is unavailable".to_string());
                }
                if !already_prepared {
                    let config_dir = self.claude_preflight_config_dir(name, &*runner).await?;
                    runner
                        .run("mkdir", &["-p", &config_dir], Path::new("/"), &ChannelLabel::Default)
                        .await
                        .map_err(|error| format!("create writable config directory: {error}"))?;
                    // Preflight: a trivial `claude -p` request under the token.
                    // There is no documented headless status command and no
                    // documented HTTP endpoint that accepts subscription OAuth
                    // tokens (the `/v1/models` x-api-key probe used for the
                    // API-key adapter takes API keys, not OAuth bearers), so
                    // the cheapest reliable probe is the CLI itself on exactly
                    // the path the crew will use — a dead token fails the
                    // request loudly in print mode. `ANTHROPIC_AUTH_TOKEN` and
                    // `ANTHROPIC_API_KEY` are unset and the empty
                    // per-credential config dir excludes any ambient
                    // `apiKeyHelper`, since each of those outranks
                    // `CLAUDE_CODE_OAUTH_TOKEN` and would mask a dead token
                    // (the stored ambient login ranks below it, so it cannot).
                    let probe = runner
                        .run_with_input(
                            "sh",
                            &[
                                "-c",
                                "IFS= read -r token; unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN; \
                                 output=$(CLAUDE_CODE_OAUTH_TOKEN=\"$token\" CLAUDE_CONFIG_DIR=\"$1\" claude -p ok 2>&1) || \
                                 { status=$?; printf '%s\\n' \"$output\" >&2; exit \"$status\"; }; printf '%s' \"$output\"",
                                "flotilla-claude-oauth-preflight",
                                &config_dir,
                            ],
                            Path::new("/"),
                            &ChannelLabel::Default,
                            material.as_bytes(),
                        )
                        .await
                        .or_else(|error| {
                            if claude_headless_credit_pool_is_exhausted(&error) {
                                tracing::warn!(
                                    credential = %name,
                                    detail = %bounded_adapter_error(name, "claude-oauth", &error.replace(material, "[redacted]")),
                                    "Claude OAuth preflight exhausted headless credits; continuing because interactive crews use subscription limits"
                                );
                                Ok(String::new())
                            } else {
                                Err(format!("subscription token preflight failed: {error}"))
                            }
                        });
                    // The scratch dir is only needed while the probe runs; the
                    // persistent-base fallback would otherwise accumulate
                    // whatever `claude -p` writes for the daemon's lifetime,
                    // and removal guarantees the next probe starts empty.
                    if let Err(error) = runner.run("rm", &["-rf", &config_dir], Path::new("/"), &ChannelLabel::Default).await {
                        tracing::warn!(credential = %name, %error, "failed to remove Claude OAuth preflight scratch directory");
                    }
                    probe?;
                }
                env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), material.to_string());
                env.insert(
                    "CLAUDE_CONFIG_DIR".to_string(),
                    delivery_paths.credential_dir(name).join("claude").to_string_lossy().into_owned(),
                );
            }
            CredentialConsumer::Codex => {
                let delivery_paths = delivery_paths.expect("Codex adapter resolves delivery paths");
                let codex_home = delivery_paths.credential_dir(name).join("codex").to_string_lossy().into_owned();
                if !already_prepared {
                    runner
                        .run("mkdir", &["-p", &codex_home], Path::new("/"), &ChannelLabel::Default)
                        .await
                        .map_err(|error| format!("create writable login cache: {error}"))?;
                    runner
                        .run_with_input(
                            "sh",
                            &["-c", "CODEX_HOME=\"$1\" codex login --with-api-key", "flotilla-codex-login", &codex_home],
                            Path::new("/"),
                            &ChannelLabel::Default,
                            material.as_bytes(),
                        )
                        .await
                        .map_err(|error| format!("login transformation failed: {error}"))?;
                    runner
                        .run(
                            "sh",
                            &["-c", "CODEX_HOME=\"$1\" codex login status", "flotilla-codex-status", &codex_home],
                            Path::new("/"),
                            &ChannelLabel::Default,
                        )
                        .await
                        .map_err(|error| format!("login preflight failed: {error}"))?;
                    api_key_preflight(
                        &*runner,
                        "https://api.openai.com/v1/models?limit=1",
                        &[("Authorization", &format!("Bearer {material}"))],
                    )
                    .await?;
                }
                env.insert("CODEX_HOME".to_string(), codex_home);
            }
            CredentialConsumer::ReviewBundleStore { endpoint, bucket, region, public_base_url, allow_http, virtual_hosted_style } => {
                let delivery_paths = delivery_paths.expect("review bundle store adapter resolves delivery paths");
                serde_json::from_str::<flotilla_resources::ReviewBundleWriteCredential>(material)
                    .map_err(|error| format!("credential file must contain review-bundle access key JSON: {error}"))?;
                let credential_file = delivery_paths.credential_dir(name).join("review-bundle.json");
                if !already_prepared {
                    runner
                        .write_file_with_mode(&credential_file, material, 0o600)
                        .await
                        .map_err(|error| format!("write credential file: {error}"))?;
                }
                env.insert("FLOTILLA_REVIEW_STORE_CREDENTIAL_FILE".to_string(), credential_file.to_string_lossy().into_owned());
                env.insert("FLOTILLA_REVIEW_STORE_ENDPOINT".to_string(), endpoint.clone());
                env.insert("FLOTILLA_REVIEW_STORE_BUCKET".to_string(), bucket.clone());
                env.insert("FLOTILLA_REVIEW_STORE_REGION".to_string(), region.clone());
                env.insert("FLOTILLA_REVIEW_STORE_PUBLIC_BASE_URL".to_string(), public_base_url.clone());
                env.insert("FLOTILLA_REVIEW_STORE_ALLOW_HTTP".to_string(), allow_http.to_string());
                env.insert("FLOTILLA_REVIEW_STORE_VIRTUAL_HOSTED_STYLE".to_string(), virtual_hosted_style.to_string());
                env.insert("FLOTILLA_REVIEW_STORE_PREFIX".to_string(), format!("{}/", flotilla_resources::REVIEW_BUNDLE_ROOT));
            }
            CredentialConsumer::DockerRegistry { .. } => {}
        }
        Ok(AdapterDelivery { env, git_credential })
    }
}

fn canonical_git_http_host(host: &str) -> Result<String, String> {
    let parsed_url = Url::parse(&format!("https://{host}")).map_err(|error| format!("invalid Git HTTPS host: {error}"))?;
    if parsed_url.username() != ""
        || parsed_url.password().is_some()
        || parsed_url.path() != "/"
        || parsed_url.query().is_some()
        || parsed_url.fragment().is_some()
    {
        return Err("Git HTTPS host must contain only a hostname and optional port".to_string());
    }
    let canonical_host = parsed_url.host_str().ok_or_else(|| "Git HTTPS host has no hostname".to_string())?;
    Ok(match parsed_url.port() {
        Some(port) => format!("{canonical_host}:{port}"),
        None => canonical_host.to_string(),
    })
}

fn forgejo_git_host(server_url: &str) -> Result<String, String> {
    let parsed_url = Url::parse(server_url.trim_end_matches('/')).map_err(|error| format!("invalid Forgejo server URL: {error}"))?;
    if parsed_url.scheme() != "https" {
        return Err("Forgejo server URL must use HTTPS".to_string());
    }
    let host = parsed_url.host_str().ok_or_else(|| "Forgejo server URL has no host".to_string())?;
    Ok(match parsed_url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

fn git_credential_fragment(credential_name: &str, adapter: &str, credential_url: impl Into<String>, helper: impl Into<String>) -> Fragment {
    Fragment::builder()
        .target(TargetId::GitConfig)
        .key(TargetKey::GitConfig(GitConfigKey::subsection("credential", credential_url, "helper")))
        .value(helper)
        .merge(Merge::Append)
        .provenance(Provenance::new(format!("credential/{adapter} {credential_name}")))
        .build()
}

fn codex_home_fragment(credential_name: &str, path: impl Into<String>) -> Fragment {
    agent_environment_fragment("CODEX_HOME", path, format!("credential/codex {credential_name}"))
}

fn github_app_token_file(paths: &CredentialDeliveryPaths, credential_name: &str) -> PathBuf {
    paths.credential_dir(credential_name).join("token")
}

async fn cleanup_stale_github_app_token_files_in(base: &Path) -> Result<(), String> {
    let credentials = base.join("credentials");
    let mut directories = match tokio::fs::read_dir(&credentials).await {
        Ok(directories) => directories,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("list credential directories at {}: {error}", credentials.display())),
    };
    let mut errors = Vec::new();
    while let Some(directory) = directories.next_entry().await.map_err(|error| format!("list credential directories: {error}"))? {
        match directory.file_type().await {
            Ok(file_type) if !file_type.is_dir() => continue,
            Err(error) => {
                errors.push(format!("inspect credential directory {}: {error}", directory.path().display()));
                continue;
            }
            Ok(_) => {}
        }
        if let Err(error) = cleanup_stale_github_app_token_files_in_directory(&directory.path()).await {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn cleanup_stale_github_app_token_files_in_directory(directory: &Path) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .map_err(|error| format!("list credential staging files in {}: {error}", directory.display()))?;
    let mut errors = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(|error| format!("list credential staging files: {error}"))? {
        let name = entry.file_name();
        let Some(suffix) = name
            .to_str()
            .and_then(|name| name.strip_prefix("token.tmp-").or_else(|| name.rsplit_once(".flotilla-tmp-").map(|(_, suffix)| suffix)))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(suffix).is_err() {
            continue;
        }
        match entry.file_type().await {
            Ok(file_type) if !file_type.is_file() => continue,
            Err(error) => {
                errors.push(format!("inspect staging file {}: {error}", entry.path().display()));
                continue;
            }
            Ok(_) => {}
        }
        let path = entry.path();
        if let Err(error) = tokio::fs::remove_file(&path).await {
            errors.push(format!("remove stale GitHub App token staging file {}: {error}", path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

async fn cleanup_stale_github_app_token_files_with_runner(runner: &dyn CommandRunner, base: &Path) -> Result<(), String> {
    runner
        .run(
            "sh",
            &[
                "-c",
                "failed=0; \
                for directory in \"$1\"/credentials/*; do \
                    [ -d \"$directory\" ] && [ ! -L \"$directory\" ] || continue; \
                    for file in \"$directory\"/token.tmp-* \"$directory\"/*.flotilla-tmp-*; do \
                        [ -f \"$file\" ] && [ ! -L \"$file\" ] || continue; \
                        case \"${file##*/}\" in \
                            token.tmp-????????-????-????-????-????????????|*.flotilla-tmp-????????-????-????-????-????????????) \
                                name=${file##*/}; \
                                suffix=${name#token.tmp-}; suffix=${suffix##*.flotilla-tmp-}; \
                                hex=$(printf '%s' \"$suffix\" | tr -d '-'); \
                                case \"$hex\" in \
                                    ????????????????????????????????) \
                                        case \"$hex\" in *[!0123456789abcdefABCDEF]*) continue;; esac; \
                                        rm -f -- \"$file\" || failed=1;; \
                                esac;; \
                        esac; \
                    done; \
                done; \
                exit \"$failed\"",
                "flotilla-github-app-token-cleanup",
                &base.to_string_lossy(),
            ],
            Path::new("/"),
            &ChannelLabel::Default,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("clean delivered GitHub App token staging files: {error}"))
}

async fn write_github_app_token_file(runner: &dyn CommandRunner, path: &Path, token: &str) -> Result<(), String> {
    runner.write_file_with_mode(path, token, 0o600).await.map_err(|error| format!("write token file: {error}"))
}

async fn replace_github_app_token_file(runner: &dyn CommandRunner, path: &Path, token: &str) -> Result<(), String> {
    write_github_app_token_file(runner, path, token).await
}

async fn write_executable(runner: &dyn CommandRunner, path: &Path, contents: &str, context: &str) -> Result<(), String> {
    runner.write_file_with_mode(path, contents, 0o700).await.map_err(|error| format!("write {context}: {error}"))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn expand_path(env: &dyn EnvVars, path: &str) -> PathBuf {
    path.strip_prefix("~/")
        .and_then(|relative| env.get("HOME").map(|home| PathBuf::from(home).join(relative)))
        .unwrap_or_else(|| PathBuf::from(path))
}

async fn api_key_preflight(runner: &dyn CommandRunner, url: &str, headers: &[(&str, &str)]) -> Result<(), String> {
    let mut config = "silent\nshow-error\nfail\n".to_string();
    for (name, value) in headers {
        config.push_str(&format!("header = \"{}: {}\"\n", sanitize_curl_config(name), sanitize_curl_config(value)));
    }
    config.push_str(&format!("url = \"{}\"\n", sanitize_curl_config(url)));
    runner
        .run_with_input("curl", &["--config", "-"], Path::new("/"), &ChannelLabel::Default, config.as_bytes())
        .await
        .map(|_| ())
        .map_err(|error| format!("authentication preflight failed: {error}"))
}

async fn remove_registry_config(path: &Path) -> Result<(), std::io::Error> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The claude CLI writes millisecond epochs; treat implausibly-large
/// second values as milliseconds so either unit decodes to the same instant.
/// Non-positive values are sentinels for absent metadata, not dates.
fn epoch_to_datetime(value: i64) -> Option<DateTime<Utc>> {
    const MILLISECOND_THRESHOLD: i64 = 100_000_000_000;
    if value <= 0 {
        return None;
    }
    if value >= MILLISECOND_THRESHOLD {
        DateTime::from_timestamp_millis(value)
    } else {
        DateTime::from_timestamp(value, 0)
    }
}

fn sanitize_curl_config(value: &str) -> String {
    value.replace(['\\', '"', '\r', '\n'], "")
}

fn registry_environment_dir(environment_ref: &str) -> String {
    let mut name = String::from("env-");
    for byte in environment_ref.bytes() {
        use std::fmt::Write;
        write!(&mut name, "{byte:02x}").expect("writing to String cannot fail");
    }
    name
}

fn safe_component(name: &str) -> String {
    name.chars().map(|character| if character.is_ascii_alphanumeric() || character == '-' { character } else { '-' }).collect()
}

fn image_registry_matches(image: &str, registry: &str) -> bool {
    image == registry || image.strip_prefix(registry).is_some_and(|remainder| remainder.starts_with('/'))
}

fn claude_headless_credit_pool_is_exhausted(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    ["credit", "quota", "billing"].iter().any(|indicator| output.contains(indicator))
}

fn bounded_adapter_error(name: &str, adapter: &str, detail: &str) -> String {
    const MAX_DETAIL: usize = 512;
    let mut detail = detail.trim().chars().take(MAX_DETAIL).collect::<String>();
    if detail.is_empty() {
        detail = "preflight failed".to_string();
    }
    format!("credential `{name}` adapter `{adapter}`: {detail}")
}

fn validate_scalar_material(name: &str, adapter: &str, material: &str) -> Result<(), String> {
    if material.contains(['\0', '\r', '\n']) {
        Err(bounded_adapter_error(name, adapter, "source produced invalid multiline scalar material"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex as StdMutex,
        },
    };

    use async_trait::async_trait;
    use flotilla_core::providers::{
        discovery::EnvironmentAssertion,
        replay::{Masks, ReplayHttpClient, Session},
        CommandOutput, ProcessCommandRunner,
    };
    use flotilla_protocol::NodeId;
    use flotilla_resources::{
        CredentialPlacementRequirements, ForgeSpec, InMemoryBackend, InputMeta, ProjectRepositorySpec, ProjectSpec, RepositorySpec,
        VirtualClock,
    };

    use super::*;
    use crate::agent_material::{tests::promisor_runner, AgentMaterialRegistry, FLOTILLA_SKILLS_DIR_ENV};

    #[derive(Default)]
    struct TestEnv(BTreeMap<String, String>);

    impl EnvVars for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    struct RotatingTokenEnv(StdMutex<VecDeque<String>>);

    impl EnvVars for RotatingTokenEnv {
        fn get(&self, key: &str) -> Option<String> {
            (key == "TEST_CLAUDE_TOKEN").then(|| self.0.lock().expect("rotating token lock").pop_front()).flatten()
        }
    }

    struct FakeGithubAppTokenMinter {
        tokens: StdMutex<VecDeque<Result<GithubAppToken, String>>>,
        requests: StdMutex<Vec<GithubAppMintRequest>>,
    }

    struct BlockingGithubAppTokenMinter {
        now: DateTime<Utc>,
        calls: AtomicUsize,
        refresh_started: tokio::sync::Notify,
        release_refresh: tokio::sync::Notify,
    }

    // Test double at the GitHub token-mint HTTP boundary.
    struct SlowFlakySkillMinter {
        calls: AtomicUsize,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        first_wave: tokio::sync::Barrier,
    }

    #[async_trait]
    impl GithubAppTokenMinter for SlowFlakySkillMinter {
        async fn resolve_installation(&self, _request: &GithubAppInstallationRequest) -> Result<u64, String> {
            Err("unexpected installation resolution".to_string())
        }

        async fn mint(&self, _request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            if call < 6 {
                self.first_wave.wait().await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if matches!(call, 0 | 2) {
                Err(GithubAppMintError::Transient("GitHub returned HTTP 500".to_string()))
            } else {
                Ok(GithubAppToken { permissions: None, value: format!("test-token-{call}"), expires_at: Utc::now() + Duration::hours(1) })
            }
        }
    }

    // Six independent stagings must enter mint concurrently, recover transient failures,
    // fetch with distinct nonempty tokens, and clean up their own token files.
    #[tokio::test(flavor = "multi_thread")]
    async fn six_concurrent_skill_stagings_each_use_own_nonempty_token_after_slow_flaky_mints() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skills = temp.path().join("generation/skills");
        std::fs::create_dir_all(&skills).expect("skill bundle");
        std::fs::write(
            skills.join(".flotilla-sources.json"),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"github-skills-fork"}]}"#,
        )
        .expect("manifest");
        std::fs::write(temp.path().join("fetches.capture-token"), "").expect("capture marker");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-skills-fork".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: Some(BTreeMap::from([("contents".to_string(), "read".to_string())])),
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: "/host/app-id".to_string(),
                        private_key_path: "/host/key".to_string(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("credential declaration");
        let minter = Arc::new(SlowFlakySkillMinter {
            calls: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            first_wave: tokio::sync::Barrier::new(6),
        });
        let store = Arc::new(CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            GithubAppMinting { clock: Arc::new(SystemClock), minter: minter.clone() },
            temp.path().to_path_buf(),
        ));
        let registry = Arc::new(AgentMaterialRegistry::new(Arc::new(TestEnv(BTreeMap::from([
            ("HOME".to_string(), temp.path().to_string_lossy().into_owned()),
            (FLOTILLA_SKILLS_DIR_ENV.to_string(), skills.to_string_lossy().into_owned()),
        ])))));
        // Prepare every fake Git fixture before spawning: constructors rewrite the shared executable.
        let runners = (0..6)
            .map(|index| {
                let mut runner = promisor_runner(temp.path());
                runner.config_base = temp.path().join(format!("config-{index}"));
                runner
            })
            .collect::<Vec<_>>();
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            futures::future::join_all(runners.into_iter().enumerate().map(|(index, runner)| {
                let store = Arc::clone(&store);
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    let token_file = store
                        .prepare_skill_source("github-skills-fork", "https://github.com/example/private-skills.git", &runner)
                        .await
                        .map_err(|error| format!("skill source private-skills credential github-skills-fork mint failed: {error}"))?;
                    let outcome = registry
                        .stage_test_skills(
                            &format!("crew-{index}"),
                            &BTreeSet::from(["claude-code".to_string()]),
                            &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                            &BTreeMap::from([("private-skills".to_string(), token_file.clone())]),
                            &runner,
                        )
                        .await;
                    assert!(!token_file.exists(), "crew {index} must clean its own token");
                    outcome?;
                    // Each staging must install the fake Git response, never content from real Git.
                    let staged_skill = runner.config_base.join("claude/skills/private-source/SKILL.md");
                    assert_eq!(
                        std::fs::read_to_string(staged_skill).expect("fake Git skill must be staged"),
                        "---\nname: private-source\ndescription: test skill\n---\n# Private source\n",
                        "crew {index} must use the fake Git response",
                    );
                    Ok::<(), String>(())
                })
            })),
        )
        .await
        .expect("all six stagings must enter mint concurrently and finish");
        for result in results {
            result.expect("staging task").expect("retryable mints should recover and stage");
        }
        assert_eq!(minter.max_in_flight.load(Ordering::SeqCst), 6, "all six initial mints must overlap");
        assert_eq!(minter.calls.load(Ordering::SeqCst), 8, "two transient failures must each be retried");
        let fetches = std::fs::read_to_string(temp.path().join("fetches")).expect("fake Git fetch log");
        assert_eq!(fetches.lines().collect::<Vec<_>>(), vec!["fetch"; 6], "each staging must fetch through fake Git");
        let tokens = std::fs::read_to_string(temp.path().join("fetches.tokens")).expect("captured fetch tokens");
        let tokens = tokens.lines().collect::<BTreeSet<_>>();
        assert_eq!(tokens.len(), 6, "each fetch must use its own nonempty token");
        assert!(tokens.iter().all(|token| token.starts_with("test-token-")));
    }

    #[async_trait]
    impl GithubAppTokenMinter for BlockingGithubAppTokenMinter {
        async fn resolve_installation(&self, _request: &GithubAppInstallationRequest) -> Result<u64, String> {
            Err("unexpected installation resolution".to_string())
        }

        async fn mint(&self, _request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    Ok(GithubAppToken { permissions: None, value: "initial-token".to_string(), expires_at: self.now + Duration::hours(1) })
                }
                1 => {
                    self.refresh_started.notify_one();
                    self.release_refresh.notified().await;
                    Ok(GithubAppToken {
                        permissions: None,
                        value: "stale-refresh-token".to_string(),
                        expires_at: self.now + Duration::hours(2),
                    })
                }
                2 => Ok(GithubAppToken {
                    permissions: None,
                    value: "reprepared-token".to_string(),
                    expires_at: self.now + Duration::hours(2),
                }),
                call => Err(GithubAppMintError::Other(format!("unexpected mint call {call}"))),
            }
        }
    }

    #[async_trait]
    impl GithubAppTokenMinter for FakeGithubAppTokenMinter {
        async fn resolve_installation(&self, _request: &GithubAppInstallationRequest) -> Result<u64, String> {
            Err("unexpected installation resolution".to_string())
        }

        async fn mint(&self, request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
            self.requests.lock().expect("GitHub App requests lock").push(request.clone());
            self.tokens
                .lock()
                .expect("GitHub App tokens lock")
                .pop_front()
                .unwrap_or_else(|| Err("no fake token available".to_string()))
                .map_err(GithubAppMintError::Other)
        }
    }

    #[test]
    fn github_app_refresh_deadline_keeps_fifteen_minutes_of_lead() {
        let issued_at: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        assert_eq!(github_app_refresh_at(issued_at, issued_at + Duration::hours(1)), issued_at + Duration::minutes(30));
        assert_eq!(github_app_refresh_at(issued_at, issued_at + Duration::minutes(20)), issued_at + Duration::minutes(5));
    }

    #[tokio::test]
    async fn github_app_refreshes_at_half_life_after_a_stalled_tick() {
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let clock = Arc::new(VirtualClock::new(now));
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Ok(GithubAppToken { permissions: None, value: "replacement".to_string(), expires_at: now + Duration::hours(2) }),
                Ok(GithubAppToken { permissions: None, value: "late-replacement".to_string(), expires_at: now + Duration::hours(3) }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_github_app_minter(
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a")),
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        store.github_app_deliveries.lock().await.insert(
            ("vessel".to_string(), "github-app".to_string()),
            GithubAppDelivery {
                effective_permissions: None,
                generation: uuid::Uuid::new_v4(),
                request: GithubAppMintRequest {
                    installation_id: 1,
                    app_id_path: "app-id".to_string(),
                    private_key_path: "key".to_string(),
                    repositories: vec!["flotilla".to_string()],
                    permissions: None,
                },
                runner,
                token_file: PathBuf::from("/state/credentials/github-app/token"),
                issued_at: now,
                expires_at: now + Duration::hours(1),
                refresh_failures: 0,
                next_refresh_attempt_at: None,
                installation_repository: None,
                scope: None,
            },
        );
        clock.advance(Duration::minutes(29));
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        assert!(minter.requests.lock().expect("requests lock").is_empty());
        clock.advance(Duration::minutes(11)); // The scheduler missed the exact half-life tick.
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 1, "refresh before the five-minute expiry margin");
        clock.advance(Duration::minutes(76));
        let errors = store.refresh_due_github_app_tokens().await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].should_surface, "a successful but late refresh still needs attention");
        assert!(errors[0].message.contains("refresh started inside the expiry margin"));
        assert!(errors[0].message.contains("expires in 4 minutes"));
        clock.advance(Duration::minutes(60));
        let errors = store.refresh_due_github_app_tokens().await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].should_surface, "one failed refresh within the expiry margin needs attention immediately");
    }

    #[tokio::test]
    async fn github_app_refresh_failures_back_off_until_expiry_margin() {
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let clock = Arc::new(VirtualClock::new(now));
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Err("outage 1".to_string()),
                Err("outage 2".to_string()),
                Err("outage 3".to_string()),
                Err("outage 4".to_string()),
                Err("outage 5".to_string()),
                Err("outage 6".to_string()),
                Err("outage 7".to_string()),
                Ok(GithubAppToken { permissions: None, value: "recovered".to_string(), expires_at: now + Duration::hours(2) }),
                Err("new outage".to_string()),
                Ok(GithubAppToken { permissions: None, value: "recovered again".to_string(), expires_at: now + Duration::hours(3) }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_github_app_minter(
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a")),
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        store.github_app_deliveries.lock().await.insert(
            ("vessel".to_string(), "github-app".to_string()),
            GithubAppDelivery {
                effective_permissions: None,
                generation: uuid::Uuid::new_v4(),
                request: GithubAppMintRequest {
                    installation_id: 1,
                    app_id_path: "app-id".to_string(),
                    private_key_path: "key".to_string(),
                    repositories: vec!["flotilla".to_string()],
                    permissions: None,
                },
                runner,
                token_file: PathBuf::from("/state/credentials/github-app/token"),
                issued_at: now,
                expires_at: now + Duration::hours(1),
                refresh_failures: 0,
                next_refresh_attempt_at: None,
                installation_repository: None,
                scope: None,
            },
        );

        clock.advance(Duration::minutes(30));
        assert_eq!(store.refresh_due_github_app_tokens().await.len(), 1);
        for (advance, expected_calls) in [
            (Duration::seconds(30), 2),
            (Duration::minutes(1), 3),
            (Duration::minutes(2), 4),
            (Duration::minutes(4), 5),
            (Duration::minutes(5), 6),
        ] {
            clock.advance(advance - Duration::seconds(1));
            assert!(store.refresh_due_github_app_tokens().await.is_empty(), "retry must wait for backoff");
            assert_eq!(minter.requests.lock().expect("requests lock").len(), expected_calls - 1);
            clock.advance(Duration::seconds(1));
            assert_eq!(store.refresh_due_github_app_tokens().await.len(), 1);
            assert_eq!(minter.requests.lock().expect("requests lock").len(), expected_calls);
        }
        clock.advance(Duration::minutes(4));
        assert!(store.refresh_due_github_app_tokens().await.is_empty(), "backoff remains capped at five minutes");
        clock.advance(Duration::minutes(8) + Duration::seconds(20));
        assert_eq!(store.refresh_due_github_app_tokens().await.len(), 1);
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 7);
        clock.advance(Duration::seconds(10));
        let errors = store.refresh_due_github_app_tokens().await;
        assert_eq!(errors.len(), 1);
        assert!(errors[0].should_surface, "entry into expiry margin needs immediate attention");
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 8);

        clock.advance(Duration::minutes(33));
        let errors = store.refresh_due_github_app_tokens().await;
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].should_surface, "a new token starts a new failure sequence");
        clock.advance(Duration::seconds(29));
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        clock.advance(Duration::seconds(1));
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 10, "backoff resets after a successful mint");
    }

    #[tokio::test]
    async fn github_app_mint_uses_resolved_permissions_for_each_crew() {
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Ok(GithubAppToken { permissions: None, value: "coder-token".to_string(), expires_at: now + Duration::hours(1) }),
                Ok(GithubAppToken { permissions: None, value: "reviewer-token".to_string(), expires_at: now + Duration::hours(1) }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let repository = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository");
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(repository.key().to_string()).build(), &repository)
            .await
            .expect("create repository");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner,
            GithubAppMinting { clock: Arc::new(VirtualClock::new(now)), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        let spec = CredentialSpecSpec {
            consumer: CredentialConsumer::GithubApp {
                actor_login: None,
                installation_id: Some(9876),
                installation_repository: None,
                permissions: Some(BTreeMap::from([("contents".to_string(), "write".to_string())])),
            },
            source: CredentialSource::GithubApp { app_id_path: "/host/app-id".to_string(), private_key_path: "/host/key".to_string() },
            lifecycle: CredentialLifecycle::Refreshable,
            placement: CredentialPlacementRequirements::default(),
        };
        let scope = BTreeSet::from([repository.key()]);
        for permissions in [
            BTreeMap::from([("contents".to_string(), "write".to_string())]),
            BTreeMap::from([("contents".to_string(), "read".to_string())]),
        ] {
            store.resolve_for_adapter("github-app", &spec, Some(&scope), Some(&permissions)).await.expect("mint token");
        }
        let requests = minter.requests.lock().expect("requests lock");
        assert_eq!(requests[0].permissions.as_ref().expect("coder permissions")["contents"], "write");
        assert_eq!(requests[1].permissions.as_ref().expect("reviewer permissions")["contents"], "read");
    }

    #[tokio::test]
    async fn project_membership_remints_live_token_without_widening_fixed_scope() {
        use flotilla_core::crew_capabilities::{credential_card, SessionCapabilitySource};
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let clock = Arc::new(VirtualClock::new(now));
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Ok(GithubAppToken { permissions: None, value: "project-initial".to_string(), expires_at: now + Duration::hours(1) }),
                Ok(GithubAppToken { permissions: None, value: "fixed-initial".to_string(), expires_at: now + Duration::hours(1) }),
                Ok(GithubAppToken {
                    permissions: Some(BTreeMap::from([("workflows".into(), "write".into()), ("contents".into(), "write".into())])),
                    value: "project-expanded".to_string(),
                    expires_at: now + Duration::hours(1),
                }),
                Ok(GithubAppToken { permissions: None, value: "different-role".to_string(), expires_at: now + Duration::hours(1) }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let first = RepositorySpec::remote("https://github.com/flotilla-org/first").expect("first repository");
        let second = RepositorySpec::remote("https://github.com/flotilla-org/second").expect("second repository");
        for (name, spec) in [("first", &first), ("second", &second)] {
            backend
                .clone()
                .using::<Repository>("flotilla")
                .create(&InputMeta::builder().name(name.to_string()).build(), spec)
                .await
                .expect("create repository");
        }
        let project = |repositories: Vec<RepositoryKey>| {
            ProjectSpec::builder()
                .display_name("Island".to_string())
                .default_workflow_ref("workflow".to_string())
                .repositories(repositories.into_iter().map(|repo| ProjectRepositorySpec::builder().repo(repo).build()).collect())
                .build()
        };
        let projects = backend.clone().using::<Project>("flotilla");
        projects
            .create(&InputMeta::builder().name("island".to_string()).build(), &project(vec![first.key()]))
            .await
            .expect("create project");
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: "/host-only/github-app.id".to_string(),
                        private_key_path: "/host-only/github-app.pem".to_string(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        let refs = BTreeSet::from(["github-app".to_string()]);
        let initial = BTreeMap::from([("github-app".to_string(), BTreeSet::from([first.key()]))]);
        store.prepare_scoped("project-env", &refs, &initial, runner.clone()).await.expect("prepare project credential");
        store.prepare_scoped("fixed-env", &refs, &initial, runner.clone()).await.expect("prepare fixed credential");
        assert!(!credential_card(&store.credentials("project-env", &refs).await.expect("launch capabilities"))
            .contains("You can push `.github/workflows`"));
        store
            .set_github_app_scopes(
                "project-env",
                &BTreeMap::from([(
                    "github-app".to_string(),
                    GithubAppScope {
                        fixed_repositories: BTreeSet::new(),
                        projects: BTreeSet::from(["island".to_string()]),
                        permissions: None,
                    },
                )]),
            )
            .await;
        store
            .set_github_app_scopes(
                "fixed-env",
                &BTreeMap::from([(
                    "github-app".to_string(),
                    GithubAppScope { fixed_repositories: BTreeSet::from([first.key()]), projects: BTreeSet::new(), permissions: None },
                )]),
            )
            .await;
        projects.delete("island").await.expect("remove old project membership");
        projects
            .create(&InputMeta::builder().name("island".to_string()).build(), &project(vec![first.key(), second.key()]))
            .await
            .expect("expand project membership");

        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        let observed = store.credentials("project-env", &refs).await.expect("live refreshed delivery");
        assert_eq!(observed[0].repositories, ["first", "second"]);
        assert!(credential_card(&observed).contains("You can push `.github/workflows`"));
        assert_eq!(store.credentials("fixed-env", &refs).await.expect("fixed delivery")[0].repositories, ["first"]);
        {
            let requests = minter.requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 3, "membership change remints before expiry; explicit scope stays unchanged");
            assert_eq!(requests[2].repositories, ["first", "second"]);
        }
        {
            let token_writes = runner.writes.lock().expect("writes lock");
            assert!(token_writes.iter().any(|(_, contents)| contents.contains("project-expanded")));
        }
        let different_permissions =
            BTreeMap::from([("github-app".to_string(), BTreeMap::from([("contents".to_string(), "read".to_string())]))]);
        let error = store
            .prepare_scoped_with_permissions("project-env", &refs, &initial, &different_permissions, runner.clone())
            .await
            .expect_err("shared environment must not overwrite a different crew's token");
        assert!(error.contains("different minted permissions"), "{error}");
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 3, "conflicting crew must not mint a discarded token");
        assert!(!runner.writes.lock().expect("writes lock").iter().any(|(_, contents)| contents.contains("different-role")));
        // Shared environments cannot advertise credentials outside this session's
        // references. Failed remints preserve the previous effective card.
        assert!(store.credentials("project-env", &BTreeSet::new()).await.expect("ungranted credential").is_empty());
        let before_failure = store.credentials("project-env", &refs).await.expect("effective card");
        {
            let mut tokens = minter.tokens.lock().expect("tokens");
            tokens.clear();
            tokens.push_back(Err("mint unavailable".into()));
        }
        store
            .set_github_app_scopes(
                "project-env",
                &BTreeMap::from([(
                    "github-app".into(),
                    GithubAppScope { fixed_repositories: BTreeSet::from([second.key()]), projects: BTreeSet::new(), permissions: None },
                )]),
            )
            .await;
        assert_eq!(store.refresh_due_github_app_tokens().await.len(), 1);
        assert_eq!(store.credentials("project-env", &refs).await.expect("failed refresh card"), before_failure);
        clock.advance(Duration::hours(2));
        assert!(store.credentials("project-env", &refs).await.expect("expired grant").is_empty());
    }

    type RecordedCall = (String, Vec<String>, Vec<u8>);

    #[derive(Default)]
    struct RecordingRunner {
        calls: StdMutex<Vec<RecordedCall>>,
        writes: StdMutex<Vec<(PathBuf, String)>>,
        protected_writes: StdMutex<Vec<(PathBuf, u32)>>,
        runtime_dir_checks: StdMutex<VecDeque<bool>>,
        // Pause the Docker login process boundary while its cache is unindexed.
        registry_login_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Semaphore)>>,
    }

    impl RecordingRunner {
        fn with_runtime_dir_checks(checks: impl IntoIterator<Item = bool>) -> Self {
            Self { runtime_dir_checks: StdMutex::new(checks.into_iter().collect()), ..Self::default() }
        }
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls lock").push((cmd.to_string(), args.iter().map(|arg| (*arg).to_string()).collect(), Vec::new()));
            if cmd == "sh" && args.contains(&"command -v gh") {
                Ok("/usr/bin/gh\n".to_string())
            } else if cmd == "sh" && args.contains(&"printf '%s' \"$PATH\"") {
                Ok("/usr/bin:/bin".to_string())
            } else {
                Ok(String::new())
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            let stdout = self.run(cmd, args, cwd, label).await?;
            let success = if cmd == "sh" && args.contains(&"flotilla-xdg-runtime-dir") {
                self.runtime_dir_checks.lock().expect("runtime dir checks lock").pop_front().unwrap_or(true)
            } else {
                true
            };
            Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(if success { 0 } else { 1 }) })
        }

        async fn run_with_input(
            &self,
            cmd: &str,
            args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            input: &[u8],
        ) -> Result<String, String> {
            self.calls.lock().expect("calls lock").push((
                cmd.to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
                input.to_vec(),
            ));
            if cmd == "docker" && args.contains(&"login") {
                if let Some(gate) = &self.registry_login_gate {
                    gate.0.notify_one();
                    gate.1.acquire().await.expect("release registry login").forget();
                }
            }
            Ok(String::new())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }

        async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
            self.writes.lock().expect("writes lock").push((path.to_path_buf(), content.to_string()));
            Ok(())
        }

        async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
            assert!(matches!(mode, 0o600 | 0o700));
            self.protected_writes.lock().expect("protected writes lock").push((path.to_path_buf(), mode));
            self.write_file(path, content).await
        }
    }

    struct FailedTokenWriteRunner;

    struct PathCommandRunner {
        bin_dir: PathBuf,
    }

    #[async_trait]
    impl CommandRunner for PathCommandRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let output = self.run_output(cmd, args, cwd, label).await?;
            if output.success() {
                Ok(output.stdout)
            } else {
                Err(output.stderr)
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            let output = tokio::process::Command::new(cmd)
                .args(args)
                .current_dir(cwd)
                .env("PATH", format!("{}:/usr/bin:/bin", self.bin_dir.display()))
                .output()
                .await
                .map_err(|error| error.to_string())?;
            Ok(CommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code(),
            })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }
    }

    #[async_trait]
    impl CommandRunner for FailedTokenWriteRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            assert_eq!(cmd, "rm", "only cleanup may run after a failed write");
            assert_eq!(&args[..2], &["-f", "--"]);
            tokio::fs::remove_file(args[2]).await.map_err(|error| error.to_string())?;
            Ok(String::new())
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            Err("unexpected command".to_string())
        }

        async fn run_with_input(
            &self,
            _cmd: &str,
            _args: &[&str],
            _cwd: &Path,
            _label: &ChannelLabel,
            _input: &[u8],
        ) -> Result<String, String> {
            Err("unexpected command".to_string())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            false
        }

        async fn write_file_with_mode(&self, _path: &Path, _content: &str, mode: u32) -> Result<(), String> {
            assert_eq!(mode, 0o600);
            Err("simulated interrupted credential write".to_string())
        }
    }

    #[tokio::test]
    async fn failed_github_app_refresh_preserves_live_token_and_removes_partial_file() {
        let directory = tempfile::tempdir().expect("create credential directory");
        let token_file = directory.path().join("token");
        tokio::fs::write(&token_file, "still-valid-token").await.expect("stage old token");

        let error = replace_github_app_token_file(&FailedTokenWriteRunner, &token_file, "new-token")
            .await
            .expect_err("interrupted refresh must fail");

        assert!(error.contains("simulated interrupted credential write"));
        assert_eq!(tokio::fs::read_to_string(&token_file).await.expect("read old token"), "still-valid-token");
        assert_eq!(std::fs::read_dir(directory.path()).expect("list credential directory").count(), 1);
    }

    #[tokio::test]
    async fn startup_cleanup_removes_only_github_app_staging_files() {
        let state = tempfile::tempdir().expect("create state directory");
        let credential_dir = state.path().join("credentials/github-app");
        tokio::fs::create_dir_all(&credential_dir).await.expect("create credential directory");
        let abandoned = credential_dir.join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        let current_abandoned = credential_dir.join(format!("token.flotilla-tmp-{}", uuid::Uuid::new_v4()));
        let helper_abandoned = credential_dir.join(format!("git-credential-github-app.flotilla-tmp-{}", uuid::Uuid::new_v4()));
        let live = credential_dir.join("token");
        let unrelated = credential_dir.join("token.tmp-other");
        for path in [&abandoned, &current_abandoned, &helper_abandoned, &live, &unrelated] {
            tokio::fs::write(path, "secret material").await.expect("write credential file");
        }
        cleanup_stale_github_app_token_files_in(state.path()).await.expect("clean staging files");

        assert!(!abandoned.exists());
        assert!(!current_abandoned.exists());
        assert!(!helper_abandoned.exists());
        assert!(unrelated.exists());
        assert!(live.exists());
    }

    #[tokio::test]
    async fn startup_cleanup_sweeps_both_bases_even_if_one_fails() {
        let state = tempfile::tempdir().expect("create state directory");
        let runtime = tempfile::tempdir().expect("create runtime directory");
        let runtime_base = runtime.path().join("flotilla");
        let state_credentials = state.path().join("credentials/github-app");
        let runtime_credentials = runtime_base.join("credentials/github-app");
        for directory in [&state_credentials, &runtime_credentials] {
            tokio::fs::create_dir_all(directory).await.expect("create credential directory");
        }
        let state_staging = state_credentials.join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        let runtime_staging = runtime_credentials.join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        for path in [&state_staging, &runtime_staging] {
            tokio::fs::write(path, "abandoned material").await.expect("write staging file");
        }
        let store = CredentialStore::new(
            ResourceBackend::InMemory(InMemoryBackend::default()),
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([("XDG_RUNTIME_DIR".to_string(), runtime.path().to_string_lossy().into_owned())]))),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            state.path().to_path_buf(),
        );

        store.cleanup_stale_github_app_token_files().await.expect("sweep both credential bases");
        assert!(!state_staging.exists());
        assert!(!runtime_staging.exists());

        tokio::fs::remove_dir_all(state.path().join("credentials")).await.expect("remove state credentials directory");
        tokio::fs::write(state.path().join("credentials"), "block directory listing").await.expect("block state credentials directory");
        tokio::fs::write(&runtime_staging, "abandoned material").await.expect("write another staging file");
        store.cleanup_stale_github_app_token_files().await.expect_err("report state directory error");
        assert!(!runtime_staging.exists(), "a bad state base must not prevent sweeping the runtime base");
    }

    #[tokio::test]
    async fn delivered_environment_cleanup_removes_staging_file_without_touching_live_token() {
        let base = tempfile::tempdir().expect("create delivery base");
        let credential_dir = base.path().join("credentials/github-app");
        tokio::fs::create_dir_all(&credential_dir).await.expect("create credential directory");
        let abandoned = credential_dir.join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        let current_abandoned = credential_dir.join(format!("token.flotilla-tmp-{}", uuid::Uuid::new_v4()));
        let helper_abandoned = credential_dir.join(format!("gh.flotilla-tmp-{}", uuid::Uuid::new_v4()));
        let live = credential_dir.join("token");
        let malformed = credential_dir.join("token.tmp-zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz");
        tokio::fs::write(&abandoned, "abandoned material").await.expect("write staging file");
        tokio::fs::write(&current_abandoned, "abandoned material").await.expect("write current staging file");
        tokio::fs::write(&helper_abandoned, "abandoned material").await.expect("write helper staging file");
        tokio::fs::write(&live, "live material").await.expect("write live token");
        tokio::fs::write(&malformed, "unrelated material").await.expect("write unrelated file");

        cleanup_stale_github_app_token_files_with_runner(&ProcessCommandRunner, base.path()).await.expect("clean delivered staging file");

        assert!(!abandoned.exists());
        assert!(!current_abandoned.exists());
        assert!(!helper_abandoned.exists());
        assert!(live.exists());
        assert!(malformed.exists());
    }

    #[tokio::test]
    async fn delivered_cleanup_continues_after_one_removal_fails() {
        let base = tempfile::tempdir().expect("create delivery base");
        let bin_dir = tempfile::tempdir().expect("create command directory");
        let fake_rm = bin_dir.path().join("rm");
        tokio::fs::write(&fake_rm, "#!/bin/sh\ncase \"$3\" in */blocked/*) exit 1;; esac\nexec /bin/rm \"$@\"\n")
            .await
            .expect("write failing rm command");
        tokio::fs::set_permissions(&fake_rm, std::fs::Permissions::from_mode(0o755)).await.expect("make rm command executable");
        let blocked = base.path().join("credentials/blocked").join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        let healthy = base.path().join("credentials/healthy").join(format!("token.tmp-{}", uuid::Uuid::new_v4()));
        for path in [&blocked, &healthy] {
            tokio::fs::create_dir_all(path.parent().expect("credential directory")).await.expect("create credential directory");
            tokio::fs::write(path, "abandoned material").await.expect("write staging file");
        }

        cleanup_stale_github_app_token_files_with_runner(&PathCommandRunner { bin_dir: bin_dir.path().to_path_buf() }, base.path())
            .await
            .expect_err("report failed removal after sweeping other directories");

        assert!(blocked.exists());
        assert!(!healthy.exists());
    }

    #[test]
    fn adapter_errors_are_named_and_bounded() {
        let error = bounded_adapter_error("model-api", "codex", &"x".repeat(2_000));
        assert!(error.starts_with("credential `model-api` adapter `codex`: "));
        assert!(error.len() < 600);
    }

    #[test]
    fn registry_matching_does_not_confuse_prefixes() {
        assert!(image_registry_matches("forgejo.lab/org/image:tag", "forgejo.lab"));
        assert!(!image_registry_matches("forgejo.lab.evil/org/image:tag", "forgejo.lab"));
    }

    fn store_with_env(env: BTreeMap<String, String>) -> CredentialStore {
        CredentialStore::new(
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a")),
            "flotilla",
            Arc::new(TestEnv(env)),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            PathBuf::from("/tmp/flotilla-test-state"),
        )
    }

    // Session endpoint observations must not leak or overwrite one another in
    // shared environments. Relaunch replaces only that session's delivered facts.
    #[tokio::test]
    async fn capability_endpoints_isolate_sessions_in_shared_environments() {
        use flotilla_core::crew_capabilities::SessionCapabilitySource;
        let store = store_with_env(BTreeMap::new());
        let first = vec![("HTTPS_PROXY".into(), "https://first:secret@127.0.0.1:7001/private".into())];
        let second = vec![("DISPLAY".into(), ":2".into())];
        store.record_capability_endpoints("shared", "first", &first).await;
        let baseline = store.endpoints("shared", "first").await.unwrap();
        store.record_capability_endpoints("shared", "second", &second).await;
        assert_eq!(store.endpoints("shared", "first").await.unwrap(), baseline);
        assert_eq!(store.endpoints("shared", "second").await.unwrap(), BTreeMap::from([("GUI display".into(), ":2".into())]));
        assert!(store.endpoints("shared", "unknown").await.unwrap().is_empty());
        assert!(store.endpoints("other", "first").await.unwrap().is_empty());
        store.record_capability_endpoints("shared", "second", &[]).await;
        assert!(store.endpoints("shared", "second").await.unwrap().is_empty());
        assert_eq!(store.endpoints("shared", "first").await.unwrap(), baseline);
        store.forget_environment("shared").await.unwrap();
        assert!(store.endpoints("shared", "first").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ambient_claude_expiry_probe_reports_timestamps_and_never_material() {
        let home = tempfile::tempdir().expect("home dir");
        let claude_dir = home.path().join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.expect("create claude dir");
        let expires_at_ms: i64 = 1_756_000_000_000;
        let refresh_expires_at_secs: i64 = 1_753_000_000;
        let credentials = format!(
            r#"{{"claudeAiOauth":{{"accessToken":"sk-ant-oat01-material","refreshToken":"sk-ant-ort01-material","expiresAt":{expires_at_ms},"refreshTokenExpiresAt":{refresh_expires_at_secs},"scopes":["user:inference"],"subscriptionType":"max"}}}}"#
        );
        tokio::fs::write(claude_dir.join(".credentials.json"), &credentials).await.expect("write credentials file");
        let store = store_with_env(BTreeMap::from([("HOME".to_string(), home.path().to_string_lossy().into_owned())]));

        let expiry = store.credential_expiry().await;

        let ambient = expiry.get(AMBIENT_CLAUDE_CREDENTIAL_SCOPE).expect("ambient claude entry");
        assert_eq!(ambient.expires_at, DateTime::from_timestamp_millis(expires_at_ms));
        assert_eq!(ambient.refresh_expires_at, DateTime::from_timestamp(refresh_expires_at_secs, 0));
        let encoded = serde_json::to_string(&expiry).expect("serialize expiry map");
        assert!(!encoded.contains("material") && !encoded.contains("sk-ant"), "material leaked into expiry metadata: {encoded}");
    }

    #[tokio::test]
    async fn ambient_claude_expiry_probe_prefers_the_configured_claude_dir() {
        let config_dir = tempfile::tempdir().expect("config dir");
        tokio::fs::write(config_dir.path().join(".credentials.json"), r#"{"claudeAiOauth":{"expiresAt":1756000000000}}"#)
            .await
            .expect("write credentials file");
        let store = store_with_env(BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), config_dir.path().to_string_lossy().into_owned())]));

        let expiry = store.credential_expiry().await;

        let ambient = expiry.get(AMBIENT_CLAUDE_CREDENTIAL_SCOPE).expect("ambient claude entry");
        assert_eq!(ambient.expires_at, DateTime::from_timestamp_millis(1_756_000_000_000));
        assert_eq!(ambient.refresh_expires_at, None);
    }

    #[tokio::test]
    async fn ambient_claude_expiry_probe_is_silent_without_a_login_or_metadata() {
        let home = tempfile::tempdir().expect("home dir");
        let store = store_with_env(BTreeMap::from([("HOME".to_string(), home.path().to_string_lossy().into_owned())]));
        assert_eq!(store.credential_expiry().await, BTreeMap::new());

        let claude_dir = home.path().join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.expect("create claude dir");
        tokio::fs::write(claude_dir.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-material"}}"#)
            .await
            .expect("write credentials file");
        assert_eq!(store.credential_expiry().await, BTreeMap::new());

        tokio::fs::write(claude_dir.join(".credentials.json"), "not json").await.expect("write malformed file");
        assert_eq!(store.credential_expiry().await, BTreeMap::new());
    }

    #[tokio::test]
    async fn ambient_claude_expiry_probe_treats_non_positive_timestamps_as_absent() {
        let home = tempfile::tempdir().expect("home dir");
        let claude_dir = home.path().join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.expect("create claude dir");
        tokio::fs::write(claude_dir.join(".credentials.json"), r#"{"claudeAiOauth":{"expiresAt":0,"refreshTokenExpiresAt":-1}}"#)
            .await
            .expect("write credentials file");
        let store = store_with_env(BTreeMap::from([("HOME".to_string(), home.path().to_string_lossy().into_owned())]));

        assert_eq!(store.credential_expiry().await, BTreeMap::new());
    }

    #[tokio::test]
    async fn ambient_claude_expiry_probe_preserves_live_metadata_alongside_a_sentinel() {
        let home = tempfile::tempdir().expect("home dir");
        let claude_dir = home.path().join(".claude");
        tokio::fs::create_dir_all(&claude_dir).await.expect("create claude dir");
        let refresh_expires_at_ms: i64 = 1_756_000_000_000;
        let credentials = format!(r#"{{"claudeAiOauth":{{"expiresAt":0,"refreshTokenExpiresAt":{refresh_expires_at_ms}}}}}"#);
        tokio::fs::write(claude_dir.join(".credentials.json"), credentials).await.expect("write credentials file");
        let store = store_with_env(BTreeMap::from([("HOME".to_string(), home.path().to_string_lossy().into_owned())]));

        let expiry = store.credential_expiry().await;

        let ambient = expiry.get(AMBIENT_CLAUDE_CREDENTIAL_SCOPE).expect("ambient claude entry");
        assert_eq!(ambient.expires_at, None);
        assert_eq!(ambient.refresh_expires_at, DateTime::from_timestamp_millis(refresh_expires_at_ms));
    }

    #[tokio::test]
    async fn github_app_resolves_caches_and_invalidates_installation_ids_through_replayed_http() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("GitHub repository spec");
        let repository_key = repository_spec.key();
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("flotilla".to_string()).build(), &repository_spec)
            .await
            .expect("create repository");
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: Some("configured-crew[bot]".to_string()),
                        installation_id: None,
                        installation_repository: Some("flotilla-org/flotilla".to_string()),
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: app_id_path.to_string_lossy().into_owned(),
                        private_key_path: private_key_path.to_string_lossy().into_owned(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let fixture = r#"
interactions:
  - channel: http
    method: GET
    url: "https://api.github.com/repos/flotilla-org/flotilla/installation"
    status: 200
    response_body: '{"id":9876}'
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["flotilla"]}'
    status: 201
    response_body: '{"token":"one","expires_at":"2026-08-03T17:00:00Z"}'
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["flotilla"]}'
    status: 404
    response_body: '{}'
  - channel: http
    method: GET
    url: "https://api.github.com/repos/flotilla-org/flotilla/installation"
    status: 200
    response_body: '{"id":9999}'
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9999/access_tokens"
    request_body: '{"repositories":["flotilla"]}'
    status: 201
    response_body: '{"token":"two","expires_at":"2026-08-03T18:00:00Z"}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_http(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            Arc::new(ReplayHttpClient::new(session.clone())),
            state.path().to_path_buf(),
        );
        let refs = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]);
        let delivered = store.prepare_scoped("env-a", &refs, &scopes, runner.clone()).await.expect("first preparation");
        assert!(delivered.contains(&("PR_SHEPHERD_AS".to_string(), "configured-crew[bot]".to_string())));
        store.prepare_scoped("env-a", &refs, &scopes, runner).await.expect("second preparation after invalidation");
        session.assert_complete();
    }

    #[tokio::test]
    async fn github_app_installation_resolution_error_names_the_declared_repository() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");
        let session = Session::replaying_from_str(
            r#"interactions:
  - channel: http
    method: GET
    url: "https://api.github.com/repos/example/missing/installation"
    status: 404
    response_body: '{}'
"#,
            Masks::new(),
        );
        let minter = RealGithubAppTokenMinter {
            env: Arc::new(TestEnv::default()),
            http: Arc::new(ReplayHttpClient::new(session.clone())),
            clock: Arc::new(SystemClock),
        };
        let error = minter
            .resolve_installation(&GithubAppInstallationRequest {
                repository: "example/missing".to_string(),
                app_id_path: app_id_path.to_string_lossy().into_owned(),
                private_key_path: private_key_path.to_string_lossy().into_owned(),
            })
            .await
            .expect_err("missing installation must fail");
        assert!(error.contains("example/missing"), "error must name the repository: {error}");
        session.assert_complete();
    }

    #[tokio::test]
    async fn github_app_host_direct_and_contained_delivery_match_and_no_grant_stays_empty() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");

        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("GitHub repository spec");
        let repository_key = repository_spec.key();
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("flotilla".to_string()).build(), &repository_spec)
            .await
            .expect("create repository");
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: app_id_path.to_string_lossy().into_owned(),
                        private_key_path: private_key_path.to_string_lossy().into_owned(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");

        let fixture = r#"
interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_headers:
      accept: "application/vnd.github+json"
      x-github-api-version: "2022-11-28"
    request_body: '{"repositories":["flotilla"]}'
    status: 201
    response_body: '{"token":"installation-token-one","expires_at":"2026-08-03T17:00:00Z"}'
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_headers:
      accept: "application/vnd.github+json"
      x-github-api-version: "2022-11-28"
    request_body: '{"repositories":["flotilla"]}'
    status: 201
    response_body: '{"token":"installation-token-two","expires_at":"2026-08-03T18:00:00Z"}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let http = Arc::new(ReplayHttpClient::new(session.clone()));
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_http(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            http,
            state.path().to_path_buf(),
        );
        let refs = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]);
        let first = store.prepare_scoped("host-direct-kiwi", &refs, &scopes, runner.clone()).await.expect("host-direct preparation");
        let second = store.prepare_scoped("contained-crew", &refs, &scopes, runner.clone()).await.expect("contained preparation");
        let no_grant = store
            .prepare_scoped("host-direct-without-grant", &BTreeSet::new(), &BTreeMap::new(), runner.clone())
            .await
            .expect("ungranted crew preparation");

        let first = first.into_iter().collect::<BTreeMap<_, _>>();
        let second = second.into_iter().collect::<BTreeMap<_, _>>();
        assert!(!first.contains_key("GH_TOKEN") && !second.contains_key("GH_TOKEN"));
        assert_eq!(first, second, "both placements must use the same credential adapter output");
        assert!(no_grant.is_empty(), "an ungranted host-direct crew must receive no credential environment");
        assert!(first["GITHUB_TOKEN_FILE"].ends_with("/credentials/github-app/token"));
        assert!(first["PATH"].contains("/credentials/github-app:"), "gh wrapper directory must be on PATH");
        assert!(first["GIT_CONFIG_GLOBAL"].ends_with("/credentials/gitconfig"));
        assert_eq!(first["GIT_TERMINAL_PROMPT"], "0");
        let writes = runner.writes.lock().expect("writes lock");
        assert!(writes.iter().any(|(_, contents)| contents.contains("installation-token-one")));
        assert!(writes.iter().any(|(_, contents)| contents.contains("installation-token-two")));
        assert!(writes
            .iter()
            .any(|(path, contents)| path.ends_with("credentials/github-app/gh") && contents.contains("GITHUB_TOKEN_FILE")));
        assert!(writes
            .iter()
            .any(|(path, contents)| path.ends_with("credentials/github-app/git-credential-github-app")
                && contents.contains("GITHUB_TOKEN_FILE")));
        assert!(writes.iter().any(|(path, contents)| path.ends_with("credentials/gitconfig") && contents.contains("flotilla-crew")));
        drop(writes);
        let calls = runner.calls.lock().expect("calls lock");
        assert_eq!(
            calls
                .iter()
                .filter(|(command, args, _)| {
                    command == "sh" && args.iter().any(|arg| arg.contains("api installation/repositories --silent"))
                })
                .count(),
            2,
        );
        session.assert_complete();
    }

    #[tokio::test]
    async fn skill_source_credential_is_one_shot_and_narrowed_to_its_repository() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-skills-fork".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: Some(BTreeMap::from([("contents".to_string(), "read".to_string())])),
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: app_id_path.to_string_lossy().into_owned(),
                        private_key_path: private_key_path.to_string_lossy().into_owned(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create skill credential declaration");
        let fixture = r#"
interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["mattpocock-skills"],"permissions":{"contents":"read"}}'
    status: 201
    response_body: '{"token":"skill-token","expires_at":"2026-08-03T17:00:00Z"}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_http(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            Arc::new(ReplayHttpClient::new(session.clone())),
            state.path().to_path_buf(),
        );

        let token_file = store
            .prepare_skill_source("github-skills-fork", "https://github.com/flotilla-org/mattpocock-skills.git", &*runner)
            .await
            .expect("mint narrowed skill-source credential");

        assert!(token_file.parent().expect("token parent").ends_with("skill-sources/github-skills-fork"));
        assert!(runner.writes.lock().expect("writes lock").iter().any(|(path, contents)| path == &token_file && contents == "skill-token"));
        assert!(store.github_app_deliveries.lock().await.is_empty(), "one-shot skill tokens must not enter refresh registrations");
        session.assert_complete();
    }

    #[tokio::test]
    async fn old_skill_source_tokens_are_reaped_without_touching_a_live_staging_token() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("skill-sources/source");
        std::fs::create_dir_all(&root).expect("token directory");
        let abandoned = root.join("token-abandoned");
        let killed_stage = root.join("token-killed-stage");
        let active = root.join("token-active");
        let recent = root.join("token-recent");
        for path in [&abandoned, &killed_stage, &active, &recent] {
            std::fs::write(path, "test-token").expect("write token");
        }
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 60 * 60);
        for path in [&abandoned, &active] {
            std::fs::File::open(path).expect("open token").set_modified(old).expect("age token");
        }
        std::fs::write(format!("{}.in-use.{}", active.display(), std::process::id()), "").expect("live marker");
        std::fs::write(format!("{}.in-use.999999999", killed_stage.display()), "").expect("dead marker");
        let runner = PathCommandRunner { bin_dir: temp.path().to_path_buf() };
        prune_abandoned_skill_source_tokens(&runner, &temp.path().join("skill-sources")).await.expect("prune tokens");
        assert!(!abandoned.exists(), "old abandoned token is removed");
        assert!(!killed_stage.exists(), "token from a killed staging shell is removed on the next pass");
        assert!(active.exists(), "old token held by a live staging shell is retained");
        assert!(recent.exists(), "recent minted token is retained");
    }

    #[tokio::test]
    async fn github_app_sends_configured_permissions_and_surfaces_downscope_refusal_detail() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");

        let fixture = r#"
interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_headers:
      accept: "application/vnd.github+json"
      x-github-api-version: "2022-11-28"
    request_body: '{"repositories":["flotilla"],"permissions":{"contents":"write"}}'
    status: 422
    response_body: '{"message":"The permissions requested are not granted to this installation."}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let minter = RealGithubAppTokenMinter {
            env: Arc::new(TestEnv::default()),
            http: Arc::new(ReplayHttpClient::new(session.clone())),
            clock: Arc::new(VirtualClock::new(now)),
        };
        let result = minter
            .mint(&GithubAppMintRequest {
                installation_id: 9876,
                app_id_path: app_id_path.to_string_lossy().into_owned(),
                private_key_path: private_key_path.to_string_lossy().into_owned(),
                repositories: vec!["flotilla".to_string()],
                permissions: Some(BTreeMap::from([("contents".to_string(), "write".to_string())])),
            })
            .await;
        let Err(error) = result else {
            panic!("unsupported downscope must fail");
        };

        let error = error.to_string();
        assert!(error.contains("HTTP 422 Unprocessable Entity"), "unexpected mint error: {error}");
        assert!(error.contains("permissions requested are not granted"), "GitHub response detail missing: {error}");
        session.assert_complete();
    }

    #[tokio::test]
    async fn github_app_classifies_http_500_mint_as_transient() {
        let state = tempfile::tempdir().expect("state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("App key");
        let fixture = r#"
interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["flotilla"]}'
    status: 500
    response_body: '{"message":"Internal Server Error"}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let minter = RealGithubAppTokenMinter {
            env: Arc::new(TestEnv::default()),
            http: Arc::new(ReplayHttpClient::new(session.clone())),
            clock: Arc::new(SystemClock),
        };
        let error = minter
            .mint(&GithubAppMintRequest {
                installation_id: 9876,
                app_id_path: app_id_path.to_string_lossy().into_owned(),
                private_key_path: private_key_path.to_string_lossy().into_owned(),
                repositories: vec!["flotilla".to_string()],
                permissions: None,
            })
            .await
            .expect_err("HTTP 500 must be retryable");
        assert!(matches!(error, GithubAppMintError::Transient(_)), "unexpected mint error: {error}");
        session.assert_complete();
    }

    #[tokio::test]
    async fn github_app_fixed_installation_id_preserves_404_response_detail() {
        let state = tempfile::tempdir().expect("create state directory");
        let app_id_path = state.path().join("github-app.id");
        let private_key_path = state.path().join("github-app.pem");
        tokio::fs::write(&app_id_path, "12345\n").await.expect("write App id");
        tokio::fs::write(&private_key_path, include_str!("fixtures/github_app_test.pem")).await.expect("write App private key");
        let fixture = r#"
interactions:
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_body: '{"repositories":["flotilla"]}'
    status: 404
    response_body: '{"message":"installation was removed"}'
"#;
        let session = Session::replaying_from_str(fixture, Masks::new());
        let minter = RealGithubAppTokenMinter {
            env: Arc::new(TestEnv::default()),
            http: Arc::new(ReplayHttpClient::new(session.clone())),
            clock: Arc::new(SystemClock),
        };
        let result = minter
            .mint(&GithubAppMintRequest {
                installation_id: 9876,
                app_id_path: app_id_path.to_string_lossy().into_owned(),
                private_key_path: private_key_path.to_string_lossy().into_owned(),
                repositories: vec!["flotilla".to_string()],
                permissions: None,
            })
            .await;
        let Err(error) = result else {
            panic!("removed fixed installation must fail");
        };
        let error = error.to_string();

        assert!(error.contains("HTTP 404 Not Found"), "unexpected mint error: {error}");
        assert!(error.contains("installation was removed"), "GitHub response detail missing: {error}");
        session.assert_complete();
    }

    #[tokio::test]
    async fn github_app_delivery_uses_replicated_scope_rebuilds_after_store_restart_and_rotates() {
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let clock = Arc::new(VirtualClock::new(now));
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Ok(GithubAppToken { permissions: None, value: "installation-token-one".to_string(), expires_at: now + Duration::hours(1) }),
                Err("temporary adoption outage one".to_string()),
                Err("temporary adoption outage two".to_string()),
                Err("persistent adoption outage".to_string()),
                Ok(GithubAppToken { permissions: None, value: "installation-token-two".to_string(), expires_at: now + Duration::hours(2) }),
                Err("temporary outage one".to_string()),
                Err("temporary outage two".to_string()),
                Err("persistent outage".to_string()),
                Ok(GithubAppToken {
                    permissions: None,
                    value: "installation-token-three".to_string(),
                    expires_at: now + Duration::hours(3),
                }),
                Ok(GithubAppToken {
                    permissions: None,
                    value: "installation-token-four".to_string(),
                    expires_at: now + Duration::hours(4),
                }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let repository_root = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("repository-root"));
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("minting-root"));
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("GitHub repository spec");
        let repository_key = repository_spec.key();
        repository_root
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("flotilla".to_string()).build(), &repository_spec)
            .await
            .expect("create repository");
        let repositories = repository_root.using::<Repository>("flotilla").list().await.expect("list repository source");
        backend
            .replica_writer::<Repository>(NodeId::new("repository-root"), "flotilla")
            .replace(&repositories, Utc::now())
            .await
            .expect("replicate repository to minting root");
        assert!(backend.using::<Repository>("flotilla").list().await.expect("list local repositories").items.is_empty());
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: "/host-only/github-app.id".to_string(),
                        private_key_path: "/host-only/github-app.pem".to_string(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new_with_github_app_minter(
            backend.clone(),
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        let refs = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]);

        assert_eq!(
            store.refresh_failure_message(
                "github-app",
                now + Duration::seconds(45),
                "credential `github-app` adapter `github-app`: invalid scalar",
            ),
            "credential `github-app` adapter `github-app`: invalid scalar; expires in 1 minute"
        );
        let environment = store
            .prepare_scoped("standing-vessel", &refs, &scopes, runner.clone())
            .await
            .expect("prepare standing vessel")
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        assert!(!environment.contains_key("GH_TOKEN"), "the installation token must not be baked into the vessel environment");
        let token_file = environment.get("GITHUB_TOKEN_FILE").expect("token file environment variable");
        assert!(token_file.ends_with("/credentials/github-app/token"));
        assert_eq!(environment.get("PATH"), Some(&"/state/credentials/github-app:/usr/bin:/bin".to_string()));
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 1);

        drop(store);
        let store = CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        );
        for expected_surface in [false, false, true] {
            let error =
                store.adopt_github_app_deliveries("standing-vessel", &refs, &scopes, runner.clone()).await.expect_err("adoption outage");
            assert_eq!(error.should_surface, expected_surface);
        }
        store.adopt_github_app_deliveries("standing-vessel", &refs, &scopes, runner.clone()).await.expect("re-adopt standing vessel");
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 5, "startup adoption retries and rebuilds the registration");

        clock.advance(Duration::minutes(60));
        let first_failure = store.refresh_due_github_app_tokens().await;
        assert_eq!(first_failure.len(), 1);
        assert!(!first_failure[0].should_surface, "one transient failure must remain retryable");
        clock.advance(Duration::seconds(30));
        let second_failure = store.refresh_due_github_app_tokens().await;
        assert!(!second_failure[0].should_surface, "two transient failures must remain retryable");
        clock.advance(Duration::minutes(1));
        let third_failure = store.refresh_due_github_app_tokens().await;
        assert!(third_failure[0].should_surface, "a repeated unrefreshable delivery must become visible");
        clock.advance(Duration::minutes(2));
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 9, "recovered material keeps retrying and eventually rotates");
        let token_writes = runner
            .writes
            .lock()
            .expect("writes lock")
            .iter()
            .filter(|(path, _)| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("token")))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(token_writes.len(), 3);
        assert!(token_writes[0].1.contains("installation-token-one"));
        assert!(token_writes[1].1.contains("installation-token-two"));
        assert!(token_writes[2].1.contains("installation-token-three"));
        assert_eq!(token_writes[0].0, token_writes[2].0, "runner atomically replaces the token at its stable path");
        assert!(runner
            .protected_writes
            .lock()
            .expect("protected writes lock")
            .iter()
            .any(|(path, mode)| { path == &token_writes[2].0 && *mode == 0o600 }));
        {
            let writes = runner.writes.lock().expect("writes lock");
            let gh_wrapper = writes.iter().find(|(path, _)| path.file_name().is_some_and(|name| name == "gh")).expect("gh wrapper");
            assert!(gh_wrapper.1.contains("cat \"$GITHUB_TOKEN_FILE\""));
            let git_helper =
                writes.iter().find(|(path, _)| path.ends_with("git-credential-github-app")).expect("GitHub App Git credential helper");
            assert!(git_helper.1.contains("cat \"$GITHUB_TOKEN_FILE\""));
        }
        {
            let calls = runner.calls.lock().expect("calls lock");
            assert!(calls.iter().any(|(command, args, _)| {
                command == "sh" && args.iter().any(|arg| arg.contains("GITHUB_TOKEN_FILE=\"$1\" \"$2\" api installation/repositories"))
            }));
            assert!(calls
                .iter()
                .any(|(command, args, _)| { command == "sh" && args.iter().any(|arg| arg.contains("git credential fill")) }));
        }

        store
            .reconcile_work_delivery("standing-vessel", &refs, &BTreeSet::new(), &scopes, runner.clone())
            .await
            .expect("settled work revokes the installation delivery");
        assert!(store.github_app_deliveries.lock().await.is_empty());
        store
            .adopt_github_app_deliveries("standing-vessel", &refs, &scopes, runner.clone())
            .await
            .expect("reactivated work mints a fresh installation token");
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 10);

        let missing_store = CredentialStore::new_with_github_app_minter(
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("missing-root")),
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock, minter },
            PathBuf::from("/state"),
        );
        for expected_surface in [false, false, true] {
            let error = missing_store
                .adopt_github_app_deliveries("missing-github-app", &refs, &scopes, runner.clone())
                .await
                .expect_err("missing scoped GitHub App declaration must remain visible");
            assert_eq!(error.should_surface, expected_surface);
        }
    }

    #[tokio::test]
    async fn stale_in_flight_refresh_cannot_overwrite_a_reprepared_delivery() {
        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let clock = Arc::new(VirtualClock::new(now));
        let minter = Arc::new(BlockingGithubAppTokenMinter {
            now,
            calls: AtomicUsize::new(0),
            refresh_started: tokio::sync::Notify::new(),
            release_refresh: tokio::sync::Notify::new(),
        });
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("GitHub repository spec");
        let repository_key = repository_spec.key();
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("flotilla".to_string()).build(), &repository_spec)
            .await
            .expect("create repository");
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: "/host-only/github-app.id".to_string(),
                        private_key_path: "/host-only/github-app.pem".to_string(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = Arc::new(CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: clock.clone(), minter: minter.clone() },
            PathBuf::from("/state"),
        ));
        let refs = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]);
        store.prepare_scoped("standing-vessel", &refs, &scopes, runner.clone()).await.expect("initial preparation");
        clock.advance(Duration::minutes(55));

        let refresh_store = Arc::clone(&store);
        let refresh = tokio::spawn(async move { refresh_store.refresh_due_github_app_tokens().await });
        minter.refresh_started.notified().await;
        store.prepare_scoped("standing-vessel", &refs, &scopes, runner.clone()).await.expect("replacement preparation");
        minter.release_refresh.notify_one();
        assert!(refresh.await.expect("refresh task").is_empty());

        let token_writes =
            runner.writes.lock().expect("writes lock").iter().filter(|(path, _)| path.ends_with("token")).cloned().collect::<Vec<_>>();
        assert_eq!(token_writes.iter().map(|(_, token)| token.as_str()).collect::<Vec<_>>(), ["initial-token", "reprepared-token"]);
    }

    #[tokio::test]
    async fn slow_github_app_preflight_does_not_delay_an_independent_environment() {
        struct PausedPreflightRunner {
            inner: RecordingRunner,
            first_preflight: AtomicUsize,
            started: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }

        #[async_trait]
        impl CommandRunner for PausedPreflightRunner {
            async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
                if args.iter().any(|arg| arg.contains("api installation/repositories"))
                    && self.first_preflight.fetch_add(1, Ordering::SeqCst) == 0
                {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                self.inner.run(cmd, args, cwd, label).await
            }

            async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
                self.inner.run_output(cmd, args, cwd, label).await
            }

            async fn run_with_input(
                &self,
                cmd: &str,
                args: &[&str],
                cwd: &Path,
                label: &ChannelLabel,
                input: &[u8],
            ) -> Result<String, String> {
                self.inner.run_with_input(cmd, args, cwd, label, input).await
            }

            async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
                self.inner.exists(cmd, args).await
            }

            async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
                self.inner.write_file(path, content).await
            }

            async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
                self.inner.write_file_with_mode(path, content, mode).await
            }
        }

        let now: DateTime<Utc> = "2026-08-03T16:00:00Z".parse().expect("test timestamp");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let repository_spec = RepositorySpec::remote("https://github.com/flotilla-org/flotilla").expect("repository spec");
        let repository_key = repository_spec.key();
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("flotilla".to_string()).build(), &repository_spec)
            .await
            .expect("repository");
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-app".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GithubApp {
                        actor_login: None,
                        installation_id: Some(9876),
                        installation_repository: None,
                        permissions: None,
                    },
                    source: CredentialSource::GithubApp {
                        app_id_path: "/host/app.id".to_string(),
                        private_key_path: "/host/app.pem".to_string(),
                    },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("credential");
        let minter = Arc::new(FakeGithubAppTokenMinter {
            tokens: StdMutex::new(VecDeque::from([
                Ok(GithubAppToken { permissions: None, value: "first-token".to_string(), expires_at: now + Duration::hours(1) }),
                Ok(GithubAppToken { permissions: None, value: "second-token".to_string(), expires_at: now + Duration::hours(1) }),
            ])),
            requests: StdMutex::new(Vec::new()),
        });
        let runner = Arc::new(PausedPreflightRunner {
            inner: RecordingRunner::default(),
            first_preflight: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let store = Arc::new(CredentialStore::new_with_github_app_minter(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner.clone(),
            GithubAppMinting { clock: Arc::new(VirtualClock::new(now)), minter },
            PathBuf::from("/state"),
        ));
        let refs = BTreeSet::from(["github-app".to_string()]);
        let scopes = BTreeMap::from([("github-app".to_string(), BTreeSet::from([repository_key]))]);
        let first_store = Arc::clone(&store);
        let first_runner = Arc::clone(&runner);
        let first_refs = refs.clone();
        let first_scopes = scopes.clone();
        let first = tokio::spawn(async move { first_store.prepare_scoped("env-a", &first_refs, &first_scopes, first_runner).await });
        runner.started.notified().await;
        let second =
            tokio::time::timeout(std::time::Duration::from_secs(1), store.prepare_scoped("env-b", &refs, &scopes, runner.clone())).await;
        runner.release.notify_one();
        first.await.expect("first task").expect("first delivery");
        second.expect("independent delivery must finish during another preflight").expect("second delivery");
    }

    #[tokio::test]
    async fn github_app_repository_scope_rejections_fail_before_minting() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let non_github_spec = RepositorySpec::remote("https://gitlab.example/flotilla-org/flotilla").expect("non-GitHub repository spec");
        let non_github_key = non_github_spec.key();
        backend
            .clone()
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name("non-github".to_string()).build(), &non_github_spec)
            .await
            .expect("create non-GitHub repository");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            runner,
            PathBuf::from("/tmp/flotilla-test-state"),
        );
        let spec = CredentialSpecSpec {
            consumer: CredentialConsumer::GithubApp {
                actor_login: None,
                installation_id: Some(9876),
                installation_repository: None,
                permissions: None,
            },
            source: CredentialSource::GithubApp {
                app_id_path: "/not-read/github-app.id".to_string(),
                private_key_path: "/not-read/github-app.pem".to_string(),
            },
            lifecycle: CredentialLifecycle::Refreshable,
            placement: CredentialPlacementRequirements::default(),
        };

        for scope in [None, Some(&BTreeSet::new())] {
            let error = store.resolve_for_adapter("github-app", &spec, scope, None).await.expect_err("empty scopes must fail");
            assert!(error.contains("empty repository scope"), "unexpected error: {error}");
        }
        let missing_key = RepositoryKey("missing-repository".to_string());
        let error = store.github_repository_names(&BTreeSet::from([missing_key])).await.expect_err("missing repository must fail");
        assert!(error.contains("missing repository"), "unexpected error: {error}");
        let error =
            store.github_repository_names(&BTreeSet::from([non_github_key])).await.expect_err("scope without GitHub repository must fail");
        assert!(error.contains("empty GitHub repository scope"), "unexpected error: {error}");
        let oversized_scope = (0..501).map(|index| RepositoryKey(format!("repository-{index}"))).collect();
        let error = store.github_repository_names(&oversized_scope).await.expect_err("oversized repository scope must fail");
        assert!(error.contains("missing repository"), "unexpected error: {error}");
        let mut static_spec = spec;
        static_spec.lifecycle = CredentialLifecycle::Static;
        let non_empty_scope = BTreeSet::from([RepositoryKey("not-resolved".to_string())]);
        let error = store
            .resolve_for_adapter("github-app", &static_spec, Some(&non_empty_scope), None)
            .await
            .expect_err("non-refreshable GitHub App credentials must fail");
        assert!(error.contains("must use the refreshable lifecycle"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn mixed_forge_scope_selects_only_github_repositories_for_app() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let github = ForgeSpec::builder()
            .forge_id("github".to_string())
            .kind(ForgeKind::Github)
            .hosts(BTreeSet::from(["github.com".to_string()]))
            .https_url("https://github.com".to_string())
            .git_ssh_host("github.com".to_string())
            .build();
        let forgejo = ForgeSpec::builder()
            .forge_id("lab".to_string())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["lab-alias".to_string(), "forgejo.lab".to_string()]))
            .https_url("https://forgejo.lab".to_string())
            .git_ssh_host("lab-alias".to_string())
            .build();
        for forge in [&github, &forgejo] {
            backend
                .definitions::<Forge>("flotilla")
                .create(&InputMeta::builder().name(forge.forge_id.clone()).build(), forge)
                .await
                .expect("create Forge");
        }
        let github_repo = RepositorySpec::remote("https://github.com/rjwittams/ghostty")
            .expect("GitHub repository")
            .on_forge(&github)
            .expect("resolve GitHub Forge");
        let forgejo_repo = RepositorySpec::remote("https://forgejo.lab/ghostty-ops/work")
            .expect("Forgejo repository")
            .on_forge(&forgejo)
            .expect("resolve Forgejo Forge");
        for (name, repository) in [("github-repo", &github_repo), ("forgejo-repo", &forgejo_repo)] {
            backend
                .using::<Repository>("flotilla")
                .create(&InputMeta::builder().name(name.to_string()).build(), repository)
                .await
                .expect("create repository");
        }
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            PathBuf::from("/tmp/flotilla-test-state"),
        );
        let scope = BTreeSet::from([github_repo.key(), forgejo_repo.key()]);
        assert_eq!(store.github_repository_names(&scope).await.expect("partition mixed scope"), ["ghostty"]);
    }

    #[tokio::test]
    async fn codex_material_is_transformed_by_stdin_login_and_never_passed_through_as_env() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("model-api".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Codex,
                    source: CredentialSource::Env { name: "TEST_OPENAI_KEY".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let secret = "test-secret-never-in-argv";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_OPENAI_KEY".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("codex", "/usr/bin/codex"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["model-api".to_string()]);

        assert_eq!(store.vessel_config_fragments(&credential_refs, &BTreeMap::new()).await.expect("Codex fragment").len(), 1);
        assert!(store
            .vessel_config_fragments(&credential_refs, &BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]),)
            .await
            .expect("explicit Codex home")
            .is_empty());

        let delivered = store.prepare("env-a", &credential_refs, runner.clone()).await.expect("prepare codex credential");

        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "CODEX_HOME");
        assert_eq!(delivered[0].1, "/tmp/flotilla-test-state/credentials/model-api/codex");
        assert!(!delivered.iter().any(|(name, value)| name == "OPENAI_API_KEY" || value == secret));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg == "CODEX_HOME=\"$1\" codex login --with-api-key") && input == secret.as_bytes()
        }));
        assert!(calls.iter().any(|(cmd, args, _)| cmd == "sh" && args.iter().any(|arg| arg.contains("codex login status"))));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.starts_with("/run/flotilla")));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
    }

    async fn create_claude_oauth_spec(backend: &ResourceBackend, name: &str, account_email: &str, source_env: &str) {
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::ClaudeOauth { account_email: account_email.to_string() },
                    source: CredentialSource::Env { name: source_env.to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
    }

    #[tokio::test]
    async fn claude_oauth_material_is_delivered_as_an_env_token_without_owning_agent_config() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let secret = "sk-ant-oat01-test-token-never-in-argv";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_CLAUDE_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["claude-max".to_string()]);

        assert!(
            store.vessel_config_fragments(&credential_refs, &BTreeMap::new()).await.expect("Claude fragments").is_empty(),
            "Claude config belongs to the agent adapter, not credential delivery"
        );

        assert!(
            store.prepare("env-without-grant", &BTreeSet::new(), runner.clone()).await.expect("prepare without a grant").is_empty(),
            "an ungranted session must receive no credential environment"
        );
        let delivered = store.prepare("env-a", &credential_refs, runner.clone()).await.expect("prepare claude-oauth credential");

        assert_eq!(delivered.len(), 2);
        assert!(
            delivered.iter().any(|(name, value)| name == "CLAUDE_CODE_OAUTH_TOKEN" && value == secret),
            "the OAuth token must be delivered under CLAUDE_CODE_OAUTH_TOKEN"
        );
        assert!(delivered
            .iter()
            .any(|(name, value)| { name == "CLAUDE_CONFIG_DIR" && value == "/tmp/flotilla-test-state/credentials/claude-max/claude" }));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight"]));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh"
                && args.iter().any(|arg| arg.contains("unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN"))
                && args.iter().any(|arg| arg.contains("CLAUDE_CODE_OAUTH_TOKEN=\"$token\"") && arg.contains("claude -p"))
                && args.iter().any(|arg| arg.contains("claude -p ok 2>&1") && arg.contains("printf '%s\\n' \"$output\" >&2"))
                && args.iter().any(|arg| arg == "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight")
                && input == secret.as_bytes()
        }));
        assert!(
            calls
                .iter()
                .any(|(cmd, args, _)| cmd == "rm" && args == &["-rf", "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight"]),
            "the preflight scratch directory must not outlive the probe"
        );
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
    }

    #[tokio::test]
    async fn claude_oauth_preflight_scratch_dir_prefers_the_user_runtime_dir_over_absolute_run() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_CLAUDE_TOKEN".to_string(), "sk-ant-oat01-test-token".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        store.prepare("env-a", &BTreeSet::from(["claude-max".to_string()]), runner.clone()).await.expect("prepare claude-oauth credential");

        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/run/user/1000/flotilla/credentials/claude-max/claude-preflight"]));
        assert!(calls.iter().any(|(cmd, args, _)| {
            cmd == "sh" && args.iter().any(|arg| arg == "/run/user/1000/flotilla/credentials/claude-max/claude-preflight")
        }));
        assert!(
            calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.starts_with("/run/flotilla")),
            "the preflight must never touch root-owned /run/flotilla on the host"
        );
    }

    #[tokio::test]
    async fn a_blank_user_runtime_dir_falls_back_to_the_daemon_state_dir() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_CLAUDE_TOKEN".to_string(), "sk-ant-oat01-test-token".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "   ".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        store.prepare("env-a", &BTreeSet::from(["claude-max".to_string()]), runner.clone()).await.expect("prepare claude-oauth credential");

        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight"]));
    }

    #[tokio::test]
    async fn a_dangling_user_runtime_dir_falls_back_to_the_daemon_state_dir() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_CLAUDE_TOKEN".to_string(), "sk-ant-oat01-test-token".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000-removed".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::with_runtime_dir_checks([false, false]));
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        store.prepare("env-a", &BTreeSet::from(["claude-max".to_string()]), runner.clone()).await.expect("prepare claude-oauth credential");

        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight"]));
        assert!(calls.iter().all(|(cmd, args, _)| {
            cmd != "mkdir" || args != &["-p", "/run/user/1000-removed/flotilla/credentials/claude-max/claude-preflight"]
        }));
    }

    #[tokio::test]
    async fn user_runtime_dir_is_rechecked_for_each_claude_oauth_preflight() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_CLAUDE_TOKEN".to_string(), "sk-ant-oat01-test-token".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::with_runtime_dir_checks([true, true, false, false]));
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["claude-max".to_string()]);

        store.prepare("env-a", &credential_refs, runner.clone()).await.expect("first preflight");
        store.prepare("env-b", &credential_refs, runner.clone()).await.expect("second preflight");

        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/run/user/1000/flotilla/credentials/claude-max/claude-preflight"]));
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "mkdir" && args == &["-p", "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight"]));
    }

    #[tokio::test]
    async fn two_claude_accounts_coexist_across_environments_but_never_share_one() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-alice", "alice@example.com", "TEST_CLAUDE_TOKEN_A").await;
        create_claude_oauth_spec(&backend, "claude-bob", "bob@example.com", "TEST_CLAUDE_TOKEN_B").await;
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("claude-api".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Claude,
                    source: CredentialSource::Env { name: "TEST_ANTHROPIC_KEY".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create API-key credential declaration");
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_CLAUDE_TOKEN_A".to_string(), "token-alice".to_string()),
            ("TEST_CLAUDE_TOKEN_B".to_string(), "token-bob".to_string()),
            ("TEST_ANTHROPIC_KEY".to_string(), "api-key".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let alice: BTreeMap<String, String> = store
            .prepare("env-a", &BTreeSet::from(["claude-alice".to_string()]), runner.clone())
            .await
            .expect("prepare alice")
            .into_iter()
            .collect();
        let bob: BTreeMap<String, String> = store
            .prepare("env-b", &BTreeSet::from(["claude-bob".to_string()]), runner.clone())
            .await
            .expect("prepare bob")
            .into_iter()
            .collect();

        assert_eq!(alice.get("CLAUDE_CODE_OAUTH_TOKEN"), Some(&"token-alice".to_string()));
        assert_eq!(bob.get("CLAUDE_CODE_OAUTH_TOKEN"), Some(&"token-bob".to_string()));
        assert_eq!(alice.get("CLAUDE_CONFIG_DIR"), Some(&"/tmp/flotilla-test-state/credentials/claude-alice/claude".to_string()));
        assert_eq!(bob.get("CLAUDE_CONFIG_DIR"), Some(&"/tmp/flotilla-test-state/credentials/claude-bob/claude".to_string()));

        let error = store
            .prepare("env-c", &BTreeSet::from(["claude-alice".to_string(), "claude-bob".to_string()]), runner.clone())
            .await
            .expect_err("two OAuth credentials in one environment would clobber CLAUDE_CODE_OAUTH_TOKEN");
        assert!(error.contains("multiple granted credentials use this adapter"), "unexpected error: {error}");
        let error = store
            .prepare("env-d", &BTreeSet::from(["claude-alice".to_string(), "claude-api".to_string()]), runner.clone())
            .await
            .expect_err("ANTHROPIC_API_KEY outranks CLAUDE_CODE_OAUTH_TOKEN, so mixing them would silently drop the OAuth identity");
        assert!(error.contains("multiple granted credentials use this adapter"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn rotating_named_claude_oauth_material_reuses_the_stable_config_directory() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("claude-max".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                    source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Refreshable,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create refreshable Claude credential declaration");
        let env = Arc::new(RotatingTokenEnv(StdMutex::new(VecDeque::from([
            "token-before-rotation".to_string(),
            "token-after-rotation".to_string(),
        ]))));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["claude-max".to_string()]);

        let before: BTreeMap<_, _> =
            store.prepare("env-a", &credential_refs, runner.clone()).await.expect("prepare before rotation").into_iter().collect();
        let after: BTreeMap<_, _> =
            store.prepare("env-a", &credential_refs, runner.clone()).await.expect("prepare after rotation").into_iter().collect();

        assert_eq!(before.get("CLAUDE_CODE_OAUTH_TOKEN"), Some(&"token-before-rotation".to_string()));
        assert_eq!(after.get("CLAUDE_CODE_OAUTH_TOKEN"), Some(&"token-after-rotation".to_string()));
        assert_eq!(before.get("CLAUDE_CONFIG_DIR"), after.get("CLAUDE_CONFIG_DIR"));
        assert_eq!(after.get("CLAUDE_CONFIG_DIR"), Some(&"/tmp/flotilla-test-state/credentials/claude-max/claude".to_string()));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().all(|(command, args, _)| {
            command != "rm" || args == &["-rf".to_string(), "/tmp/flotilla-test-state/credentials/claude-max/claude-preflight".to_string()]
        }));
        assert!(calls
            .iter()
            .flat_map(|(_, args, _)| args)
            .all(|arg| !arg.contains("token-before-rotation") && !arg.contains("token-after-rotation")));
    }

    struct DeadTokenRunner {
        inner: RecordingRunner,
        secret: String,
    }

    #[async_trait]
    impl CommandRunner for DeadTokenRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.inner.run(cmd, args, cwd, label).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.inner.run_output(cmd, args, cwd, label).await
        }

        async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            self.inner.run_with_input(cmd, args, cwd, label, input).await?;
            if args.iter().any(|arg| arg.contains("claude -p")) {
                return Err(format!("Login expired for {} · Please run /login", self.secret));
            }
            Ok(String::new())
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            self.inner.exists(cmd, args).await
        }

        async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
            self.inner.write_file(path, content).await
        }
    }

    #[tokio::test]
    async fn a_dead_claude_oauth_token_fails_preparation_loudly_without_leaking_material() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let secret = "sk-ant-oat01-expired-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_CLAUDE_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(DeadTokenRunner { inner: RecordingRunner::default(), secret: secret.to_string() });
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["claude-max".to_string()]);

        let error = store.prepare("env-a", &credential_refs, runner.clone()).await.expect_err("a dead token must fail preparation");

        assert!(error.contains("credential `claude-max` adapter `claude-oauth`"), "unexpected error: {error}");
        assert!(error.contains("preflight failed"), "unexpected error: {error}");
        assert!(!error.contains(secret), "material must be redacted: {error}");
        assert!(error.contains("[redacted]"), "unexpected error: {error}");

        store.prepare("env-a", &credential_refs, runner.clone()).await.expect_err("a dead token must fail again, not be cached");
        let calls = runner.inner.calls.lock().expect("calls lock");
        assert_eq!(
            calls.iter().filter(|(cmd, args, _)| cmd == "sh" && args.iter().any(|arg| arg.contains("claude -p"))).count(),
            2,
            "failed preparation must not mark the credential prepared"
        );
    }

    struct ExhaustedHeadlessCreditsRunner {
        inner: RecordingRunner,
    }

    #[async_trait]
    impl CommandRunner for ExhaustedHeadlessCreditsRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.inner.run(cmd, args, cwd, label).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.inner.run_output(cmd, args, cwd, label).await
        }

        async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            self.inner.run_with_input(cmd, args, cwd, label, input).await?;
            if args.iter().any(|arg| arg.contains("claude -p")) {
                return Err(
                    "You've reached your monthly Agent SDK credit quota. Add billing credits to continue using Claude Code headlessly."
                        .to_string(),
                );
            }
            Ok(String::new())
        }

        async fn exists(&self, cmd: &str, args: &[&str]) -> bool {
            self.inner.exists(cmd, args).await
        }

        async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
            self.inner.write_file(path, content).await
        }
    }

    #[tokio::test]
    async fn exhausted_headless_credits_do_not_reject_a_valid_claude_oauth_token() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_claude_oauth_spec(&backend, "claude-max", "ops@example.com", "TEST_CLAUDE_TOKEN").await;
        let secret = "sk-ant-oat01-valid-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_CLAUDE_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(ExhaustedHeadlessCreditsRunner { inner: RecordingRunner::default() });
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("claude", "/usr/bin/claude"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));
        let credential_refs = BTreeSet::from(["claude-max".to_string()]);

        let delivered = store
            .prepare("env-a", &credential_refs, runner.clone())
            .await
            .expect("headless credit exhaustion does not invalidate the token used by interactive crews");

        assert!(delivered.contains(&("CLAUDE_CODE_OAUTH_TOKEN".to_string(), secret.to_string())));
        store.prepare("env-a", &credential_refs, runner.clone()).await.expect("successful preparation is cached");
        let calls = runner.inner.calls.lock().expect("calls lock");
        assert_eq!(
            calls.iter().filter(|(cmd, args, _)| cmd == "sh" && args.iter().any(|arg| arg.contains("claude -p"))).count(),
            1,
            "accepted credit exhaustion should mark the credential prepared"
        );
    }

    #[tokio::test]
    async fn held_credentials_are_about_host_local_material_not_vessel_binaries() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("model-api".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Codex,
                    source: CredentialSource::Env { name: "TEST_OPENAI_KEY".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_OPENAI_KEY".to_string(), "host-secret".to_string())])));
        let store = CredentialStore::new(
            backend,
            "flotilla",
            env,
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        assert_eq!(store.held_credentials().await.expect("resolve held credentials"), BTreeSet::from(["model-api".to_string()]));
    }

    #[tokio::test]
    async fn forgetting_an_environment_evicts_material_and_preflight_state() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            PathBuf::from("/tmp/flotilla-test-state"),
        );
        store
            .prepared
            .lock()
            .await
            .extend([("env-a".to_string(), "model-api".to_string()), ("env-b".to_string(), "model-api".to_string())]);
        store.materials.lock().await.extend([
            (("env-a".to_string(), "model-api".to_string()), "secret-a".to_string()),
            (("env-b".to_string(), "model-api".to_string()), "secret-b".to_string()),
        ]);
        for environment_ref in ["env-a", "env-b"] {
            store.git_config_fragments.lock().await.insert(
                environment_ref.to_string(),
                BTreeMap::from([(
                    "github".to_string(),
                    git_credential_fragment("github", "gh", "https://github.com", "!gh auth git-credential"),
                )]),
            );
        }

        store.forget_environment("env-a").await.expect("forget environment");

        assert_eq!(store.prepared.lock().await.clone(), BTreeSet::from([("env-b".to_string(), "model-api".to_string())]));
        assert_eq!(
            store.materials.lock().await.clone(),
            BTreeMap::from([(("env-b".to_string(), "model-api".to_string()), "secret-b".to_string())])
        );
        assert_eq!(store.git_config_fragments.lock().await.keys().cloned().collect::<Vec<_>>(), vec!["env-b".to_string()]);
    }

    #[tokio::test]
    async fn work_delivery_reconciles_running_complete_and_reactivated_turns() {
        let state = tempfile::tempdir().expect("state directory");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github-work".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GitHttpToken { host: "github.com".to_string(), username: "bot".to_string() },
                    source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Issued,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create issued credential");
        let runner = Arc::new(flotilla_core::providers::ProcessCommandRunner);
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(RotatingTokenEnv(StdMutex::new(VecDeque::from(["first-token".to_string(), "second-token".to_string()])))),
            EnvironmentBag::new(),
            runner.clone(),
            state.path().to_path_buf(),
        );
        let granted = BTreeSet::from(["github-work".to_string()]);
        let absent = BTreeSet::new();
        let scopes = BTreeMap::new();
        let git_config = state.path().join("credentials/gitconfig");
        let git_config = git_config.to_string_lossy().into_owned();
        let can_fill_git_credential = || {
            let runner = runner.clone();
            let git_config = git_config.clone();
            async move {
                runner
                    .run(
                        "sh",
                        &[
                            "-c",
                            "export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill",
                            "credential-probe",
                            &git_config,
                        ],
                        Path::new("/"),
                        &ChannelLabel::Default,
                    )
                    .await
                    .is_ok()
            }
        };

        store.reconcile_work_delivery("env-a", &granted, &granted, &scopes, runner.clone()).await.expect("running work mints");
        assert!(can_fill_git_credential().await);

        store.reconcile_work_delivery("env-a", &granted, &absent, &scopes, runner.clone()).await.expect("completed work revokes");
        assert!(!can_fill_git_credential().await);

        store.reconcile_work_delivery("env-a", &granted, &granted, &scopes, runner.clone()).await.expect("reactivated work re-mints");
        assert!(can_fill_git_credential().await);

        store.reconcile_work_delivery("env-a", &granted, &absent, &scopes, runner.clone()).await.expect("settle second turn");
        assert!(!can_fill_git_credential().await);
        let error = store
            .reconcile_work_delivery("env-a", &granted, &granted, &scopes, runner)
            .await
            .expect_err("a missing source must report a mint failure");
        assert!(error.contains("TEST_CLAUDE_TOKEN"), "actual source failure should be reported: {error}");
    }

    #[tokio::test]
    async fn gh_material_authenticates_both_gh_and_git_without_interactive_prompts() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Gh,
                    source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let secret = "github-test-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_GITHUB_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered =
            store.prepare("env-a", &BTreeSet::from(["github".to_string()]), runner.clone()).await.expect("prepare GitHub credential");

        assert_eq!(
            delivered,
            vec![
                ("GH_TOKEN".to_string(), secret.to_string()),
                ("GIT_CONFIG_GLOBAL".to_string(), "/tmp/flotilla-test-state/credentials/gitconfig".to_string()),
                ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ]
        );
        let writes = runner.writes.lock().expect("writes lock");
        assert_eq!(writes.as_slice(), &[(
            PathBuf::from("/tmp/flotilla-test-state/credentials/gitconfig"),
            "# fragment: credential/gh github\n[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n\n# fragment: vessel/crew-identity\n[user]\n\temail = 309902803+flotilla-crew[bot]@users.noreply.github.com\n\n# fragment: vessel/crew-identity\n[user]\n\tname = flotilla-crew[bot]\n".to_string()
        )]);
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg.contains("gh api user --silent")) && input == secret.as_bytes()
        }));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg.contains("GIT_CONFIG_GLOBAL")) && input == secret.as_bytes()
        }));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.starts_with("/run/flotilla")));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
    }

    #[tokio::test]
    async fn github_and_git_http_token_helpers_compose_without_cross_talk() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Gh,
                    source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create GitHub credential declaration");
        create_git_http_token_spec(&backend, "lab-forgejo", "forgejo.lab", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_GITHUB_TOKEN".to_string(), "github-test-token".to_string()),
            ("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
            .with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered: BTreeMap<String, String> = store
            .prepare("env-a", &BTreeSet::from(["github".to_string(), "lab-forgejo".to_string()]), runner.clone())
            .await
            .expect("prepare GitHub and Git HTTP credentials")
            .into_iter()
            .collect();

        assert_eq!(delivered.get("GIT_CONFIG_GLOBAL"), Some(&"/tmp/flotilla-test-state/credentials/gitconfig".to_string()));
        assert_eq!(delivered.get("GIT_TERMINAL_PROMPT"), Some(&"0".to_string()));
        assert!(!delivered
            .keys()
            .any(|key| key == "GIT_CONFIG_COUNT" || key.starts_with("GIT_CONFIG_KEY_") || key.starts_with("GIT_CONFIG_VALUE_")));
        let writes = runner.writes.lock().expect("writes lock");
        let gitconfig = writes
            .iter()
            .rev()
            .find(|(path, _)| path == Path::new("/tmp/flotilla-test-state/credentials/gitconfig"))
            .map(|(_, content)| content)
            .expect("staged shared Git config");
        assert!(gitconfig.contains("[credential \"https://github.com\"]\n\thelper = !gh auth git-credential"));
        assert!(gitconfig.contains(
            "[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-http-token"
        ));
        assert!(gitconfig.contains("# fragment: credential/gh github"));
        assert!(gitconfig.contains("# fragment: credential/git-http-token lab-forgejo"));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, _)| {
            cmd == "sh"
                && args.iter().any(|arg| arg.contains("GIT_CONFIG_GLOBAL"))
                && args.iter().any(|arg| arg.contains("host=github.com"))
        }));
        assert!(calls.iter().any(|(cmd, args, _)| {
            cmd == "sh"
                && args.iter().any(|arg| arg.contains("GIT_CONFIG_GLOBAL"))
                && args.iter().any(|arg| arg.contains("host=%s"))
                && args.iter().any(|arg| arg == "forgejo.lab")
        }));
    }

    #[tokio::test]
    async fn gitconfig_keeps_fragments_from_disjoint_preparations_of_one_environment() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Gh,
                    source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create GitHub credential declaration");
        create_git_http_token_spec(&backend, "lab-forgejo", "forgejo.lab", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([
            ("TEST_GITHUB_TOKEN".to_string(), "github-test-token".to_string()),
            ("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string()),
        ])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new()
            .with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"))
            .with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        store.prepare("env-a", &BTreeSet::from(["github".to_string()]), runner.clone()).await.expect("prepare GitHub credential");
        store.prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone()).await.expect("prepare Git HTTP credential");

        let writes = runner.writes.lock().expect("writes lock");
        let gitconfig = writes
            .iter()
            .rev()
            .find(|(path, _)| path == Path::new("/tmp/flotilla-test-state/credentials/gitconfig"))
            .map(|(_, content)| content)
            .expect("staged shared Git config");
        assert!(gitconfig.contains("[credential \"https://github.com\"]\n\thelper = !gh auth git-credential"));
        assert!(gitconfig.contains(
            "[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-http-token"
        ));
    }

    #[tokio::test]
    async fn git_http_token_is_delivered_as_a_protected_file_and_persisted_helper() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("lab-forgejo".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GitHttpToken { host: "forgejo.lab".to_string(), username: "crew-reader".to_string() },
                    source: CredentialSource::Env { name: "TEST_FORGEJO_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let secret = "forgejo-test-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect("prepare Git HTTP credential");

        assert_eq!(
            delivered,
            vec![
                ("GIT_CONFIG_GLOBAL".to_string(), "/tmp/flotilla-test-state/credentials/gitconfig".to_string()),
                ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            ]
        );
        let writes = runner.writes.lock().expect("writes lock");
        assert_eq!(writes[0], (PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/token"), secret.to_string()));
        assert_eq!(writes[1].0, PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-http-token"));
        assert!(writes[1].1.contains("[ \"$protocol\" = https ]"));
        assert!(writes[1].1.contains("[ \"$request_host\" = 'forgejo.lab' ]"));
        assert!(writes[1].1.contains("printf 'username=%s\\n' 'crew-reader'"));
        assert!(writes[1].1.contains("cat '/tmp/flotilla-test-state/credentials/lab-forgejo/token'"));
        assert!(!writes[1].1.contains(secret));
        assert_eq!(
            writes[2],
            (
                PathBuf::from("/tmp/flotilla-test-state/credentials/gitconfig"),
                "# fragment: credential/git-http-token lab-forgejo\n[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-http-token\n\n# fragment: vessel/crew-identity\n[user]\n\temail = 309902803+flotilla-crew[bot]@users.noreply.github.com\n\n# fragment: vessel/crew-identity\n[user]\n\tname = flotilla-crew[bot]\n".to_string()
            )
        );
        let protected = runner.protected_writes.lock().expect("protected writes lock");
        assert!(protected.contains(&(PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/token"), 0o600)));
        assert!(protected.contains(&(PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-http-token"), 0o700)));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh"
                && args.iter().any(|arg| arg.contains("GIT_CONFIG_NOSYSTEM=1"))
                && args.iter().any(|arg| arg == "forgejo.lab")
                && input.is_empty()
        }));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.starts_with("/run/flotilla")));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
    }

    #[tokio::test]
    async fn review_store_credential_is_staged_as_a_scoped_file() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("review-store".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::ReviewBundleStore {
                        endpoint: "http://rustfs.lab:9000".to_string(),
                        bucket: "flotilla".to_string(),
                        region: "us-east-1".to_string(),
                        public_base_url: "https://reviews.example/flotilla".to_string(),
                        allow_http: true,
                        virtual_hosted_style: false,
                    },
                    source: CredentialSource::Env { name: "TEST_REVIEW_STORE_CREDENTIAL".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create review store credential declaration");
        let material = r#"{"access_key_id":"crew","secret_access_key":"secret"}"#;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_REVIEW_STORE_CREDENTIAL".to_string(), material.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            env,
            EnvironmentBag::new(),
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let delivered: BTreeMap<_, _> = store
            .prepare("env-a", &BTreeSet::from(["review-store".to_string()]), runner.clone())
            .await
            .expect("prepare review store credential")
            .into_iter()
            .collect();

        assert_eq!(delivered["FLOTILLA_REVIEW_STORE_PREFIX"], "reviews/");
        assert_eq!(delivered["FLOTILLA_REVIEW_STORE_ENDPOINT"], "http://rustfs.lab:9000");
        let credential_file = &delivered["FLOTILLA_REVIEW_STORE_CREDENTIAL_FILE"];
        assert!(runner
            .writes
            .lock()
            .expect("writes lock")
            .iter()
            .any(|(path, contents)| path == Path::new(credential_file) && contents == material));
        assert!(runner.protected_writes.lock().expect("protected writes lock").contains(&(PathBuf::from(credential_file), 0o600)));
    }

    async fn create_git_http_token_spec(backend: &ResourceBackend, name: &str, host: &str, source_env: &str) {
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::GitHttpToken { host: host.to_string(), username: "crew-reader".to_string() },
                    source: CredentialSource::Env { name: source_env.to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
    }

    #[tokio::test]
    async fn git_http_token_host_rejects_a_url() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "lab-forgejo", "http://forgejo.lab", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect_err("a host containing a URL must be rejected");

        assert!(error.contains("must contain only a hostname"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty(), "no material may be written for a rejected URL");
    }

    #[test]
    fn git_http_token_host_is_canonicalized_before_delivery() {
        assert_eq!(canonical_git_http_host("FORGEJO.LAB/"), Ok("forgejo.lab".to_string()));
        assert_eq!(canonical_git_http_host("forgejo.lab:3000/"), Ok("forgejo.lab:3000".to_string()));
    }

    #[tokio::test]
    async fn git_http_token_host_must_parse() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "lab-forgejo", "not a host", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect_err("an unparsable Git HTTPS host must be rejected");

        assert!(error.contains("invalid Git HTTPS host"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty(), "no material may be written for a rejected URL");
    }

    #[tokio::test]
    async fn git_http_token_helper_and_config_agree_on_an_explicit_port() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "lab-forgejo", "forgejo.lab:3000", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered: BTreeMap<String, String> = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect("prepare Git HTTP credential with an explicit port")
            .into_iter()
            .collect();

        assert_eq!(delivered.get("GIT_CONFIG_GLOBAL"), Some(&"/tmp/flotilla-test-state/credentials/gitconfig".to_string()));
        let writes = runner.writes.lock().expect("writes lock");
        assert!(
            writes[1].1.contains("[ \"$request_host\" = 'forgejo.lab:3000' ]"),
            "helper must compare against the host:port form git passes when a port is present"
        );
        assert!(writes[2].1.contains("[credential \"https://forgejo.lab:3000\"]"));
    }

    #[tokio::test]
    async fn git_http_token_credentials_for_multiple_hosts_coexist() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "lab-a", "forgejo.lab", "TEST_TOKEN_A").await;
        create_git_http_token_spec(&backend, "lab-b", "other.lab", "TEST_TOKEN_B").await;
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([
                ("TEST_TOKEN_A".to_string(), "token-a".to_string()),
                ("TEST_TOKEN_B".to_string(), "token-b".to_string()),
            ]))),
            bag,
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let delivered = store
            .prepare("env-a", &BTreeSet::from(["lab-a".to_string(), "lab-b".to_string()]), runner.clone())
            .await
            .expect("host-specific helpers coexist");

        assert!(delivered.iter().any(|(key, _)| key == "GIT_CONFIG_GLOBAL"));
        let writes = runner.writes.lock().expect("writes lock");
        let gitconfig = &writes.last().expect("composed Git config").1;
        assert!(gitconfig.contains("[credential \"https://forgejo.lab\"]"));
        assert!(gitconfig.contains("[credential \"https://other.lab\"]"));
    }

    #[tokio::test]
    async fn git_http_token_credentials_for_the_same_canonical_host_fail_before_delivery() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "lab-a", "FORGEJO.LAB", "TEST_TOKEN_A").await;
        create_git_http_token_spec(&backend, "lab-b", "forgejo.lab/", "TEST_TOKEN_B").await;
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([
                ("TEST_TOKEN_A".to_string(), "token-a".to_string()),
                ("TEST_TOKEN_B".to_string(), "token-b".to_string()),
            ]))),
            EnvironmentBag::new(),
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-a".to_string(), "lab-b".to_string()]), runner.clone())
            .await
            .expect_err("duplicate canonical hosts must be rejected");

        assert!(error.contains("multiple granted credentials target the same Git HTTPS host"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty());
    }

    #[tokio::test]
    async fn forgejo_and_git_http_token_credentials_for_the_same_host_fail_before_delivery() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .definitions::<Forge>("flotilla")
            .create(
                &InputMeta::builder().name("lab".to_string()).build(),
                &ForgeSpec::builder()
                    .forge_id("lab".to_string())
                    .kind(ForgeKind::Forgejo)
                    .hosts(BTreeSet::from(["forgejo.lab".to_string()]))
                    .https_url("https://FORGEJO.LAB/".to_string())
                    .git_ssh_host("forgejo.lab".to_string())
                    .build(),
            )
            .await
            .expect("create Forge definition");
        create_git_http_token_spec(&backend, "git-only", "forgejo.lab", "TEST_GIT_TOKEN").await;
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("forgejo-api".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Forgejo { forge_ref: "lab".to_string(), username: "crew-reader".to_string() },
                    source: CredentialSource::Env { name: "TEST_FORGEJO_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create Forgejo credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([
                ("TEST_GIT_TOKEN".to_string(), "git-token".to_string()),
                ("TEST_FORGEJO_TOKEN".to_string(), "forgejo-token".to_string()),
            ]))),
            EnvironmentBag::new(),
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let error = store
            .prepare("env-a", &BTreeSet::from(["forgejo-api".to_string(), "git-only".to_string()]), runner.clone())
            .await
            .expect_err("cross-adapter duplicate hosts must be rejected");

        assert!(error.contains("multiple granted credentials target the same Git HTTPS host"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty());
    }

    #[tokio::test]
    async fn forgejo_delivery_derives_urls_from_referenced_forge() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let forge = ForgeSpec::builder()
            .forge_id("lab".to_string())
            .kind(ForgeKind::Forgejo)
            .hosts(BTreeSet::from(["forgejo.lab".to_string()]))
            .https_url("https://forgejo.lab".to_string())
            .git_ssh_host("forgejo.lab".to_string())
            .build();
        backend
            .definitions::<Forge>("flotilla")
            .create(&InputMeta::builder().name("lab".to_string()).build(), &forge)
            .await
            .expect("create Forge definition");
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("forgejo-api".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Forgejo { forge_ref: "lab".to_string(), username: "crew".to_string() },
                    source: CredentialSource::Env { name: "TEST_FORGEJO_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "secret".to_string())]))),
            EnvironmentBag::new(),
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );
        let env: BTreeMap<_, _> = store
            .prepare("env-a", &BTreeSet::from(["forgejo-api".to_string()]), runner)
            .await
            .expect("prepare Forgejo credential")
            .into_iter()
            .collect();
        assert_eq!(env.get("FORGEJO_SERVER_URL").map(String::as_str), Some("https://forgejo.lab"));
        assert_eq!(env.get("FORGEJO_API_URL").map(String::as_str), Some("https://forgejo.lab/api/v1"));
        let ledger_env = store.ledger_delivery_environment("env-a").await.expect("Forgejo delivery record");
        assert_eq!(ledger_env.get("FORGEJO_TOKEN_FILE"), env.get("FORGEJO_TOKEN_FILE"));
        assert_eq!(ledger_env.get("FORGEJO_API_URL"), env.get("FORGEJO_API_URL"));
        assert!(!ledger_env.contains_key("FORGEJO_SERVER_URL"));
        store.forget_environment("env-a").await.expect("forget Forgejo delivery");
        assert!(store.ledger_delivery_environment("env-a").await.expect("forgotten record").is_empty());
    }

    #[tokio::test]
    async fn github_and_git_http_token_credentials_for_github_fail_before_delivery() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_git_http_token_spec(&backend, "git-only", "GITHUB.COM/", "TEST_GIT_TOKEN").await;
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("github".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::Gh,
                    source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create GitHub credential declaration");
        let runner = Arc::new(RecordingRunner::default());
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([
                ("TEST_GIT_TOKEN".to_string(), "git-token".to_string()),
                ("TEST_GITHUB_TOKEN".to_string(), "github-token".to_string()),
            ]))),
            EnvironmentBag::new(),
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let error = store
            .prepare("env-a", &BTreeSet::from(["git-only".to_string(), "github".to_string()]), runner.clone())
            .await
            .expect_err("GitHub cross-adapter duplicate hosts must be rejected");

        assert!(error.contains("multiple granted credentials target the same Git HTTPS host"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty());
    }

    // #2729: image push needs a declared builder and a host-action grant.
    // Each operation gets a distinct config that is gone when it returns;
    // credentials are supplied on stdin and never in argv or a crew environment.
    #[tokio::test]
    async fn image_push_requires_host_grant_and_uses_throwaway_config() {
        use flotilla_resources::{
            CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, Host, HostActionSelector, HostImageAction, HostSpec,
            ImageBuildCapacity, ImageBuildReservation,
        };
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("registry".into()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::DockerRegistry { registry: "registry.example".into(), username: "builder".into() },
                    source: CredentialSource::Env { name: "TEST_REGISTRY_TOKEN".into() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: Default::default(),
                },
            )
            .await
            .expect("credential");
        let hosts = backend.using::<Host>("flotilla");
        hosts.create(&InputMeta::builder().name("builder".into()).build(), &HostSpec::default()).await.expect("host");
        let runner = Arc::new(RecordingRunner::default());
        let state = tempfile::tempdir().expect("state");
        let store = CredentialStore::new(
            backend.clone(),
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([("TEST_REGISTRY_TOKEN".into(), "test-secret".into())]))),
            EnvironmentBag::new(),
            runner.clone(),
            state.path().into(),
        );
        let operation = || {
            store.image_registry_operation(
                "builder",
                HostImageAction::ImagePush,
                "registry",
                "registry.example/images",
                &["push", "registry.example/images:label"],
            )
        };
        assert!(operation().await.expect_err("no grant").contains("no ImagePush grant"));
        backend
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &InputMeta::builder().name("push".into()).build(),
                &CredentialGrantSpec::builder()
                    .selector(
                        CredentialGrantSelector::builder()
                            .host_action(HostActionSelector::builder().action(HostImageAction::ImagePush).build())
                            .build(),
                    )
                    .credentials(BTreeSet::from(["registry".into()]))
                    .build(),
            )
            .await
            .expect("grant");
        assert!(operation().await.is_err(), "implicit build capacity cannot authorize pushes");
        assert!(runner.calls.lock().expect("calls").is_empty());
        let host = hosts.get("builder").await.expect("host");
        let mut spec = host.spec.clone();
        spec.image_build_capacity = Some(ImageBuildCapacity::Builder {
            architecture: "amd64".into(),
            slots: 1,
            reservation: ImageBuildReservation { cpu: 1, disk_bytes: 1024 },
        });
        hosts.update(&InputMeta::from(&host.metadata), &host.metadata.resource_version, &spec).await.expect("builder");
        operation().await.expect("push");
        operation().await.expect("second push");
        let calls = runner.calls.lock().expect("calls");
        assert_eq!(calls.len(), 4);
        assert_ne!(calls[0].1[1], calls[2].1[1]);
        for (index, (_, args, input)) in calls.iter().enumerate() {
            assert_eq!(args[0], "--config");
            assert!(!Path::new(&args[1]).exists(), "config removed after operation");
            assert!(args[1].starts_with(state.path().to_str().expect("state path")));
            assert!(!args.iter().any(|arg| arg.contains("test-secret")));
            assert_eq!(input.as_slice(), if index % 2 == 0 { b"test-secret".as_slice() } else { &[] });
        }
    }

    // Cancellation while login is in flight removes the config and never
    // starts a pull; cleanup does not rely on the operation reaching a return.
    #[tokio::test]
    async fn cancelled_host_image_operation_removes_private_config() {
        use flotilla_resources::{CredentialGrant, CredentialGrantSelector, CredentialGrantSpec, HostActionSelector, HostImageAction};
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("registry".into()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::DockerRegistry { registry: "registry.example".into(), username: "host".into() },
                    source: CredentialSource::Env { name: "TEST_REGISTRY_TOKEN".into() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: Default::default(),
                },
            )
            .await
            .expect("credential");
        backend
            .definitions::<CredentialGrant>("flotilla")
            .create(
                &InputMeta::builder().name("pull".into()).build(),
                &CredentialGrantSpec::builder()
                    .selector(
                        CredentialGrantSelector::builder()
                            .host_action(HostActionSelector::builder().action(HostImageAction::ImagePull).build())
                            .build(),
                    )
                    .credentials(BTreeSet::from(["registry".into()]))
                    .build(),
            )
            .await
            .expect("grant");
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Semaphore::new(0)));
        let runner = Arc::new(RecordingRunner { registry_login_gate: Some(gate.clone()), ..Default::default() });
        let state = tempfile::tempdir().expect("state");
        let store = Arc::new(CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv(BTreeMap::from([("TEST_REGISTRY_TOKEN".into(), "test-secret".into())]))),
            EnvironmentBag::new(),
            runner.clone(),
            state.path().into(),
        ));
        let task = tokio::spawn(async move {
            store
                .image_registry_operation(
                    "host",
                    HostImageAction::ImagePull,
                    "registry",
                    "registry.example/images",
                    &["pull", "registry.example/images@sha256:3333333333333333333333333333333333333333333333333333333333333333"],
                )
                .await
        });
        gate.0.notified().await;
        let path = runner.calls.lock().expect("calls")[0].1[1].clone();
        assert!(Path::new(&path).is_dir());
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        assert!(!Path::new(&path).exists());
        assert_eq!(runner.calls.lock().expect("calls").len(), 1);
    }

    #[tokio::test]
    async fn registry_config_survives_concurrent_sweep_and_preflight_until_the_environment_is_forgotten() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(
                &InputMeta::builder().name("private-registry".to_string()).build(),
                &CredentialSpecSpec {
                    consumer: CredentialConsumer::DockerRegistry { registry: "registry.example".to_string(), username: "crew".to_string() },
                    source: CredentialSource::Env { name: "TEST_REGISTRY_TOKEN".to_string() },
                    lifecycle: CredentialLifecycle::Static,
                    placement: CredentialPlacementRequirements::default(),
                },
            )
            .await
            .expect("create credential declaration");
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_REGISTRY_TOKEN".to_string(), "registry-secret".to_string())])));
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Semaphore::new(0)));
        let runner = Arc::new(RecordingRunner { registry_login_gate: Some(Arc::clone(&gate)), ..RecordingRunner::default() });
        let state = tempfile::tempdir().expect("create state directory");
        let store =
            Arc::new(CredentialStore::new(backend, "flotilla", env, EnvironmentBag::new(), runner.clone(), state.path().to_path_buf()));

        let preparing = {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .prepare_registry_pull("env-a", &BTreeSet::from(["private-registry".to_string()]), "registry.example/crew:latest")
                    .await
            })
        };
        gate.0.notified().await;
        // At the process boundary the cache exists but has not been indexed.
        // Sweeping must be excluded across this whole vulnerable lifecycle.
        assert!(store.registry_cache_maintenance.try_write().is_err(), "sweep must wait for in-flight cache registration");
        let sweeping = {
            let store = Arc::clone(&store);
            tokio::spawn(async move { store.sweep_orphaned_registry_configs(&BTreeSet::new(), &BTreeSet::new()).await })
        };
        gate.1.add_permits(1);
        let PreparedEnvironmentAuth::RegistryConfig { directory } =
            preparing.await.expect("preparation task").expect("prepare registry credential")
        else {
            panic!("matching credential");
        };
        let config_dir = directory.as_path();
        sweeping.await.expect("sweep task").expect("sweep with stale empty owner snapshots");

        assert!(config_dir.is_dir(), "credential config must remain available to docker run");
        assert_eq!(
            std::fs::metadata(config_dir).expect("credential config metadata").permissions().mode() & 0o777,
            0o700,
            "credential config directory must not be readable by other host users"
        );
        let config = config_dir.to_string_lossy();
        {
            let calls = runner.calls.lock().expect("calls lock");
            assert!(calls
                .iter()
                .any(|(command, args, _)| { command == "docker" && args.windows(2).any(|pair| pair == ["--config", config.as_ref()]) }));
        }

        store.forget_environment("env-a").await.expect("forget environment");
        assert!(!config_dir.exists(), "credential config should be deleted with the environment");
    }

    #[tokio::test]
    async fn registry_sweep_removes_orphans_and_preserves_live_environment_directories() {
        let state = tempfile::tempdir().expect("state directory");
        let root = state.path().join("credential-runtime");
        let live = root.join(registry_environment_dir("live-env")).join(uuid::Uuid::new_v4().to_string());
        let running = root.join(registry_environment_dir("running-env")).join(uuid::Uuid::new_v4().to_string());
        let orphan = root.join(registry_environment_dir("orphan-env")).join(uuid::Uuid::new_v4().to_string());
        let legacy = root.join(format!("private-registry-{}", uuid::Uuid::new_v4()));
        for path in [&live, &running, &orphan] {
            std::fs::create_dir_all(path).expect("cache directory");
            std::fs::write(path.join("config.json"), "secret").expect("cache file");
        }
        std::fs::create_dir_all(&legacy).expect("legacy cache directory");
        let store = CredentialStore::new(
            ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a")),
            "flotilla",
            Arc::new(TestEnv::default()),
            EnvironmentBag::new(),
            Arc::new(RecordingRunner::default()),
            state.path().to_path_buf(),
        );
        store
            .sweep_orphaned_registry_configs(&BTreeSet::from(["live-env".to_string()]), &BTreeSet::from(["running-env".to_string()]))
            .await
            .expect("sweep cache directories");
        assert!(live.is_dir(), "live Environment directory must remain");
        assert!(running.is_dir(), "running backing directory must remain");
        assert!(!orphan.exists(), "orphan directory must be removed");
        assert!(legacy.is_dir(), "unattributed legacy cache could belong to a live environment");
        store.sweep_orphaned_registry_configs(&BTreeSet::new(), &BTreeSet::new()).await.expect("sweep with no live owners");
        assert!(!legacy.exists(), "legacy orphan must be removed once no environment can own it");
    }
}

#[cfg(test)]
#[path = "credential/http_contract.rs"]
mod http_contract;

#[async_trait]
impl flotilla_core::crew_capabilities::SessionCapabilitySource for CredentialStore {
    async fn endpoints(&self, environment: &str, session: &str) -> Result<BTreeMap<String, String>, String> {
        Ok(self.capability_endpoints.lock().await.get(&(environment.to_string(), session.to_string())).cloned().unwrap_or_default())
    }

    async fn credentials(
        &self,
        environment: &str,
        references: &BTreeSet<String>,
    ) -> Result<Vec<flotilla_core::crew_capabilities::CredentialCapability>, String> {
        let names = self
            .prepared
            .lock()
            .await
            .iter()
            .filter(|(env, name)| env == environment && references.contains(name))
            .map(|(_, name)| name.clone())
            .collect::<BTreeSet<_>>();
        let scopes = self.capability_scopes.lock().await.clone();
        let deliveries = self.github_app_deliveries.lock().await;
        let mut capabilities = Vec::new();
        for name in names {
            let delivery = deliveries.get(&(environment.to_string(), name.clone()));
            if delivery.is_some_and(|delivery| delivery.expires_at <= self.clock.now()) {
                continue;
            }
            capabilities.push(
                flotilla_core::crew_capabilities::CredentialCapability::builder()
                    .name(name.clone())
                    .repositories(
                        delivery
                            .map(|delivery| delivery.request.repositories.clone())
                            .unwrap_or_else(|| scopes.get(&(environment.to_string(), name.clone())).cloned().unwrap_or_default()),
                    )
                    .maybe_permissions(delivery.and_then(|delivery| delivery.effective_permissions.clone()))
                    .build(),
            );
        }
        Ok(capabilities)
    }
}
