use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_core::providers::{
    discovery::{EnvVars, EnvironmentBag},
    ChannelLabel, CommandRunner, HttpClient, ReqwestHttpClient,
};
use flotilla_resources::{
    CredentialConsumer, CredentialLifecycle, CredentialSource, CredentialSpec, CredentialSpecSpec, Repository, RepositoryKey,
    ResourceBackend, ResourceError,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

use crate::vessel_config::{compose, Fragment, GitConfigKey, Merge, Provenance, TargetId, TargetKey};

const GIT_CONFIG_PATH: &str = "/run/flotilla/credentials/gitconfig";

#[derive(Serialize)]
struct GithubAppJwtClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Serialize)]
struct GithubAppTokenRequest {
    repositories: Vec<String>,
}

#[derive(Deserialize)]
struct GithubAppTokenResponse {
    token: String,
}

pub(crate) struct CredentialStore {
    backend: ResourceBackend,
    namespace: String,
    env: Arc<dyn EnvVars>,
    host_bag: EnvironmentBag,
    host_runner: Arc<dyn CommandRunner>,
    http: Arc<dyn HttpClient>,
    state_dir: PathBuf,
    prepared: Mutex<BTreeSet<(String, String)>>,
    materials: Mutex<BTreeMap<(String, String), String>>,
    git_config_fragments: Mutex<BTreeMap<String, BTreeMap<String, Fragment>>>,
    registry_configs: Mutex<BTreeMap<String, PathBuf>>,
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
    Forgejo { host: String, token_file: String, username: String },
}

