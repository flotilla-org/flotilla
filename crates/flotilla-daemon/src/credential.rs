use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_core::providers::{
    discovery::{EnvVars, EnvironmentBag},
    ChannelLabel, CommandRunner, HttpClient, ReqwestHttpClient,
};
use flotilla_resources::{
    Clock, CredentialConsumer, CredentialExpiry, CredentialLifecycle, CredentialSource, CredentialSpec, CredentialSpecSpec, Repository,
    RepositoryKey, ResourceBackend, ResourceError, SystemClock, AMBIENT_CLAUDE_CREDENTIAL_SCOPE,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

use crate::vessel_config::{
    agent_environment_fragment, compose, crew_gitconfig_fragments, Fragment, GitConfigKey, Merge, Provenance, TargetId, TargetKey,
};

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
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct GithubAppInstallationResponse {
    id: u64,
}

#[derive(Clone)]
struct GithubAppToken {
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
    Other(String),
}

impl std::fmt::Display for GithubAppMintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstallationNotFound(message) | Self::Other(message) => formatter.write_str(message),
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
        Ok(GithubAppToken { value: response.token, expires_at: response.expires_at })
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

pub(crate) struct CredentialStore {
    backend: ResourceBackend,
    namespace: String,
    env: Arc<dyn EnvVars>,
    host_bag: EnvironmentBag,
    host_runner: Arc<dyn CommandRunner>,
    clock: Arc<dyn Clock>,
    github_app_minter: Arc<dyn GithubAppTokenMinter>,
    state_dir: PathBuf,
    prepared: Mutex<BTreeSet<(String, String)>>,
    materials: Mutex<BTreeMap<(String, String), String>>,
    git_config_fragments: Mutex<BTreeMap<String, BTreeMap<String, Fragment>>>,
    registry_configs: Mutex<BTreeMap<String, PathBuf>>,
    github_app_deliveries: Mutex<BTreeMap<(String, String), GithubAppDelivery>>,
    github_app_adoption_failures: Mutex<BTreeMap<String, usize>>,
    github_app_installations: Mutex<BTreeMap<GithubAppInstallationRequest, u64>>,
}

const GITHUB_APP_REFRESH_MARGIN: Duration = Duration::minutes(5);

#[derive(Clone)]
struct GithubAppDelivery {
    generation: uuid::Uuid,
    request: GithubAppMintRequest,
    runner: Arc<dyn CommandRunner>,
    token_file: PathBuf,
    expires_at: DateTime<Utc>,
    refresh_failures: usize,
    installation_repository: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CredentialRefreshError {
    pub(crate) environment_ref: String,
    pub(crate) message: String,
    pub(crate) should_surface: bool,
}

const GITHUB_APP_REFRESH_FAILURE_THRESHOLD: usize = 3;

#[derive(Debug)]
struct ResolvedMaterial {
    value: String,
    github_app: Option<(GithubAppMintRequest, DateTime<Utc>)>,
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
        }
    }
}

impl CredentialStore {
    pub(crate) fn new(
        backend: ResourceBackend,
        namespace: &str,
        env: Arc<dyn EnvVars>,
        host_bag: EnvironmentBag,
        host_runner: Arc<dyn CommandRunner>,
        state_dir: PathBuf,
    ) -> Self {
        Self::new_with_http(backend, namespace, env, host_bag, host_runner, Arc::new(ReqwestHttpClient::new()), state_dir)
    }

