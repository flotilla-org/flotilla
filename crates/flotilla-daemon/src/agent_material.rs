use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::providers::{
    discovery::EnvVars,
    environment::{
        runner::{CONTAINED_CODEX_HOME, CONTAINED_WRITABLE_CONFIG_BASE},
        ProvisionedMount, ProvisionedMountMode,
    },
    ChannelLabel, CommandRunner,
};
use flotilla_protocol::ResourceRef;
use flotilla_resources::{api_version, Environment, MaterialPoolSpec, MaterialPoolUnitSpec, Resource, ResourceBackend};
use tokio::fs;
use tracing::{info, warn};

use crate::{
    material_pool::{MaterialLeaseOutcome, MaterialPoolManager},
    vessel_config::{agent_environment_fragment, Fragment},
};

const CODEX_ADAPTER_ID: &str = "codex";
const CLAUDE_CODE_ADAPTER_ID: &str = "claude-code";
const CODEX_POOL_REF: &str = "codex-login";
pub(crate) const FLOTILLA_SKILLS_DIR_ENV: &str = "FLOTILLA_SKILLS_DIR";
const SKILL_BUNDLE_MANIFEST: &str = ".flotilla-sources.json";
const CONTAINER_SKILLS_SOURCE: &str = "/run/flotilla/skills";
const PRIVATE_SKILL_REPOSITORY: &str = "mattpocock-skills";
const PUBLIC_SKILL_REPOSITORY: &str = "rjw-skills";
pub(crate) const CONTAINER_CODEX_HOME: &str = CONTAINED_CODEX_HOME;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMaterialPreflight {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) failure_context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMaterialDelivery {
    pub(crate) mount: ProvisionedMount,
    pub(crate) preflight: AgentMaterialPreflight,
    pub(crate) github_repository_grants: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMaterialOutcome {
    NotRequired,
    Ready(AgentMaterialDelivery),
    Waiting { pool_ref: String, message: String },
}

#[async_trait]
trait AgentMaterialAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn pool_ref(&self) -> Option<&'static str>;
    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment>;
    fn config_home_variable(&self) -> &'static str;
    fn is_managed_config_home(&self, config_home: &Path, config_base: &Path) -> bool;
    fn externally_managed_home_opts_out_of_skills(&self) -> bool;

    fn skill_destination(&self, environment: &[(String, String)], config_base: &Path) -> Result<Option<PathBuf>, String> {
        let variable = self.config_home_variable();
        let Some(config_home) = environment.iter().find(|(name, _)| name == variable).map(|(_, value)| PathBuf::from(value)) else {
            return if self.externally_managed_home_opts_out_of_skills() {
                Ok(None)
            } else {
                Err(format!("contained {} skill staging requires seam-resolved `{variable}`", self.id()))
            };
        };
        if !self.is_managed_config_home(&config_home, config_base) {
            return if self.externally_managed_home_opts_out_of_skills() {
                Ok(None)
            } else {
                Err(format!("contained {} skill target {} is outside {}", self.id(), config_home.display(), config_base.display()))
            };
        }
        Ok(Some(config_home.join("skills")))
    }

    fn requires_skills_for_prepare(&self, environment: &BTreeMap<String, String>) -> bool {
        !self.externally_managed_home_opts_out_of_skills() || !environment.contains_key(self.config_home_variable())
    }

    async fn prepare(&self, holder_ref: &ResourceRef, environment: &BTreeMap<String, String>) -> Result<AgentMaterialOutcome, String>;
}

pub(crate) struct AgentMaterialRegistry {
    namespace: String,
    pools: Arc<MaterialPoolManager>,
    adapters: BTreeMap<&'static str, Arc<dyn AgentMaterialAdapter>>,
    skills: SkillBundle,
}

