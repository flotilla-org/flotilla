use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_core::providers::{
    discovery::{EnvVars, EnvironmentBag},
    ChannelLabel, CommandRunner,
};
use flotilla_resources::{
    CredentialConsumer, CredentialLifecycle, CredentialSource, CredentialSpec, CredentialSpecSpec, ResourceBackend, ResourceError,
};
use tokio::sync::Mutex;

pub(crate) struct CredentialStore {
    backend: ResourceBackend,
    namespace: String,
    env: Arc<dyn EnvVars>,
    host_bag: EnvironmentBag,
    host_runner: Arc<dyn CommandRunner>,
    state_dir: PathBuf,
    prepared: Mutex<BTreeSet<(String, String)>>,
    materials: Mutex<BTreeMap<(String, String), String>>,
    registry_configs: Mutex<BTreeMap<String, PathBuf>>,
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
        Self {
            backend,
            namespace: namespace.to_string(),
            env,
            host_bag,
            host_runner,
            state_dir,
            prepared: Mutex::new(BTreeSet::new()),
            materials: Mutex::new(BTreeMap::new()),
            registry_configs: Mutex::new(BTreeMap::new()),
        }
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

    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        credential_refs: &BTreeSet<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut env = BTreeMap::new();
        for name in credential_refs {
            let spec = self.spec(name).await?;
            if matches!(spec.consumer, CredentialConsumer::DockerRegistry { .. }) {
                continue;
            }
            let cache_key = (environment_ref.to_string(), name.clone());
            let cached_material = {
                let materials = self.materials.lock().await;
                materials.get(&cache_key).cloned()
            };
            // Issued material is minted once per environment and evicted with
            // that environment. Refreshable material is resolved for every
            // preparation; static material follows the same environment cache.
            let material = if spec.lifecycle == CredentialLifecycle::Refreshable {
                self.resolve_for_adapter(name, &spec).await?
            } else if let Some(material) = cached_material {
                material
            } else {
                let material = self.resolve_for_adapter(name, &spec).await?;
                self.materials.lock().await.insert(cache_key.clone(), material.clone());
                material
            };
            let already_prepared = spec.lifecycle != CredentialLifecycle::Refreshable && self.prepared.lock().await.contains(&cache_key);
            let material = material.trim_end();
            if let Err(error) = validate_scalar_material(name, spec.consumer.adapter_name(), material) {
                self.materials.lock().await.remove(&cache_key);
                return Err(error);
            }
            let delivered = match self.prepare_adapter(name, &spec, material, Arc::clone(&runner), already_prepared).await {
                Ok(delivered) => delivered,
                Err(message) => {
                    self.materials.lock().await.remove(&cache_key);
                    return Err(bounded_adapter_error(name, spec.consumer.adapter_name(), &message.replace(material, "[redacted]")));
                }
            };
            env.extend(delivered);
            self.prepared.lock().await.insert(cache_key);
        }
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
        let material = self.resolve_for_adapter(&name, &spec).await?;
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
        };
        if material.trim().is_empty() {
            return Err(format!("credential `{name}` source produced empty material"));
        }
        Ok(material)
    }

    async fn resolve_for_adapter(&self, name: &str, spec: &CredentialSpecSpec) -> Result<String, String> {
        self.resolve(name, spec).await.map_err(|error| bounded_adapter_error(name, spec.consumer.adapter_name(), &error))
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
    ) -> Result<BTreeMap<String, String>, String> {
        let mut env = BTreeMap::new();
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
            }
            CredentialConsumer::Forgejo { api_url } => {
                let path = format!("/run/flotilla/credentials/{}/token", safe_component(name));
                if !already_prepared {
                    runner.write_file(Path::new(&path), material).await.map_err(|error| format!("write token file: {error}"))?;
                    runner
                        .run("chmod", &["0600", &path], Path::new("/"), &ChannelLabel::Noop)
                        .await
                        .map_err(|error| format!("protect token file: {error}"))?;
                    let url = format!("{}/api/v1/user", api_url.trim_end_matches('/'));
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
                env.insert("FORGEJO_TOKEN_FILE".to_string(), path);
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
        Ok(env)
    }
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
    use flotilla_core::providers::{discovery::EnvironmentAssertion, CommandOutput};
    use flotilla_protocol::NodeId;
    use flotilla_resources::{CredentialPlacementRequirements, InMemoryBackend, InputMeta};

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

        store.forget_environment("env-a").await.expect("forget environment");

        assert_eq!(store.prepared.lock().await.clone(), BTreeSet::from([("env-b".to_string(), "model-api".to_string())]));
        assert_eq!(
            store.materials.lock().await.clone(),
            BTreeMap::from([(("env-b".to_string(), "model-api".to_string()), "secret-b".to_string())])
        );
    }

    #[tokio::test]
    async fn forgejo_material_is_delivered_as_a_protected_file_and_preflighted() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        backend
            .clone()
            .definitions::<CredentialSpec>("flotilla")
            .create(&InputMeta::builder().name("lab-forgejo".to_string()).build(), &CredentialSpecSpec {
                consumer: CredentialConsumer::Forgejo { api_url: "https://forgejo.lab".to_string() },
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

        assert_eq!(delivered, vec![("FORGEJO_TOKEN_FILE".to_string(), "/run/flotilla/credentials/lab-forgejo/token".to_string(),)]);
        assert_eq!(runner.writes.lock().expect("writes lock").as_slice(), &[(
            PathBuf::from("/run/flotilla/credentials/lab-forgejo/token"),
            secret.to_string()
        )]);
        let calls = runner.calls.lock().expect("calls lock");
        assert!(calls.iter().any(|(cmd, args, _)| cmd == "chmod" && args == &["0600", "/run/flotilla/credentials/lab-forgejo/token"]));
        assert!(calls.iter().any(|(cmd, args, input)| {
            cmd == "curl"
                && args == &["--config", "-"]
                && String::from_utf8_lossy(input).contains("https://forgejo.lab/api/v1/user")
                && String::from_utf8_lossy(input).contains(secret)
        }));
        assert!(calls.iter().flat_map(|(_, args, _)| args).all(|arg| !arg.contains(secret)));
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