    fn new_with_http(
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
            materials: Mutex::new(BTreeMap::new()),
            git_config_fragments: Mutex::new(BTreeMap::new()),
            registry_configs: Mutex::new(BTreeMap::new()),
            github_app_deliveries: Mutex::new(BTreeMap::new()),
            github_app_adoption_failures: Mutex::new(BTreeMap::new()),
            github_app_installations: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn vessel_config_fragments(
        &self,
        credential_refs: &BTreeSet<String>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Vec<Fragment>, String> {
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
    }

    pub(crate) async fn vessel_config_fragments_for_runner(
        &self,
        credential_refs: &BTreeSet<String>,
        environment: &BTreeMap<String, String>,
        runner: &dyn CommandRunner,
    ) -> Result<Vec<Fragment>, String> {
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
    }

    pub(crate) async fn held_credentials(&self) -> Result<BTreeSet<String>, String> {
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
    }

    /// Expiry metadata for held material, keyed by scope name. Timestamps
    /// only — material is never read into the result. Declared
    /// `CredentialSpec`s contribute here once an adapter can express expiry
    /// without touching material; today none of the declared sources carry
    /// such metadata, so the map holds only the ambient claude login.
    pub(crate) async fn credential_expiry(&self) -> BTreeMap<String, CredentialExpiry> {
        let mut expiry = BTreeMap::new();
        if let Some(ambient) = self.ambient_claude_expiry().await {
            expiry.insert(AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), ambient);
        }
        expiry
    }

    async fn ambient_claude_expiry(&self) -> Option<CredentialExpiry> {
        let path = match self.env.get("CLAUDE_CONFIG_DIR").filter(|dir| !dir.trim().is_empty()) {
            Some(dir) => PathBuf::from(dir).join(".credentials.json"),
            None => self.expand_path("~/.claude/.credentials.json"),
        };
        let contents = tokio::fs::read(&path).await.ok()?;
        let metadata: AmbientClaudeCredentialsMetadata = match serde_json::from_slice(&contents) {
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

    #[cfg(test)]
    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        self.prepare_scoped(environment_ref, credential_refs, &BTreeMap::new(), runner).await
    }

    pub(crate) async fn prepare_scoped(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        self.prepare_scoped_with_github_repository_grants(environment_ref, credential_refs, credential_scopes, &BTreeSet::new(), runner)
            .await
    }

    pub(crate) async fn prepare_scoped_with_github_repository_grants(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        github_repository_grants: &BTreeSet<String>,
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
        for (name, spec) in &specs {
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
                    | CredentialConsumer::ClaudeOauth { .. }
                    | CredentialConsumer::Codex
                    | CredentialConsumer::ReviewBundleStore { .. }
            )
        }) {
            Some(self.delivery_paths(&*runner).await?)
        } else {
            None
        };
        let mut env = BTreeMap::new();
        let mut new_git_config_fragments = BTreeMap::new();
        let mut git_config_owner = None;
        let mut pending_git_preflights = Vec::new();
        let mut prepared_cache_keys = Vec::new();
        for (name, spec) in &specs {
            let cache_key = (environment_ref.to_string(), name.clone());
            let cached_material = {
                let materials = self.materials.lock().await;
                materials.get(&cache_key).cloned()
            };
            // Issued material is minted once per environment and evicted with
            // that environment. Refreshable material is resolved for every
            // preparation; static material follows the same environment cache.
            let resolved = if spec.lifecycle == CredentialLifecycle::Refreshable {
                self.resolve_for_adapter(name, spec, credential_scopes.get(name), github_repository_grants).await?
            } else if let Some(material) = cached_material {
                ResolvedMaterial { value: material, github_app: None }
            } else {
                let material = self.resolve_for_adapter(name, spec, credential_scopes.get(name), github_repository_grants).await?;
                self.materials.lock().await.insert(cache_key.clone(), material.value.clone());
                material
            };
            let already_prepared = spec.lifecycle != CredentialLifecycle::Refreshable && self.prepared.lock().await.contains(&cache_key);
            let material = resolved.value.trim_end();
            if let Err(error) = validate_scalar_material(name, spec.consumer.adapter_name(), material) {
                self.materials.lock().await.remove(&cache_key);
                return Err(error);
            }
            let mut github_app_deliveries =
                if resolved.github_app.is_some() { Some(self.github_app_deliveries.lock().await) } else { None };
            let delivered =
                match self.prepare_adapter(name, spec, material, Arc::clone(&runner), already_prepared, delivery_paths.as_ref()).await {
                    Ok(delivered) => delivered,
                    Err(message) => {
                        self.materials.lock().await.remove(&cache_key);
                        return Err(bounded_adapter_error(name, spec.consumer.adapter_name(), &message.replace(material, "[redacted]")));
                    }
                };
            env.extend(delivered.env);
            if let Some((request, expires_at)) = resolved.github_app {
                let paths = delivery_paths.as_ref().expect("GitHub App adapter resolves delivery paths");
                github_app_deliveries.as_mut().expect("GitHub App delivery holds the write lock").insert(
                    cache_key.clone(),
                    GithubAppDelivery {
                        generation: uuid::Uuid::new_v4(),
                        request,
                        runner: Arc::clone(&runner),
                        token_file: github_app_token_file(paths, name),
                        expires_at,
                        refresh_failures: 0,
                        installation_repository: match &spec.consumer {
                            CredentialConsumer::GithubApp { installation_repository, .. } => installation_repository.clone(),
                            _ => None,
                        },
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
        self.prepared.lock().await.extend(prepared_cache_keys);
        Ok(env.into_iter().collect())
    }

    /// Rebuild refresh registrations for an already-running environment from
    /// its durable credential requirements. Reconciliation calls this on every
    /// pass, so a live registration makes the operation a no-op.
    pub(crate) async fn adopt_github_app_deliveries(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        credential_scopes: &BTreeMap<String, BTreeSet<RepositoryKey>>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<(), CredentialRefreshError> {
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
        if let Err(message) = self.prepare_scoped(environment_ref, &github_app_refs, &github_app_scopes, runner).await {
            return Err(self.record_adoption_failure(environment_ref, message).await);
        }
        self.github_app_adoption_failures.lock().await.remove(environment_ref);
        Ok(())
    }

    async fn record_adoption_failure(&self, environment_ref: &str, message: String) -> CredentialRefreshError {
        let mut failures = self.github_app_adoption_failures.lock().await;
        let failures = failures.entry(environment_ref.to_string()).or_default();
        *failures += 1;
        CredentialRefreshError {
            environment_ref: environment_ref.to_string(),
            message,
            should_surface: *failures >= GITHUB_APP_REFRESH_FAILURE_THRESHOLD,
        }
    }

    pub(crate) async fn prepare_registry_pull(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        image: &str,
    ) -> Result<Option<PathBuf>, String> {
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
            return Ok(None);
        };
        if !matching.is_empty() {
            return Err(bounded_adapter_error(&name, "docker-registry", "multiple granted credentials match the image registry"));
        }
        let CredentialConsumer::DockerRegistry { registry, username } = &spec.consumer else {
            unreachable!("matching credentials are docker-registry consumers");
        };
        let previous = self.registry_configs.lock().await.remove(environment_ref);
        if let Some(previous) = previous {
            remove_registry_config(&previous)
                .await
                .map_err(|error| bounded_adapter_error(&name, "docker-registry", &format!("remove stale writable cache: {error}")))?;
        }
        let material = self.resolve_for_adapter(&name, &spec, None, &BTreeSet::new()).await?;
        let material = material.value.trim_end();
        validate_scalar_material(&name, "docker-registry", material)?;
        let config_dir = self.state_dir.join("credential-runtime").join(format!("{}-{}", safe_component(&name), uuid::Uuid::new_v4()));
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
        Ok(Some(config_dir))
    }

    pub(crate) async fn forget_environment(&self, environment_ref: &str) -> Result<(), String> {
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
    }

    /// Re-mint and atomically replace GitHub App files that are approaching
    /// expiry. The daemon calls this from its host-side periodic loop; vessels
    /// receive only the resulting file and never the App signing material.
    pub(crate) async fn refresh_due_github_app_tokens(&self) -> Vec<CredentialRefreshError> {
        let refresh_before = self.clock.now() + GITHUB_APP_REFRESH_MARGIN;
        let due = self
            .github_app_deliveries
            .lock()
            .await
            .iter()
            .filter(|(_, delivery)| delivery.expires_at <= refresh_before)
            .map(|(key, delivery)| (key.clone(), delivery.clone()))
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        for (key, delivery) in due {
            let mut request = delivery.request.clone();
            let token = match self.mint_github_app(&mut request, delivery.installation_repository.as_deref()).await {
                Ok(token) => token,
                Err(error) => {
                    let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                    errors.push(CredentialRefreshError {
                        environment_ref: key.0.clone(),
                        message: format!("credential `{}` adapter `github-app`: {error}", key.1),
                        should_surface,
                    });
                    continue;
                }
            };
            if let Err(error) = validate_scalar_material(&key.1, "github-app", token.value.trim_end()) {
                let should_surface = self.record_refresh_failure(&key, delivery.generation).await;
                errors.push(CredentialRefreshError { environment_ref: key.0.clone(), message: error, should_surface });
                continue;
            }
            let mut deliveries = self.github_app_deliveries.lock().await;
            let Some(current) = deliveries.get_mut(&key).filter(|current| current.generation == delivery.generation) else {
                continue;
            };
            if let Err(error) = write_github_app_token_file(&*current.runner, &current.token_file, token.value.trim_end()).await {
                current.refresh_failures += 1;
                errors.push(CredentialRefreshError {
                    environment_ref: key.0.clone(),
                    message: bounded_adapter_error(&key.1, "github-app", &error),
                    should_surface: current.refresh_failures >= GITHUB_APP_REFRESH_FAILURE_THRESHOLD
                        || self.clock.now() >= current.expires_at,
                });
                continue;
            }
            current.expires_at = token.expires_at;
            current.refresh_failures = 0;
            current.request = request;
        }
        errors
    }

    async fn record_refresh_failure(&self, key: &(String, String), generation: uuid::Uuid) -> bool {
        let mut deliveries = self.github_app_deliveries.lock().await;
        let Some(current) = deliveries.get_mut(key).filter(|current| current.generation == generation) else {
            return false;
        };
        current.refresh_failures += 1;
        current.refresh_failures >= GITHUB_APP_REFRESH_FAILURE_THRESHOLD || self.clock.now() >= current.expires_at
    }

    async fn spec(&self, name: &str) -> Result<CredentialSpecSpec, String> {
        self.backend.clone().definitions::<CredentialSpec>(&self.namespace).get(name).await.map(|object| object.spec).map_err(|error| {
            match error {
                ResourceError::NotFound { .. } => format!("credential `{name}` declaration not found"),
                error => format!("credential `{name}` declaration unavailable: {error}"),
            }
        })
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
        github_repository_grants: &BTreeSet<String>,
    ) -> Result<ResolvedMaterial, String> {
        let result = match (&spec.consumer, &spec.source) {
            (
                CredentialConsumer::GithubApp { installation_id, installation_repository, permissions },
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
                repositories.extend(github_repository_grants.iter().cloned());
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
                    permissions: permissions.clone(),
                };
                self.mint_github_app(&mut request, installation_repository.as_deref())
                    .await
                    .map(|token| ResolvedMaterial { value: token.value, github_app: Some((request, token.expires_at)) })
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
        match self.github_app_minter.mint(request).await {
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
                self.github_app_minter.mint(request).await.map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn github_repository_names(&self, repository_scope: &BTreeSet<RepositoryKey>) -> Result<Vec<String>, String> {
        if repository_scope.len() > 500 {
            return Err("GitHub App repository scope exceeds the 500-repository API limit".to_string());
        }
        let repositories = self
            .backend
            .including_replicas::<Repository>(&self.namespace)
            .list()
            .await
            .map_err(|error| format!("list repository identities: {error}"))?;
        let mut names = Vec::with_capacity(repository_scope.len());
        for key in repository_scope {
            let repository = repositories
                .items
                .iter()
                .find(|repository| repository.object.spec.key() == *key)
                .ok_or_else(|| format!("repository scope references missing repository `{key}`"))?;
            let forge = repository.object.spec.forge().ok_or_else(|| format!("repository `{key}` has no forge identity"))?;
            let service = Url::parse(&forge.service_url).map_err(|error| format!("repository `{key}` has invalid forge URL: {error}"))?;
            if service.host_str() != Some("github.com") {
                return Err(format!("repository `{key}` is not hosted on github.com"));
            }
            let (_, name) = forge
                .repository
                .rsplit_once('/')
                .ok_or_else(|| format!("repository `{key}` has invalid GitHub identity `{}`", forge.repository))?;
            names.push(name.to_string());
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
            CredentialConsumer::Forgejo { server_url, username } => {
                let delivery_paths = delivery_paths.expect("Forgejo adapter resolves delivery paths");
                let server_url = server_url.trim_end_matches('/');
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
                    runner.write_file(Path::new(&path), material).await.map_err(|error| format!("write token file: {error}"))?;
                    runner
                        .run("chmod", &["0600", &path], Path::new("/"), &ChannelLabel::Default)
                        .await
                        .map_err(|error| format!("protect token file: {error}"))?;
                    let helper = format!(
                        "#!/bin/sh\n[ \"$1\" = get ] || exit 0\nprotocol=\nhost=\nwhile IFS='=' read -r key value; do\n  case \"$key\" in\n    protocol) protocol=$value ;;\n    host) host=$value ;;\n  esac\ndone\n[ \"$protocol\" = https ] || exit 0\n[ \"$host\" = {host}{} ] || exit 0\nprintf 'username=%s\\n' \"$FORGEJO_USERNAME\"\nprintf 'password='\ncat \"$FORGEJO_TOKEN_FILE\"\nprintf '\\n'\n",
                        parsed_url.port().map(|port| format!(":{port}")).unwrap_or_default()
                    );
                    runner
                        .write_file(Path::new(&helper_path), &helper)
                        .await
                        .map_err(|error| format!("write Git credential helper: {error}"))?;
                    runner
                        .run("chmod", &["0700", &helper_path], Path::new("/"), &ChannelLabel::Default)
                        .await
                        .map_err(|error| format!("protect Git credential helper: {error}"))?;
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
            CredentialConsumer::Claude => {
                if !runner.exists("claude", &["--version"]).await {
                    return Err("consumer binary is unavailable".to_string());
                }
                if !already_prepared {
                    api_key_preflight(&*runner, "https://api.anthropic.com/v1/models?limit=1", &[
                        ("x-api-key", material),
                        ("anthropic-version", "2023-06-01"),
                    ])
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
                    api_key_preflight(&*runner, "https://api.openai.com/v1/models?limit=1", &[(
                        "Authorization",
                        &format!("Bearer {material}"),
                    )])
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
                    runner.write_file(&credential_file, material).await.map_err(|error| format!("write credential file: {error}"))?;
                    let path = credential_file.to_string_lossy();
                    runner
                        .run("chmod", &["0600", &path], Path::new("/"), &ChannelLabel::Default)
                        .await
                        .map_err(|error| format!("protect credential file: {error}"))?;
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

async fn write_github_app_token_file(runner: &dyn CommandRunner, path: &Path, token: &str) -> Result<(), String> {
    runner.write_file(path, token).await.map_err(|error| format!("write token file: {error}"))?;
    let path = path.to_string_lossy();
    runner
        .run("chmod", &["0600", &path], Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("protect token file: {error}"))
}

async fn write_executable(runner: &dyn CommandRunner, path: &Path, contents: &str, context: &str) -> Result<(), String> {
    runner.write_file(path, contents).await.map_err(|error| format!("write {context}: {error}"))?;
    let path = path.to_string_lossy();
    runner
        .run("chmod", &["0700", &path], Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("protect {context}: {error}"))
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
        CommandOutput,
    };
    use flotilla_protocol::NodeId;
    use flotilla_resources::{CredentialPlacementRequirements, InMemoryBackend, InputMeta, RepositorySpec, VirtualClock};

    use super::*;

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

    #[async_trait]
    impl GithubAppTokenMinter for BlockingGithubAppTokenMinter {
        async fn resolve_installation(&self, _request: &GithubAppInstallationRequest) -> Result<u64, String> {
            Err("unexpected installation resolution".to_string())
        }

        async fn mint(&self, _request: &GithubAppMintRequest) -> Result<GithubAppToken, GithubAppMintError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(GithubAppToken { value: "initial-token".to_string(), expires_at: self.now + Duration::hours(1) }),
                1 => {
                    self.refresh_started.notify_one();
                    self.release_refresh.notified().await;
                    Ok(GithubAppToken { value: "stale-refresh-token".to_string(), expires_at: self.now + Duration::hours(2) })
                }
                2 => Ok(GithubAppToken { value: "reprepared-token".to_string(), expires_at: self.now + Duration::hours(2) }),
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

    type RecordedCall = (String, Vec<String>, Vec<u8>);

    #[derive(Default)]
    struct RecordingRunner {
        calls: StdMutex<Vec<RecordedCall>>,
        writes: StdMutex<Vec<(PathBuf, String)>>,
        runtime_dir_checks: StdMutex<VecDeque<bool>>,
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
            Ok(CommandOutput { stdout, stderr: String::new(), success })
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
            Ok(String::new())
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }

        async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
            self.writes.lock().expect("writes lock").push((path.to_path_buf(), content.to_string()));
            Ok(())
        }
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
            .create(&InputMeta::builder().name("github-app".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp {
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
            })
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

        store.prepare_scoped("env-a", &refs, &scopes, runner.clone()).await.expect("first preparation");
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
    async fn github_app_mints_task_and_provisioning_repository_grants_on_every_prepare() {
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
            .create(&InputMeta::builder().name("github-app".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp { installation_id: Some(9876), installation_repository: None, permissions: None },
                source: CredentialSource::GithubApp {
                    app_id_path: app_id_path.to_string_lossy().into_owned(),
                    private_key_path: private_key_path.to_string_lossy().into_owned(),
                },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            })
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
    request_body: '{"repositories":["flotilla","mattpocock-skills"]}'
    status: 201
    response_body: '{"token":"installation-token-one","expires_at":"2026-08-03T17:00:00Z"}'
  - channel: http
    method: POST
    url: "https://api.github.com/app/installations/9876/access_tokens"
    request_headers:
      accept: "application/vnd.github+json"
      x-github-api-version: "2022-11-28"
    request_body: '{"repositories":["flotilla","mattpocock-skills"]}'
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
        let grants = BTreeSet::from(["mattpocock-skills".to_string()]);

        let first = store
            .prepare_scoped_with_github_repository_grants("env-a", &refs, &scopes, &grants, runner.clone())
            .await
            .expect("first preparation");
        let second = store
            .prepare_scoped_with_github_repository_grants("env-a", &refs, &scopes, &grants, runner.clone())
            .await
            .expect("second preparation");

        let first = first.into_iter().collect::<BTreeMap<_, _>>();
        let second = second.into_iter().collect::<BTreeMap<_, _>>();
        assert!(!first.contains_key("GH_TOKEN") && !second.contains_key("GH_TOKEN"));
        assert_eq!(first.get("GITHUB_TOKEN_FILE"), second.get("GITHUB_TOKEN_FILE"));
        assert_eq!(first.get("PATH"), second.get("PATH"));
        let writes = runner.writes.lock().expect("writes lock");
        assert!(writes.iter().any(|(_, contents)| contents.contains("installation-token-one")));
        assert!(writes.iter().any(|(_, contents)| contents.contains("installation-token-two")));
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
                Ok(GithubAppToken { value: "installation-token-one".to_string(), expires_at: now + Duration::hours(1) }),
                Err("temporary adoption outage one".to_string()),
                Err("temporary adoption outage two".to_string()),
                Err("persistent adoption outage".to_string()),
                Ok(GithubAppToken { value: "installation-token-two".to_string(), expires_at: now + Duration::hours(2) }),
                Err("temporary outage one".to_string()),
                Err("temporary outage two".to_string()),
                Err("persistent outage".to_string()),
                Ok(GithubAppToken { value: "installation-token-three".to_string(), expires_at: now + Duration::hours(3) }),
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
            .create(&InputMeta::builder().name("github-app".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp { installation_id: Some(9876), installation_repository: None, permissions: None },
                source: CredentialSource::GithubApp {
                    app_id_path: "/host-only/github-app.id".to_string(),
                    private_key_path: "/host-only/github-app.pem".to_string(),
                },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            })
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

        clock.advance(Duration::minutes(115));
        let first_failure = store.refresh_due_github_app_tokens().await;
        assert_eq!(first_failure.len(), 1);
        assert!(!first_failure[0].should_surface, "one transient failure must remain retryable");
        let second_failure = store.refresh_due_github_app_tokens().await;
        assert!(!second_failure[0].should_surface, "two transient failures must remain retryable");
        let third_failure = store.refresh_due_github_app_tokens().await;
        assert!(third_failure[0].should_surface, "a repeated unrefreshable delivery must become visible");
        assert!(store.refresh_due_github_app_tokens().await.is_empty());
        assert_eq!(minter.requests.lock().expect("requests lock").len(), 9, "recovered material keeps retrying and eventually rotates");
        let token_writes =
            runner.writes.lock().expect("writes lock").iter().filter(|(path, _)| path.ends_with("token")).cloned().collect::<Vec<_>>();
        assert_eq!(token_writes.len(), 3);
        assert!(token_writes[0].1.contains("installation-token-one"));
        assert!(token_writes[1].1.contains("installation-token-two"));
        assert!(token_writes[2].1.contains("installation-token-three"));
        assert_eq!(token_writes[0].0, token_writes[2].0, "rotation replaces the file observed by the standing vessel");
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
            .create(&InputMeta::builder().name("github-app".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::GithubApp { installation_id: Some(9876), installation_repository: None, permissions: None },
                source: CredentialSource::GithubApp {
                    app_id_path: "/host-only/github-app.id".to_string(),
                    private_key_path: "/host-only/github-app.pem".to_string(),
                },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            })
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
            consumer: CredentialConsumer::GithubApp { installation_id: Some(9876), installation_repository: None, permissions: None },
            source: CredentialSource::GithubApp {
                app_id_path: "/not-read/github-app.id".to_string(),
                private_key_path: "/not-read/github-app.pem".to_string(),
            },
            lifecycle: CredentialLifecycle::Refreshable,
            placement: CredentialPlacementRequirements::default(),
        };

        for scope in [None, Some(&BTreeSet::new())] {
            let error = store.resolve_for_adapter("github-app", &spec, scope, &BTreeSet::new()).await.expect_err("empty scopes must fail");
            assert!(error.contains("empty repository scope"), "unexpected error: {error}");
        }
        let missing_key = RepositoryKey("missing-repository".to_string());
        let error = store.github_repository_names(&BTreeSet::from([missing_key])).await.expect_err("missing repository must fail");
        assert!(error.contains("missing repository"), "unexpected error: {error}");
        let error = store.github_repository_names(&BTreeSet::from([non_github_key])).await.expect_err("non-GitHub repository must fail");
        assert!(error.contains("not hosted on github.com"), "unexpected error: {error}");
        let oversized_scope = (0..501).map(|index| RepositoryKey(format!("repository-{index}"))).collect();
        let error = store.github_repository_names(&oversized_scope).await.expect_err("oversized repository scope must fail");
        assert!(error.contains("500-repository API limit"), "unexpected error: {error}");
        let mut static_spec = spec;
        static_spec.lifecycle = CredentialLifecycle::Static;
        let non_empty_scope = BTreeSet::from([RepositoryKey("not-resolved".to_string())]);
        let error = store
            .resolve_for_adapter("github-app", &static_spec, Some(&non_empty_scope), &BTreeSet::new())
            .await
            .expect_err("non-refreshable GitHub App credentials must fail");
        assert!(error.contains("must use the refreshable lifecycle"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn codex_material_is_transformed_by_stdin_login_and_never_passed_through_as_env() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("model-api".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Codex,
                source: CredentialSource::Env { name: "TEST_OPENAI_KEY".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
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
            .create(&InputMeta::builder().name(name.to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: account_email.to_string() },
                source: CredentialSource::Env { name: source_env.to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
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
            .create(&InputMeta::builder().name("claude-api".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Claude,
                source: CredentialSource::Env { name: "TEST_ANTHROPIC_KEY".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
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
            .create(&InputMeta::builder().name("claude-max".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::ClaudeOauth { account_email: "ops@example.com".to_string() },
                source: CredentialSource::Env { name: "TEST_CLAUDE_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Refreshable,
                placement: CredentialPlacementRequirements::default(),
            })
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
            .create(&InputMeta::builder().name("model-api".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Codex,
                source: CredentialSource::Env { name: "TEST_OPENAI_KEY".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
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
    async fn gh_material_authenticates_both_gh_and_git_without_interactive_prompts() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("github".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create credential declaration");
        let secret = "github-test-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_GITHUB_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("gh", "/usr/bin/gh"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered =
            store.prepare("env-a", &BTreeSet::from(["github".to_string()]), runner.clone()).await.expect("prepare GitHub credential");

        assert_eq!(delivered, vec![
            ("GH_TOKEN".to_string(), secret.to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/tmp/flotilla-test-state/credentials/gitconfig".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ]);
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
    async fn gh_and_forgejo_helpers_compose_in_one_environment() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("github".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create GitHub credential declaration");
        create_forgejo_spec(&backend, "lab-forgejo", "https://forgejo.lab", "TEST_FORGEJO_TOKEN").await;
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
            .expect("prepare GitHub and Forgejo credentials")
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
            "[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-forgejo"
        ));
        assert!(gitconfig.contains("# fragment: credential/gh github"));
        assert!(gitconfig.contains("# fragment: credential/forgejo lab-forgejo"));
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
            .create(&InputMeta::builder().name("github".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Gh,
                source: CredentialSource::Env { name: "TEST_GITHUB_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create GitHub credential declaration");
        create_forgejo_spec(&backend, "lab-forgejo", "https://forgejo.lab", "TEST_FORGEJO_TOKEN").await;
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
        store.prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone()).await.expect("prepare Forgejo credential");

        let writes = runner.writes.lock().expect("writes lock");
        let gitconfig = writes
            .iter()
            .rev()
            .find(|(path, _)| path == Path::new("/tmp/flotilla-test-state/credentials/gitconfig"))
            .map(|(_, content)| content)
            .expect("staged shared Git config");
        assert!(gitconfig.contains("[credential \"https://github.com\"]\n\thelper = !gh auth git-credential"));
        assert!(gitconfig.contains(
            "[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-forgejo"
        ));
    }

    #[tokio::test]
    async fn forgejo_material_is_delivered_as_a_protected_file_and_preflighted() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("lab-forgejo".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo {
                    server_url: "https://forgejo.lab".to_string(),
                    username: "flotilla-crew".to_string(),
                },
                source: CredentialSource::Env { name: "TEST_FORGEJO_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create credential declaration");
        let secret = "forgejo-test-token";
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), secret.to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered =
            store.prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone()).await.expect("prepare Forgejo credential");

        assert_eq!(delivered, vec![
            ("FORGEJO_API_URL".to_string(), "https://forgejo.lab/api/v1".to_string()),
            ("FORGEJO_SERVER_URL".to_string(), "https://forgejo.lab".to_string()),
            ("FORGEJO_TOKEN_FILE".to_string(), "/tmp/flotilla-test-state/credentials/lab-forgejo/token".to_string()),
            ("FORGEJO_USERNAME".to_string(), "flotilla-crew".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/tmp/flotilla-test-state/credentials/gitconfig".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ]);
        let writes = runner.writes.lock().expect("writes lock");
        assert_eq!(writes[0], (PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/token"), secret.to_string()));
        assert_eq!(writes[1].0, PathBuf::from("/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-forgejo"));
        assert!(writes[1].1.contains("[ \"$protocol\" = https ]"));
        assert!(writes[1].1.contains("[ \"$host\" = forgejo.lab ]"));
        assert!(writes[1].1.contains("$FORGEJO_USERNAME"));
        assert!(!writes[1].1.contains(secret));
        assert_eq!(
            writes[2],
            (
                PathBuf::from("/tmp/flotilla-test-state/credentials/gitconfig"),
                "# fragment: credential/forgejo lab-forgejo\n[credential \"https://forgejo.lab\"]\n\thelper = !/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-forgejo\n\n# fragment: vessel/crew-identity\n[user]\n\temail = 309902803+flotilla-crew[bot]@users.noreply.github.com\n\n# fragment: vessel/crew-identity\n[user]\n\tname = flotilla-crew[bot]\n".to_string()
            )
        );
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| cmd == "chmod" && args == &["0600", "/tmp/flotilla-test-state/credentials/lab-forgejo/token"]));
        assert!(calls.iter().any(|(cmd, args, _)| {
            cmd == "chmod" && args == &["0700", "/tmp/flotilla-test-state/credentials/lab-forgejo/git-credential-forgejo"]
        }));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "curl"
                && args == &["--config", "-"]
                && String::from_utf8_lossy(input).contains("https://forgejo.lab/api/v1/user")
                && String::from_utf8_lossy(input).contains(secret)
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
            .create(&InputMeta::builder().name("review-store".to_string()).build(), &CredentialSpecSpec {
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
            })
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
        assert!(runner
            .calls
            .lock()
            .expect("calls lock")
            .iter()
            .any(|(command, args, _)| command == "chmod" && args == &["0600", credential_file]));
    }

    async fn create_forgejo_spec(backend: &ResourceBackend, name: &str, server_url: &str, source_env: &str) {
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name(name.to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { server_url: server_url.to_string(), username: "flotilla-crew".to_string() },
                source: CredentialSource::Env { name: source_env.to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create credential declaration");
    }

    #[tokio::test]
    async fn forgejo_server_url_must_be_https() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_forgejo_spec(&backend, "lab-forgejo", "http://forgejo.lab", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect_err("plain-HTTP Forgejo server URL must be rejected");

        assert!(error.contains("must use HTTPS"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty(), "no material may be written for a rejected URL");
    }

    #[tokio::test]
    async fn forgejo_server_url_must_parse() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_forgejo_spec(&backend, "lab-forgejo", "not a url", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect_err("an unparsable Forgejo server URL must be rejected");

        assert!(error.contains("invalid Forgejo server URL"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty(), "no material may be written for a rejected URL");
    }

    #[tokio::test]
    async fn forgejo_helper_and_git_config_agree_on_an_explicit_port() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_forgejo_spec(&backend, "lab-forgejo", "https://forgejo.lab:3000", "TEST_FORGEJO_TOKEN").await;
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_FORGEJO_TOKEN".to_string(), "forgejo-test-token".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(backend, "flotilla", env, bag, runner.clone(), PathBuf::from("/tmp/flotilla-test-state"));

        let delivered: BTreeMap<String, String> = store
            .prepare("env-a", &BTreeSet::from(["lab-forgejo".to_string()]), runner.clone())
            .await
            .expect("prepare Forgejo credential with an explicit port")
            .into_iter()
            .collect();

        assert_eq!(delivered.get("FORGEJO_SERVER_URL"), Some(&"https://forgejo.lab:3000".to_string()));
        assert_eq!(delivered.get("GIT_CONFIG_GLOBAL"), Some(&"/tmp/flotilla-test-state/credentials/gitconfig".to_string()));
        let writes = runner.writes.lock().expect("writes lock");
        assert!(
            writes[1].1.contains("[ \"$host\" = forgejo.lab:3000 ]"),
            "helper must compare against the host:port form git passes when a port is present"
        );
        assert!(writes[2].1.contains("[credential \"https://forgejo.lab:3000\"]"));
    }

    #[tokio::test]
    async fn a_second_credential_on_the_same_adapter_fails_loudly() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        create_forgejo_spec(&backend, "lab-a", "https://forgejo.lab", "TEST_TOKEN_A").await;
        create_forgejo_spec(&backend, "lab-b", "https://other.lab", "TEST_TOKEN_B").await;
        let runner = Arc::new(RecordingRunner::default());
        let bag = EnvironmentBag::new().with(EnvironmentAssertion::binary("curl", "/usr/bin/curl"));
        let store = CredentialStore::new(
            backend,
            "flotilla",
            Arc::new(TestEnv::default()),
            bag,
            runner.clone(),
            PathBuf::from("/tmp/flotilla-test-state"),
        );

        let error = store
            .prepare("env-a", &BTreeSet::from(["lab-a".to_string(), "lab-b".to_string()]), runner.clone())
            .await
            .expect_err("two credentials on one adapter would silently clobber each other's delivery");

        assert!(error.contains("multiple granted credentials use this adapter"), "unexpected error: {error}");
        assert!(runner.writes.lock().expect("writes lock").is_empty(), "no material may be written when preparation is rejected");
    }

    #[tokio::test]
    async fn registry_config_survives_preflight_until_the_environment_is_forgotten() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("private-registry".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::DockerRegistry { registry: "registry.example".to_string(), username: "crew".to_string() },
                source: CredentialSource::Env { name: "TEST_REGISTRY_TOKEN".to_string() },
                lifecycle: CredentialLifecycle::Static,
                placement: CredentialPlacementRequirements::default(),
            })
            .await
            .expect("create credential declaration");
        let env = Arc::new(TestEnv(BTreeMap::from([("TEST_REGISTRY_TOKEN".to_string(), "registry-secret".to_string())])));
        let runner = Arc::new(RecordingRunner::default());
        let state = tempfile::tempdir().expect("create state directory");
        let store = CredentialStore::new(backend, "flotilla", env, EnvironmentBag::new(), runner.clone(), state.path().to_path_buf());

        let config_dir = store
            .prepare_registry_pull("env-a", &BTreeSet::from(["private-registry".to_string()]), "registry.example/crew:latest")
            .await
            .expect("prepare registry credential")
            .expect("matching registry credential");

        assert!(config_dir.is_dir(), "credential config must remain available to docker run");
        assert_eq!(
            std::fs::metadata(&config_dir).expect("credential config metadata").permissions().mode() & 0o777,
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
}