impl GitCredentialPreflight {
    async fn run(&self, runner: &dyn CommandRunner, material: &str) -> Result<(), String> {
        match self {
            Self::Gh => {
                runner
                    .run_with_input(
                        "sh",
                        &[
                            "-c",
                            "IFS= read -r token; export GH_TOKEN=\"$token\" GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0; printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill >/dev/null",
                            "flotilla-gh-git-preflight",
                            GIT_CONFIG_PATH,
                        ],
                        Path::new("/"),
                        &ChannelLabel::Noop,
                        material.as_bytes(),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("Git credential preflight failed: {error}"))
            }
            Self::Forgejo { host, token_file, username } => runner
                .run(
                    "sh",
                    &[
                        "-c",
                        "export GIT_CONFIG_GLOBAL=\"$1\" GIT_TERMINAL_PROMPT=0 FORGEJO_TOKEN_FILE=\"$2\" FORGEJO_USERNAME=\"$3\"; printf 'protocol=https\\nhost=%s\\n\\n' \"$4\" | git credential fill >/dev/null",
                        "flotilla-forgejo-git-preflight",
                        GIT_CONFIG_PATH,
                        token_file,
                        username,
                        host,
                    ],
                    Path::new("/"),
                    &ChannelLabel::Noop,
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
        Self {
            backend,
            namespace: namespace.to_string(),
            env,
            host_bag,
            host_runner,
            http,
            state_dir,
            prepared: Mutex::new(BTreeSet::new()),
            materials: Mutex::new(BTreeMap::new()),
            git_config_fragments: Mutex::new(BTreeMap::new()),
            registry_configs: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn consumer_adapters(&self, credential_refs: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
        let mut adapters = BTreeSet::new();
        for name in credential_refs {
            adapters.insert(self.spec(name).await?.consumer.adapter_name().to_string());
        }
        Ok(adapters)
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
            let material = if spec.lifecycle == CredentialLifecycle::Refreshable {
                self.resolve_for_adapter(name, spec, credential_scopes.get(name)).await?
            } else if let Some(material) = cached_material {
                material
            } else {
                let material = self.resolve_for_adapter(name, spec, credential_scopes.get(name)).await?;
                self.materials.lock().await.insert(cache_key.clone(), material.clone());
                material
            };
            let already_prepared = spec.lifecycle != CredentialLifecycle::Refreshable && self.prepared.lock().await.contains(&cache_key);
            let material = material.trim_end();
            if let Err(error) = validate_scalar_material(name, spec.consumer.adapter_name(), material) {
                self.materials.lock().await.remove(&cache_key);
                return Err(error);
            }
            let delivered = match self.prepare_adapter(name, spec, material, Arc::clone(&runner), already_prepared).await {
                Ok(delivered) => delivered,
                Err(message) => {
                    self.materials.lock().await.remove(&cache_key);
                    return Err(bounded_adapter_error(name, spec.consumer.adapter_name(), &message.replace(material, "[redacted]")));
                }
            };
            env.extend(delivered.env);
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
            let gitconfig = match compose(TargetId::GitConfig, composed_fragments.values().cloned()) {
                Ok(gitconfig) => gitconfig,
                Err(error) => {
                    let (name, adapter, cache_key) = git_config_owner.expect("Git config fragments have an owner");
                    self.materials.lock().await.remove(&cache_key);
                    return Err(bounded_adapter_error(&name, &adapter, &format!("compose shared Git config: {error}")));
                }
            };
            if let Err(error) = runner.write_file(Path::new(GIT_CONFIG_PATH), &gitconfig.contents).await {
                let (name, adapter, cache_key) = git_config_owner.expect("Git config fragments have an owner");
                self.materials.lock().await.remove(&cache_key);
                return Err(bounded_adapter_error(&name, &adapter, &format!("write shared Git config: {error}")));
            }
            env.insert("GIT_CONFIG_GLOBAL".to_string(), GIT_CONFIG_PATH.to_string());
            env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
            for pending in pending_git_preflights {
                if let Err(message) = pending.preflight.run(&*runner, &pending.material).await {
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
        let material = self.resolve_for_adapter(&name, &spec, None).await?;
        let material = material.trim_end();
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
                    &ChannelLabel::Noop,
                    material.as_bytes(),
                )
                .await
                .map_err(|error| format!("login preflight failed: {}", error.replace(material, "[redacted]")))?;
            self.host_runner
                .run("docker", &["--config", &config, "pull", image], Path::new("/"), &ChannelLabel::Noop)
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
        let config_dir = self.registry_configs.lock().await.remove(environment_ref);
        if let Some(config_dir) = config_dir {
            remove_registry_config(&config_dir).await.map_err(|error| format!("remove Docker credential cache: {error}"))?;
        }
        Ok(())
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
                    .run(command, &args, Path::new("/"), &ChannelLabel::Noop)
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
    ) -> Result<String, String> {
        let result = match (&spec.consumer, &spec.source) {
            (CredentialConsumer::GithubApp { installation_id }, CredentialSource::GithubApp { app_id_path, private_key_path }) => {
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
                self.mint_github_app_token(*installation_id, app_id_path, private_key_path, repository_scope).await
            }
            (CredentialConsumer::GithubApp { .. }, _) => Err("github-app consumer requires a github-app source".to_string()),
            (_, CredentialSource::GithubApp { .. }) => Err("github-app source requires a github-app consumer".to_string()),
            _ => self.resolve(name, spec).await,
        };
        result.map_err(|error| bounded_adapter_error(name, spec.consumer.adapter_name(), &error))
    }

    async fn mint_github_app_token(
        &self,
        installation_id: u64,
        app_id_path: &str,
        private_key_path: &str,
        repository_scope: &BTreeSet<RepositoryKey>,
    ) -> Result<String, String> {
        let app_id =
            tokio::fs::read_to_string(self.expand_path(app_id_path)).await.map_err(|error| format!("read host-local App id: {error}"))?;
        let private_key =
            tokio::fs::read(self.expand_path(private_key_path)).await.map_err(|error| format!("read host-local private key: {error}"))?;
        let now = chrono::Utc::now().timestamp();
        let claims = GithubAppJwtClaims { iat: now - 60, exp: now + 9 * 60, iss: app_id.trim().to_string() };
        if claims.iss.is_empty() {
            return Err("host-local App id is empty".to_string());
        }
        let key = EncodingKey::from_rsa_pem(&private_key).map_err(|error| format!("decode host-local private key: {error}"))?;
        let jwt =
            jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|error| format!("sign GitHub App JWT: {error}"))?;
        let repositories = self.github_repository_names(repository_scope).await?;
        let url = format!("https://api.github.com/app/installations/{installation_id}/access_tokens");
        let request = flotilla_resources::tls::client()
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&GithubAppTokenRequest { repositories })
            .build()
            .map_err(|error| format!("build installation token request: {error}"))?;
        let label = ChannelLabel::http_from_url(&url);
        let response = self.http.execute(request, &label).await.map_err(|error| format!("mint installation token: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("mint installation token: GitHub returned HTTP {}", response.status()));
        }
        let response: GithubAppTokenResponse =
            serde_json::from_slice(response.body()).map_err(|error| format!("decode installation token response: {error}"))?;
        if response.token.trim().is_empty() {
            return Err("installation token response was empty".to_string());
        }
        Ok(response.token)
    }

    async fn github_repository_names(&self, repository_scope: &BTreeSet<RepositoryKey>) -> Result<Vec<String>, String> {
        if repository_scope.len() > 500 {
            return Err("GitHub App repository scope exceeds the 500-repository API limit".to_string());
        }
        let repositories = self
            .backend
            .clone()
            .using::<Repository>(&self.namespace)
            .list()
            .await
            .map_err(|error| format!("list repository identities: {error}"))?;
        let mut names = Vec::with_capacity(repository_scope.len());
        for key in repository_scope {
            let repository = repositories
                .items
                .iter()
                .find(|repository| repository.spec.key() == *key)
                .ok_or_else(|| format!("repository scope references missing repository `{key}`"))?;
            let forge = repository.spec.forge().ok_or_else(|| format!("repository `{key}` has no forge identity"))?;
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
        path.strip_prefix("~/")
            .and_then(|relative| self.env.get("HOME").map(|home| PathBuf::from(home).join(relative)))
            .unwrap_or_else(|| PathBuf::from(path))
    }

    async fn prepare_adapter(
        &self,
        name: &str,
        spec: &CredentialSpecSpec,
        material: &str,
        runner: Arc<dyn CommandRunner>,
        already_prepared: bool,
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
                            &ChannelLabel::Noop,
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
                runner
                    .run_with_input(
                        "sh",
                        &["-c", "IFS= read -r token; GH_TOKEN=\"$token\" gh api installation/repositories --silent"],
                        Path::new("/"),
                        &ChannelLabel::Noop,
                        material.as_bytes(),
                    )
                    .await
                    .map_err(|error| format!("installation authentication preflight failed: {error}"))?;
                env.insert("GH_TOKEN".to_string(), material.to_string());
                git_credential = Some(GitCredentialContribution {
                    fragment: git_credential_fragment(name, "github-app", "https://github.com", "!gh auth git-credential"),
                    preflight: Some(GitCredentialPreflight::Gh),
                });
            }
            CredentialConsumer::Forgejo { api_url, username } => {
                let server_url = api_url.trim_end_matches('/');
                let parsed_url = Url::parse(server_url).map_err(|error| format!("invalid Forgejo server URL: {error}"))?;
                if parsed_url.scheme() != "https" {
                    return Err("Forgejo server URL must use HTTPS".to_string());
                }
                let host = parsed_url.host_str().ok_or_else(|| "Forgejo server URL has no host".to_string())?;
                let credential_url = match parsed_url.port() {
                    Some(port) => format!("https://{host}:{port}"),
                    None => format!("https://{host}"),
                };
                let path = format!("/run/flotilla/credentials/{}/token", safe_component(name));
                let helper_path = format!("/run/flotilla/credentials/{}/git-credential-forgejo", safe_component(name));
                if !already_prepared {
                    runner.write_file(Path::new(&path), material).await.map_err(|error| format!("write token file: {error}"))?;
                    runner
                        .run("chmod", &["0600", &path], Path::new("/"), &ChannelLabel::Noop)
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
                        .run("chmod", &["0700", &helper_path], Path::new("/"), &ChannelLabel::Noop)
                        .await
                        .map_err(|error| format!("protect Git credential helper: {error}"))?;
                    let url = format!("{server_url}/api/v1/user");
                    let curl_config = format!(
                        "silent\nshow-error\nfail\nheader = \"Authorization: token {}\"\nurl = \"{}\"\n",
                        sanitize_curl_config(material),
                        sanitize_curl_config(&url)
                    );
                    runner
                        .run_with_input("curl", &["--config", "-"], Path::new("/"), &ChannelLabel::Noop, curl_config.as_bytes())
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
            CredentialConsumer::Codex => {
                let codex_home = format!("/run/flotilla/credentials/{}/codex", safe_component(name));
                if !already_prepared {
                    runner
                        .run("mkdir", &["-p", &codex_home], Path::new("/"), &ChannelLabel::Noop)
                        .await
                        .map_err(|error| format!("create writable login cache: {error}"))?;
                    runner
                        .run_with_input(
                            "sh",
                            &["-c", "CODEX_HOME=\"$1\" codex login --with-api-key", "flotilla-codex-login", &codex_home],
                            Path::new("/"),
                            &ChannelLabel::Noop,
                            material.as_bytes(),
                        )
                        .await
                        .map_err(|error| format!("login transformation failed: {error}"))?;
                    runner
                        .run(
                            "sh",
                            &["-c", "CODEX_HOME=\"$1\" codex login status", "flotilla-codex-status", &codex_home],
                            Path::new("/"),
                            &ChannelLabel::Noop,
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
            CredentialConsumer::DockerRegistry { .. } => {}
        }
        Ok(AdapterDelivery { env, git_credential })
    }
}

fn git_credential_fragment(credential_name: &str, adapter: &str, credential_url: impl Into<String>, helper: impl Into<String>) -> Fragment {
    Fragment::new(
        TargetId::GitConfig,
        TargetKey::GitConfig(GitConfigKey::subsection("credential", credential_url, "helper")),
        helper,
        Provenance::new(format!("credential/{adapter} {credential_name}")),
    )
    .with_merge(Merge::Append)
}

async fn api_key_preflight(runner: &dyn CommandRunner, url: &str, headers: &[(&str, &str)]) -> Result<(), String> {
    let mut config = "silent\nshow-error\nfail\n".to_string();
    for (name, value) in headers {
        config.push_str(&format!("header = \"{}: {}\"\n", sanitize_curl_config(name), sanitize_curl_config(value)));
    }
    config.push_str(&format!("url = \"{}\"\n", sanitize_curl_config(url)));
    runner
        .run_with_input("curl", &["--config", "-"], Path::new("/"), &ChannelLabel::Noop, config.as_bytes())
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

fn sanitize_curl_config(value: &str) -> String {
    value.replace(['\\', '"', '\r', '\n'], "")
}

fn safe_component(name: &str) -> String {
    name.chars().map(|character| if character.is_ascii_alphanumeric() || character == '-' { character } else { '-' }).collect()
}

fn image_registry_matches(image: &str, registry: &str) -> bool {
    image == registry || image.strip_prefix(registry).is_some_and(|remainder| remainder.starts_with('/'))
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
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use flotilla_core::providers::{
        discovery::EnvironmentAssertion,
        replay::{Masks, ReplayHttpClient, Session},
        CommandOutput,
    };
    use flotilla_protocol::NodeId;
    use flotilla_resources::{CredentialPlacementRequirements, InMemoryBackend, InputMeta, RepositorySpec};

    use super::*;

    #[derive(Default)]
    struct TestEnv(BTreeMap<String, String>);

    impl EnvVars for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    type RecordedCall = (String, Vec<String>, Vec<u8>);

    #[derive(Default)]
    struct RecordingRunner {
        calls: StdMutex<Vec<RecordedCall>>,
        writes: StdMutex<Vec<(PathBuf, String)>>,
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls.lock().expect("calls lock").push((cmd.to_string(), args.iter().map(|arg| (*arg).to_string()).collect(), Vec::new()));
            Ok(String::new())
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| CommandOutput { stdout, stderr: String::new(), success: true })
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

    #[tokio::test]
    async fn github_app_mints_a_repository_scoped_token_on_every_prepare_through_replayed_http() {
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
                consumer: CredentialConsumer::GithubApp { installation_id: 9876 },
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

        let first = store.prepare_scoped("env-a", &refs, &scopes, runner.clone()).await.expect("first preparation");
        let second = store.prepare_scoped("env-a", &refs, &scopes, runner.clone()).await.expect("second preparation");

        assert!(first.contains(&("GH_TOKEN".to_string(), "installation-token-one".to_string())));
        assert!(second.contains(&("GH_TOKEN".to_string(), "installation-token-two".to_string())));
        let calls = runner.calls.lock().expect("calls lock");
        assert_eq!(
            calls
                .iter()
                .filter(|(command, args, _)| {
                    command == "sh" && args.iter().any(|arg| arg.contains("gh api installation/repositories --silent"))
                })
                .count(),
            2,
        );
        session.assert_complete();
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
            consumer: CredentialConsumer::GithubApp { installation_id: 9876 },
            source: CredentialSource::GithubApp {
                app_id_path: "/not-read/github-app.id".to_string(),
                private_key_path: "/not-read/github-app.pem".to_string(),
            },
            lifecycle: CredentialLifecycle::Refreshable,
            placement: CredentialPlacementRequirements::default(),
        };

        for scope in [None, Some(&BTreeSet::new())] {
            let error = store.resolve_for_adapter("github-app", &spec, scope).await.expect_err("empty scopes must fail");
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
            .resolve_for_adapter("github-app", &static_spec, Some(&non_empty_scope))
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

        let delivered =
            store.prepare("env-a", &BTreeSet::from(["model-api".to_string()]), runner.clone()).await.expect("prepare codex credential");

        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, "CODEX_HOME");
        assert!(!delivered.iter().any(|(name, value)| name == "OPENAI_API_KEY" || value == secret));
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg == "CODEX_HOME=\"$1\" codex login --with-api-key") && input == secret.as_bytes()
        }));
        assert!(calls.iter().any(|(cmd, args, _)| cmd == "sh" && args.iter().any(|arg| arg.contains("codex login status"))));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
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
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/flotilla/credentials/gitconfig".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ]);
        let writes = runner.writes.lock().expect("writes lock");
        assert_eq!(writes.as_slice(), &[(
            PathBuf::from("/run/flotilla/credentials/gitconfig"),
            "# fragment: credential/gh github\n[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n".to_string()
        )]);
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg.contains("gh api user --silent")) && input == secret.as_bytes()
        }));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "sh" && args.iter().any(|arg| arg.contains("GIT_CONFIG_GLOBAL")) && input == secret.as_bytes()
        }));
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

        assert_eq!(delivered.get("GIT_CONFIG_GLOBAL"), Some(&"/run/flotilla/credentials/gitconfig".to_string()));
        assert_eq!(delivered.get("GIT_TERMINAL_PROMPT"), Some(&"0".to_string()));
        assert!(!delivered
            .keys()
            .any(|key| key == "GIT_CONFIG_COUNT" || key.starts_with("GIT_CONFIG_KEY_") || key.starts_with("GIT_CONFIG_VALUE_")));
        let writes = runner.writes.lock().expect("writes lock");
        let gitconfig = writes
            .iter()
            .rev()
            .find(|(path, _)| path == Path::new("/run/flotilla/credentials/gitconfig"))
            .map(|(_, content)| content)
            .expect("staged shared Git config");
        assert!(gitconfig.contains("[credential \"https://github.com\"]\n\thelper = !gh auth git-credential"));
        assert!(gitconfig
            .contains("[credential \"https://forgejo.lab\"]\n\thelper = !/run/flotilla/credentials/lab-forgejo/git-credential-forgejo"));
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
            .find(|(path, _)| path == Path::new("/run/flotilla/credentials/gitconfig"))
            .map(|(_, content)| content)
            .expect("staged shared Git config");
        assert!(gitconfig.contains("[credential \"https://github.com\"]\n\thelper = !gh auth git-credential"));
        assert!(gitconfig
            .contains("[credential \"https://forgejo.lab\"]\n\thelper = !/run/flotilla/credentials/lab-forgejo/git-credential-forgejo"));
    }

    #[tokio::test]
    async fn forgejo_material_is_delivered_as_a_protected_file_and_preflighted() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("lab-forgejo".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { api_url: "https://forgejo.lab".to_string(), username: "flotilla-crew".to_string() },
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
            ("FORGEJO_TOKEN_FILE".to_string(), "/run/flotilla/credentials/lab-forgejo/token".to_string()),
            ("FORGEJO_USERNAME".to_string(), "flotilla-crew".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/flotilla/credentials/gitconfig".to_string()),
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ]);
        let writes = runner.writes.lock().expect("writes lock");
        assert_eq!(writes[0], (PathBuf::from("/run/flotilla/credentials/lab-forgejo/token"), secret.to_string()));
        assert_eq!(writes[1].0, PathBuf::from("/run/flotilla/credentials/lab-forgejo/git-credential-forgejo"));
        assert!(writes[1].1.contains("[ \"$protocol\" = https ]"));
        assert!(writes[1].1.contains("[ \"$host\" = forgejo.lab ]"));
        assert!(writes[1].1.contains("$FORGEJO_USERNAME"));
        assert!(!writes[1].1.contains(secret));
        assert_eq!(
            writes[2],
            (
                PathBuf::from("/run/flotilla/credentials/gitconfig"),
                "# fragment: credential/forgejo lab-forgejo\n[credential \"https://forgejo.lab\"]\n\thelper = !/run/flotilla/credentials/lab-forgejo/git-credential-forgejo\n".to_string()
            )
        );
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, _)| cmd == "chmod" && args == &["0600", "/run/flotilla/credentials/lab-forgejo/token"]));
        assert!(calls
            .iter()
            .any(|(cmd, args, _)| { cmd == "chmod" && args == &["0700", "/run/flotilla/credentials/lab-forgejo/git-credential-forgejo"] }));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "curl"
                && args == &["--config", "-"]
                && String::from_utf8_lossy(input).contains("https://forgejo.lab/api/v1/user")
                && String::from_utf8_lossy(input).contains(secret)
        }));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
    }

    async fn create_forgejo_spec(backend: &ResourceBackend, name: &str, api_url: &str, source_env: &str) {
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name(name.to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { api_url: api_url.to_string(), username: "flotilla-crew".to_string() },
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
        assert_eq!(delivered.get("GIT_CONFIG_GLOBAL"), Some(&"/run/flotilla/credentials/gitconfig".to_string()));
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