impl AgentMaterialRegistry {
    pub(crate) fn new(backend: ResourceBackend, namespace: &str, env: Arc<dyn EnvVars>) -> Self {
        let pools = Arc::new(MaterialPoolManager::new(backend, namespace));
        let pool_dir = env
            .get("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"))
            .join(".config/flotilla/credentials/codex-pool");
        let skills = SkillBundle::new(env.get(FLOTILLA_SKILLS_DIR_ENV).map(PathBuf::from));
        let codex: Arc<dyn AgentMaterialAdapter> =
            Arc::new(CodexMaterialAdapter::new(Arc::clone(&pools), pool_dir, cfg!(any(target_os = "linux", test))));
        let claude_code: Arc<dyn AgentMaterialAdapter> = Arc::new(ClaudeCodeMaterialAdapter);
        Self {
            namespace: namespace.to_string(),
            pools,
            adapters: BTreeMap::from([(codex.id(), codex), (claude_code.id(), claude_code)]),
            skills,
        }
    }

    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        required_adapters: &BTreeSet<String>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Vec<AgentMaterialDelivery>, AgentMaterialPrepareError> {
        let holder_ref = self.holder_ref(environment_ref);
        let mut deliveries = Vec::new();
        for adapter_id in required_adapters {
            let Some(adapter) = self.adapters.get(adapter_id.as_str()) else {
                continue;
            };
            match adapter.prepare(&holder_ref, environment).await {
                Ok(AgentMaterialOutcome::NotRequired) => {}
                Ok(AgentMaterialOutcome::Ready(delivery)) => deliveries.push(delivery),
                Ok(AgentMaterialOutcome::Waiting { pool_ref, message }) => {
                    return Err(AgentMaterialPrepareError::Waiting { pool_ref, message });
                }
                Err(message) => return Err(AgentMaterialPrepareError::Failed(message)),
            }
        }
        if required_adapters
            .iter()
            .filter_map(|adapter_id| self.adapters.get(adapter_id.as_str()))
            .any(|adapter| adapter.requires_skills_for_prepare(environment))
        {
            let delivery = self.skills.prepare().await.map_err(AgentMaterialPrepareError::Failed)?;
            deliveries.push(delivery);
        }
        Ok(deliveries)
    }

    pub(crate) async fn stage_skills(
        &self,
        environment_ref: &str,
        required_adapters: &BTreeSet<String>,
        environment: &[(String, String)],
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        let adapters = required_adapters.iter().filter_map(|adapter_id| self.adapters.get(adapter_id.as_str())).collect::<Vec<_>>();
        self.skills.stage(environment_ref, &adapters, environment, runner).await
    }

    pub(crate) fn fragments(&self, required_adapters: &BTreeSet<String>, environment: &BTreeMap<String, String>) -> Vec<Fragment> {
        required_adapters
            .iter()
            .filter_map(|adapter_id| self.adapters.get(adapter_id.as_str()))
            .filter_map(|adapter| adapter.fragment(environment))
            .collect()
    }

    pub(crate) async fn release(&self, environment_ref: &str) -> Result<(), String> {
        self.pools.release_holder(&self.holder_ref(environment_ref)).await
    }

    pub(crate) async fn recover(&self, active_environment_refs: impl IntoIterator<Item = String>) -> Result<(), String> {
        let active = active_environment_refs.into_iter().map(|name| self.holder_ref(&name)).collect::<HashSet<_>>();
        let pool_refs = self.adapters.values().filter_map(|adapter| adapter.pool_ref().map(str::to_string)).collect::<BTreeSet<_>>();
        self.pools.recover(&pool_refs, &active).await
    }

    fn holder_ref(&self, environment_ref: &str) -> ResourceRef {
        ResourceRef::new(api_version(Environment::API_PATHS), Environment::API_PATHS.kind, &self.namespace, environment_ref)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentMaterialPrepareError {
    Waiting { pool_ref: String, message: String },
    Failed(String),
}

struct CodexMaterialAdapter {
    pools: Arc<MaterialPoolManager>,
    pool_dir: PathBuf,
    supported: bool,
}

impl CodexMaterialAdapter {
    fn new(pools: Arc<MaterialPoolManager>, pool_dir: PathBuf, supported: bool) -> Self {
        Self { pools, pool_dir, supported }
    }

    async fn usable_units(&self) -> Result<BTreeMap<String, MaterialPoolUnitSpec>, String> {
        let mut entries = match fs::read_dir(&self.pool_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(format!("read Codex login pool {}: {error}", self.pool_dir.display())),
        };
        let mut numbered: BTreeMap<u64, PathBuf> = BTreeMap::new();
        while let Some(entry) =
            entries.next_entry().await.map_err(|error| format!("read Codex login pool {}: {error}", self.pool_dir.display()))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(number) = name.strip_prefix("slot-").and_then(|suffix| suffix.parse::<u64>().ok()) else {
                continue;
            };
            let path = entry.path();
            let auth_path = path.join("auth.json");
            let Ok(metadata) = fs::metadata(&auth_path).await else {
                continue;
            };
            if metadata.is_file() && metadata.len() > 0 && metadata.permissions().mode() & 0o777 == 0o600 {
                if let Some(kept_path) = numbered.get(&number) {
                    warn!(
                        slot_number = number,
                        kept_path = %kept_path.display(),
                        skipped_path = %path.display(),
                        "duplicate numeric Codex login slot; skipping directory"
                    );
                    continue;
                }
                numbered.insert(number, path);
            }
        }
        Ok(numbered
            .into_iter()
            .map(|(number, path)| (format!("slot-{number:020}"), MaterialPoolUnitSpec { directory: path.to_string_lossy().into_owned() }))
            .collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct SkillBundleManifest {
    schema_version: u32,
    required_skills: Vec<String>,
    sources: Vec<SkillSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct SkillSource {
    name: String,
    repository: String,
    revision: String,
}

#[derive(Debug, Clone)]
struct SkillBundle {
    source: Option<PathBuf>,
}

impl SkillBundle {
    fn new(source: Option<PathBuf>) -> Self {
        Self { source }
    }

    async fn prepare(&self) -> Result<AgentMaterialDelivery, String> {
        let source = self
            .source
            .clone()
            .ok_or_else(|| format!("contained agent requires generation-pinned skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))?;
        tokio::task::spawn_blocking({
            let source = source.clone();
            move || inspect_skill_sources(&source)
        })
        .await
        .map_err(|error| format!("inspect generation-pinned skill bundle task failed: {error}"))??;
        Ok(AgentMaterialDelivery {
            mount: ProvisionedMount::new(source, CONTAINER_SKILLS_SOURCE, ProvisionedMountMode::Ro),
            preflight: AgentMaterialPreflight {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "test -f \"$1/.flotilla-sources.json\"".to_string(),
                    "flotilla-skills-preflight".to_string(),
                    CONTAINER_SKILLS_SOURCE.to_string(),
                ],
                failure_context: "generation-pinned skill source preflight failed".to_string(),
            },
            // The App token is deliberately limited to the private fork. The
            // other pinned source must remain publicly readable.
            github_repository_grants: BTreeSet::from([PRIVATE_SKILL_REPOSITORY.to_string()]),
        })
    }

    async fn stage(
        &self,
        environment_ref: &str,
        adapters: &[&Arc<dyn AgentMaterialAdapter>],
        environment: &[(String, String)],
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        if adapters.is_empty() {
            return Ok(());
        }
        let config_base = runner
            .writable_config_base(None, Path::new(CONTAINED_WRITABLE_CONFIG_BASE))
            .await
            .map_err(|error| format!("resolve contained agent skill base: {error}"))?;
        let mut destinations = Vec::new();
        for adapter in adapters {
            if let Some(destination) = adapter.skill_destination(environment, &config_base)? {
                destinations.push((adapter.id(), destination));
            }
        }
        if destinations.is_empty() {
            return Ok(());
        }
        let git_config = environment
            .iter()
            .find(|(name, _)| name == "GIT_CONFIG_GLOBAL")
            .map(|(_, value)| value)
            .ok_or_else(|| "contained agent skill staging requires a prepared Git credential configuration".to_string())?;
        let github_token_file = environment.iter().find(|(name, _)| name == "GITHUB_TOKEN_FILE").map(|(_, value)| value);
        let github_token = environment.iter().find(|(name, _)| name == "GH_TOKEN").map(|(_, value)| value);
        let token_mode = if github_token_file.is_some() {
            "token-file"
        } else if github_token.is_some() {
            "stdin-token"
        } else {
            return Err("contained agent skill staging requires a prepared GitHub credential".to_string());
        };
        let source = self
            .source
            .clone()
            .ok_or_else(|| format!("contained agent requires generation-pinned skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))?;
        let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&source))
            .await
            .map_err(|error| format!("inspect generation-pinned skill sources task failed: {error}"))??;
        let mut args = vec![
            "-c".to_string(),
            "set -eu\ngit_config=$1\ntoken_mode=$2\ntoken_file=$3\nmanifest=$4\ndestination=$5\nrequired_count=$6\nshift 6\nexport GIT_CONFIG_GLOBAL=\"$git_config\" GIT_TERMINAL_PROMPT=0\nif [ \"$token_mode\" = token-file ]; then\n  export GITHUB_TOKEN_FILE=\"$token_file\"\nelse\n  GH_TOKEN=$(cat)\n  export GH_TOKEN\nfi\nstaged=\"${destination}.flotilla-staging\"\nsources=\"${destination}.flotilla-sources.$$\"\ntrap 'rm -rf \"$staged\" \"$sources\"' EXIT HUP INT TERM\nrm -rf \"$staged\" \"$sources\"\nmkdir -p \"$staged\" \"$sources\"\nrequired_file=\"$sources/required-skills\"\n: >\"$required_file\"\nwhile [ \"$required_count\" -gt 0 ]; do\n  printf '%s\\n' \"$1\" >>\"$required_file\"\n  shift\n  required_count=$((required_count - 1))\ndone\nwhile [ \"$#\" -gt 0 ]; do\n  name=$1\n  repository=$2\n  revision=$3\n  shift 3\n  checkout=\"$sources/$name\"\n  git -C \"$sources\" init \"$name\" >/dev/null\n  git -C \"$checkout\" remote add origin \"$repository\"\n  git -C \"$checkout\" fetch --depth=1 origin \"$revision\" >/dev/null\n  test \"$(git -C \"$checkout\" rev-parse FETCH_HEAD)\" = \"$revision\"\n  git -C \"$checkout\" checkout --detach FETCH_HEAD >/dev/null\n  find \"$checkout\" -type f -name SKILL.md >\"$sources/skill-files\"\n  while IFS= read -r skill_file; do\n    skill_dir=${skill_file%/SKILL.md}\n    skill_name=${skill_dir##*/}\n    target=\"$staged/$skill_name\"\n    if [ -e \"$target\" ]; then\n      echo \"duplicate skill name $skill_name from $repository\" >&2\n      exit 1\n    fi\n    mkdir -p \"$target\"\n    cp -R \"$skill_dir\"/. \"$target\"/\n  done <\"$sources/skill-files\"\ndone\nwhile IFS= read -r required; do\n  test -f \"$staged/$required/SKILL.md\" || { echo \"pinned skill sources are missing required skill $required\" >&2; exit 1; }\ndone <\"$required_file\"\ncp \"$manifest\" \"$staged/.flotilla-sources.json\"\nrm -rf \"$destination\"\nmv \"$staged\" \"$destination\"\nrm -rf \"$sources\"\ntrap - EXIT HUP INT TERM".to_string(),
            "flotilla-stage-skills".to_string(),
            git_config.clone(),
            token_mode.to_string(),
            github_token_file.cloned().unwrap_or_default(),
            format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}"),
            String::new(),
            inspection.required_skills.len().to_string(),
        ];
        args.extend(inspection.required_skills.iter().cloned());
        for source in &inspection.sources {
            args.extend([source.name.clone(), source.repository.clone(), source.revision.clone()]);
        }
        for (adapter, destination) in destinations {
            args[7] = destination.to_string_lossy().into_owned();
            let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
            let result = match github_token {
                Some(token) if github_token_file.is_none() => {
                    runner.run_with_input("sh", &arg_refs, Path::new("/"), &ChannelLabel::Default, token.as_bytes()).await
                }
                _ => runner.run("sh", &arg_refs, Path::new("/"), &ChannelLabel::Default).await,
            };
            result.map_err(|error| format!("stage generation-pinned skills for {environment_ref}: {error}"))?;
            info!(environment = environment_ref, adapter, sources = ?inspection.sources, "staged generation-pinned contained agent skills");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillBundleInspection {
    required_skills: Vec<String>,
    sources: Vec<SkillSource>,
}

fn inspect_skill_sources(source: &Path) -> Result<SkillBundleInspection, String> {
    let manifest_path = source.join(SKILL_BUNDLE_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read skill bundle manifest {}: {error}", manifest_path.display()))?;
    let manifest = serde_json::from_str::<SkillBundleManifest>(&manifest)
        .map_err(|error| format!("decode skill bundle manifest {}: {error}", manifest_path.display()))?;
    let expected = BTreeMap::from([
        (PRIVATE_SKILL_REPOSITORY, "https://github.com/flotilla-org/mattpocock-skills.git"),
        (PUBLIC_SKILL_REPOSITORY, "https://github.com/rjwittams/rjw-skills.git"),
    ]);
    let actual = manifest.sources.iter().map(|source| (source.name.as_str(), source.repository.as_str())).collect::<BTreeMap<_, _>>();
    if manifest.schema_version != 2 || actual != expected || manifest.sources.len() != expected.len() {
        return Err(format!("skill source manifest {} must name exactly the supported repositories", manifest_path.display()));
    }
    if manifest.sources.iter().any(|source| source.revision.len() != 40 || !source.revision.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return Err(format!("skill source manifest {} must pin every source to a full commit SHA", manifest_path.display()));
    }
    let required = manifest.required_skills.iter().collect::<BTreeSet<_>>();
    if manifest.required_skills.is_empty()
        || required.len() != manifest.required_skills.len()
        || manifest.required_skills.iter().any(|name| {
            name.is_empty()
                || name == "."
                || name == ".."
                || name.contains('/')
                || name.contains('\\')
                || name.chars().any(|character| matches!(character, '\r' | '\n'))
        })
    {
        return Err(format!("skill source manifest {} has invalid required skills", manifest_path.display()));
    }
    Ok(SkillBundleInspection { required_skills: manifest.required_skills, sources: manifest.sources })
}

#[async_trait]
impl AgentMaterialAdapter for CodexMaterialAdapter {
    fn id(&self) -> &'static str {
        CODEX_ADAPTER_ID
    }

    fn pool_ref(&self) -> Option<&'static str> {
        Some(CODEX_POOL_REF)
    }

    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment> {
        (!environment.contains_key("CODEX_HOME"))
            .then(|| agent_environment_fragment("CODEX_HOME", CONTAINER_CODEX_HOME, format!("agent-material/codex {CODEX_POOL_REF}")))
    }

    fn config_home_variable(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn is_managed_config_home(&self, config_home: &Path, _config_base: &Path) -> bool {
        config_home == Path::new(CONTAINER_CODEX_HOME)
    }

    fn externally_managed_home_opts_out_of_skills(&self) -> bool {
        true
    }

    async fn prepare(&self, holder_ref: &ResourceRef, environment: &BTreeMap<String, String>) -> Result<AgentMaterialOutcome, String> {
        if environment.contains_key("CODEX_HOME") {
            return Ok(AgentMaterialOutcome::NotRequired);
        }
        if !self.supported {
            return Err("Codex login material delivery is supported only on Linux placement hosts".to_string());
        }

        let spec = MaterialPoolSpec { units: self.usable_units().await? };
        self.pools.reconcile_pool(CODEX_POOL_REF, &spec).await?;
        match self.pools.acquire(CODEX_POOL_REF, holder_ref).await? {
            MaterialLeaseOutcome::Leased { unit, .. } => Ok(AgentMaterialOutcome::Ready(AgentMaterialDelivery {
                mount: ProvisionedMount::new(PathBuf::from(&unit.directory), CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw),
                preflight: AgentMaterialPreflight {
                    command: "codex".to_string(),
                    args: vec!["login".to_string(), "status".to_string()],
                    failure_context: "Codex login preflight failed".to_string(),
                },
                github_repository_grants: BTreeSet::new(),
            })),
            MaterialLeaseOutcome::Waiting { unit_count } => Ok(AgentMaterialOutcome::Waiting {
                pool_ref: CODEX_POOL_REF.to_string(),
                message: format!(
                    "waiting for codex login material; {unit_count} in pool, all leased; mint another unit to increase concurrency"
                ),
            }),
        }
    }
}

struct ClaudeCodeMaterialAdapter;

#[async_trait]
impl AgentMaterialAdapter for ClaudeCodeMaterialAdapter {
    fn id(&self) -> &'static str {
        CLAUDE_CODE_ADAPTER_ID
    }

    fn pool_ref(&self) -> Option<&'static str> {
        None
    }

    fn fragment(&self, _environment: &BTreeMap<String, String>) -> Option<Fragment> {
        None
    }

    fn config_home_variable(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn is_managed_config_home(&self, config_home: &Path, config_base: &Path) -> bool {
        config_home.starts_with(config_base)
    }

    fn externally_managed_home_opts_out_of_skills(&self) -> bool {
        false
    }

    async fn prepare(&self, _holder_ref: &ResourceRef, _environment: &BTreeMap<String, String>) -> Result<AgentMaterialOutcome, String> {
        Ok(AgentMaterialOutcome::NotRequired)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, path::Path, sync::Mutex};

    use flotilla_core::providers::discovery::test_support::TestEnvVars;
    use flotilla_protocol::NodeId;
    use flotilla_resources::InMemoryBackend;

    use super::*;

    #[derive(Clone)]
    struct LogCaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for LogCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log capture lock should be healthy").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingRunner(Mutex<Vec<(String, Vec<String>)>>);

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.0
                .lock()
                .expect("recording runner lock should be healthy")
                .push((cmd.to_string(), args.iter().map(|arg| (*arg).to_string()).collect()));
            Ok(String::new())
        }

        async fn run_output(
            &self,
            cmd: &str,
            args: &[&str],
            cwd: &Path,
            label: &ChannelLabel,
        ) -> Result<flotilla_core::providers::CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| flotilla_core::providers::CommandOutput {
                stdout,
                stderr: String::new(),
                success: true,
            })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    fn write_named_slot(root: &Path, name: &str, number: u64) -> PathBuf {
        let slot = root.join(name);
        std::fs::create_dir_all(&slot).expect("create slot");
        let auth = slot.join("auth.json");
        std::fs::write(&auth, format!("{{\"slot\":{number}}}")).expect("write auth");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).expect("protect auth");
        slot
    }

    fn write_slot(root: &Path, number: u64) -> PathBuf {
        write_named_slot(root, &format!("slot-{number}"), number)
    }

    fn write_skill_sources(root: &Path) -> PathBuf {
        let skills = root.join("generation/skills");
        std::fs::create_dir_all(&skills).expect("create skill bundle");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":2,"required_skills":["wayfinder","pr-shepherd"],"sources":[{"name":"mattpocock-skills","repository":"https://github.com/flotilla-org/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111"},{"name":"rjw-skills","repository":"https://github.com/rjwittams/rjw-skills.git","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#,
        )
        .expect("write skill bundle manifest");
        skills
    }

    fn registry(home: &Path) -> AgentMaterialRegistry {
        let skills = write_skill_sources(home);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        AgentMaterialRegistry::new(
            backend,
            "flotilla",
            Arc::new(TestEnvVars::new([
                ("HOME", home.to_string_lossy().into_owned()),
                (FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned()),
            ])),
        )
    }

    #[tokio::test]
    async fn codex_adapter_specializes_a_generic_lease_as_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let slot = write_slot(&temp.path().join(".config/flotilla/credentials/codex-pool"), 0);
        let registry = registry(temp.path());

        let deliveries =
            registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()).await.expect("prepare");

        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].mount, ProvisionedMount::new(&slot, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw));
        assert_eq!(
            deliveries[1].mount,
            ProvisionedMount::new(
                registry.skills.source.clone().expect("generation skill source"),
                CONTAINER_SKILLS_SOURCE,
                ProvisionedMountMode::Ro,
            )
        );
        let composed = crate::vessel_config::compose(
            crate::vessel_config::TargetId::AgentEnvironment,
            registry.fragments(&BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()),
        )
        .expect("compose Codex home");
        assert_eq!(composed.environment, vec![("CODEX_HOME".to_string(), CONTAINER_CODEX_HOME.to_string())]);
        assert!(composed.contents.contains("# fragment: agent-material/codex codex-login"));
    }

    #[tokio::test]
    async fn codex_login_material_is_leased_per_holder() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        let first = write_slot(&pool, 0);
        let second = write_slot(&pool, 1);
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        registry.prepare("crew-alice", &required, &BTreeMap::new()).await.expect("prepare Alice");
        registry.prepare("crew-bob", &required, &BTreeMap::new()).await.expect("prepare Bob");

        assert_ne!(first, second, "each holder must receive its own config home");
    }

    #[tokio::test]
    async fn pinned_skills_are_staged_to_the_seam_resolved_claude_config() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]);
        let environment = vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "/tmp/flotilla-config/credentials/claude-max/claude".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/tmp/flotilla-config/credentials/gitconfig".to_string()),
            ("GITHUB_TOKEN_FILE".to_string(), "/tmp/flotilla-config/credentials/github-app/token".to_string()),
        ];
        let runner = RecordingRunner::default();

        registry.stage_skills("crew-alice", &required, &environment, &runner).await.expect("stage pinned skills");

        let calls = runner.0.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sh");
        assert!(calls[0].1.contains(&format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}")));
        assert!(calls[0].1.contains(&"/tmp/flotilla-config/credentials/claude-max/claude/skills".to_string()));
        assert!(calls[0].1.contains(&"https://github.com/flotilla-org/mattpocock-skills.git".to_string()));
        assert!(calls[0].1.contains(&"1111111111111111111111111111111111111111".to_string()));
        assert!(calls[0].1.contains(&"wayfinder".to_string()));
        assert!(calls[0].1.contains(&"pr-shepherd".to_string()));
        assert!(!calls[0].1[1].contains("wayfinder"), "required skill policy must come from the manifest");
    }

    #[tokio::test]
    async fn pinned_skills_are_staged_to_the_seam_resolved_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);
        let environment = vec![
            ("CODEX_HOME".to_string(), CONTAINER_CODEX_HOME.to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/flotilla/config/credentials/gitconfig".to_string()),
            ("GITHUB_TOKEN_FILE".to_string(), "/run/flotilla/config/credentials/github-app/token".to_string()),
        ];
        let runner = RecordingRunner::default();

        registry.stage_skills("crew-codex", &required, &environment, &runner).await.expect("stage pinned skills");

        let calls = runner.0.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sh");
        assert!(calls[0].1.contains(&format!("{CONTAINER_CODEX_HOME}/skills")));
        assert!(calls[0].1.contains(&"https://github.com/flotilla-org/mattpocock-skills.git".to_string()));
        assert!(calls[0].1.contains(&"1111111111111111111111111111111111111111".to_string()));
    }

    #[tokio::test]
    async fn externally_managed_codex_home_skips_skill_staging() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);
        let environment = vec![
            ("CODEX_HOME".to_string(), "/image/codex".to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/flotilla/config/credentials/gitconfig".to_string()),
            ("GITHUB_TOKEN_FILE".to_string(), "/run/flotilla/config/credentials/github-app/token".to_string()),
        ];
        let runner = RecordingRunner::default();

        registry.stage_skills("crew-codex", &required, &environment, &runner).await.expect("skip external Codex home");

        assert!(runner.0.lock().expect("recording runner lock should be healthy").is_empty());
    }

    #[test]
    fn staged_skill_source_revisions_are_reported() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogCaptureWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let inspection = inspect_skill_sources(&skills).expect("inspect skill sources");
        info!(
            environment = "crew-alice",
            adapter = CLAUDE_CODE_ADAPTER_ID,
            sources = ?inspection.sources,
            "staged generation-pinned contained agent skills"
        );

        let logs = String::from_utf8(output.lock().expect("log capture lock should be healthy").clone()).expect("UTF-8 logs");
        assert!(logs.contains("staged generation-pinned contained agent skills"), "missing provisioning event: {logs}");
        assert!(logs.contains("crew-alice"), "provisioning event must identify its holder: {logs}");
        assert!(logs.contains("1111111111111111111111111111111111111111"), "provisioning event must report source pins: {logs}");
    }

    #[test]
    fn skill_manifest_rejects_invalid_required_skills() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        std::fs::write(&manifest_path, manifest.replace(r#"["wayfinder","pr-shepherd"]"#, r#"["wayfinder","wayfinder"]"#))
            .expect("write invalid fixture manifest");

        let error = inspect_skill_sources(&skills).expect_err("duplicate required skill must fail validation");

        assert!(error.contains("invalid required skills"), "unexpected validation error: {error}");
    }

    #[tokio::test]
    async fn duplicate_numeric_codex_slots_keep_one_unit_and_warn_with_both_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("codex-pool");
        let slot_zero = write_named_slot(&pool_dir, "slot-0", 0);
        let slot_zero_padded = write_named_slot(&pool_dir, "slot-00", 0);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let adapter = CodexMaterialAdapter::new(Arc::new(MaterialPoolManager::new(backend, "flotilla")), pool_dir, true);
        let log_output = Arc::new(Mutex::new(Vec::new()));

        let units = {
            let writer = LogCaptureWriter(Arc::clone(&log_output));
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || writer.clone())
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            adapter.usable_units().await.expect("discover usable units")
        };

        assert_eq!(units.len(), 1);
        assert_eq!(units.keys().map(String::as_str).collect::<Vec<_>>(), ["slot-00000000000000000000"]);
        let logs = String::from_utf8(log_output.lock().expect("log capture lock should be healthy").clone()).expect("logs should be utf-8");
        assert!(logs.contains("duplicate numeric Codex login slot; skipping directory"), "missing duplicate warning: {logs}");
        assert!(logs.contains(&slot_zero.to_string_lossy().into_owned()), "warning should name slot-0: {logs}");
        assert!(logs.contains(&slot_zero_padded.to_string_lossy().into_owned()), "warning should name slot-00: {logs}");
    }

    #[tokio::test]
    async fn codex_adapter_waits_on_exhaustion_and_reconciles_new_units() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        write_slot(&pool, 0);
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("first lease");
        assert!(matches!(
            registry.prepare("env-b", &required, &BTreeMap::new()).await,
            Err(AgentMaterialPrepareError::Waiting { pool_ref, .. }) if pool_ref == CODEX_POOL_REF
        ));

        let second = write_slot(&pool, 1);
        let delivery = registry.prepare("env-b", &required, &BTreeMap::new()).await.expect("lease new unit");
        assert_eq!(delivery[0].mount, ProvisionedMount::new(second, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw));
    }

    #[tokio::test]
    async fn missing_generation_skill_sources_fail_before_agent_container_creation() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_slot(&temp.path().join(".config/flotilla/credentials/codex-pool"), 0);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let registry = AgentMaterialRegistry::new(
            backend,
            "flotilla",
            Arc::new(TestEnvVars::new([("HOME", temp.path().to_string_lossy().into_owned())])),
        );

        let error = registry
            .prepare("env-a", &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("contained Claude must require pinned skill sources");

        assert!(matches!(error, AgentMaterialPrepareError::Failed(message) if message.contains(FLOTILLA_SKILLS_DIR_ENV)));

        let codex_error = registry
            .prepare("env-b", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("contained Codex must require pinned skill sources");
        assert!(matches!(codex_error, AgentMaterialPrepareError::Failed(message) if message.contains(FLOTILLA_SKILLS_DIR_ENV)));
    }

    #[tokio::test]
    async fn codex_specific_validation_stays_in_the_adapter() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        let slot = write_slot(&pool, 0);
        std::fs::set_permissions(slot.join("auth.json"), std::fs::Permissions::from_mode(0o644)).expect("weaken auth");
        let registry = registry(temp.path());

        assert!(matches!(
            registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new()).await,
            Err(AgentMaterialPrepareError::Waiting { .. })
        ));
    }

    #[tokio::test]
    async fn codex_adapter_defers_to_an_existing_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        assert!(registry
            .prepare("env-home", &required, &BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]))
            .await
            .expect("existing home")
            .is_empty());
    }
}
