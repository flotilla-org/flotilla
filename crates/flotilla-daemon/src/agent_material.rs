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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillSourceCredentialRequest {
    pub(crate) source: String,
    pub(crate) repository: String,
    pub(crate) credential: String,
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
    homes_dir: PathBuf,
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
        let homes_dir = env
            .get("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"))
            .join(".local/share/flotilla/agent-homes");
        let skills = SkillBundle::new(env.get(FLOTILLA_SKILLS_DIR_ENV).map(PathBuf::from));
        let codex: Arc<dyn AgentMaterialAdapter> =
            Arc::new(CodexMaterialAdapter::new(Arc::clone(&pools), pool_dir, homes_dir.clone(), cfg!(any(target_os = "linux", test))));
        let claude_code: Arc<dyn AgentMaterialAdapter> = Arc::new(ClaudeCodeMaterialAdapter);
        Self {
            namespace: namespace.to_string(),
            pools,
            homes_dir,
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
        source_token_files: &BTreeMap<String, PathBuf>,
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        let adapters = required_adapters.iter().filter_map(|adapter_id| self.adapters.get(adapter_id.as_str())).collect::<Vec<_>>();
        self.skills.stage(environment_ref, &adapters, environment, source_token_files, runner).await
    }

    pub(crate) async fn skill_source_credentials(&self) -> Result<Vec<SkillSourceCredentialRequest>, String> {
        self.skills.credential_requests().await
    }

    pub(crate) async fn will_stage_skills(
        &self,
        required_adapters: &BTreeSet<String>,
        environment: &[(String, String)],
        runner: &dyn CommandRunner,
    ) -> Result<bool, String> {
        let adapters = required_adapters.iter().filter_map(|adapter_id| self.adapters.get(adapter_id.as_str())).collect::<Vec<_>>();
        if adapters.is_empty() {
            return Ok(false);
        }
        let config_base = runner
            .writable_config_base(None, Path::new(CONTAINED_WRITABLE_CONFIG_BASE))
            .await
            .map_err(|error| format!("resolve contained agent skill base: {error}"))?;
        for adapter in adapters {
            if adapter.skill_destination(environment, &config_base)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn fragments(&self, required_adapters: &BTreeSet<String>, environment: &BTreeMap<String, String>) -> Vec<Fragment> {
        required_adapters
            .iter()
            .filter_map(|adapter_id| self.adapters.get(adapter_id.as_str()))
            .filter_map(|adapter| adapter.fragment(environment))
            .collect()
    }

    pub(crate) async fn release(&self, environment_ref: &str) -> Result<(), String> {
        let auth = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID).join("auth.json");
        match fs::remove_file(&auth).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove released Codex login {}: {error}", auth.display())),
        }
        self.pools.release_holder(&self.holder_ref(environment_ref)).await
    }

    pub(crate) async fn remove_environment_home(&self, environment_ref: &str) -> Result<(), String> {
        let home = self.homes_dir.join(environment_ref);
        match fs::remove_dir_all(&home).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove persistent agent home {}: {error}", home.display())),
        }
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
    homes_dir: PathBuf,
    supported: bool,
}

impl CodexMaterialAdapter {
    fn new(pools: Arc<MaterialPoolManager>, pool_dir: PathBuf, homes_dir: PathBuf, supported: bool) -> Self {
        Self { pools, pool_dir, homes_dir, supported }
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
    sources: Vec<SkillSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct SkillSource {
    name: String,
    repository: String,
    revision: String,
    credential: Option<String>,
    #[serde(default = "default_skill_source_paths")]
    paths: Vec<String>,
}

fn default_skill_source_paths() -> Vec<String> {
    vec!["skills".to_string()]
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
        })
    }

    async fn credential_requests(&self) -> Result<Vec<SkillSourceCredentialRequest>, String> {
        let source = self
            .source
            .clone()
            .ok_or_else(|| format!("contained agent requires generation-pinned skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))?;
        let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&source))
            .await
            .map_err(|error| format!("inspect generation-pinned skill sources task failed: {error}"))??;
        Ok(inspection
            .sources
            .into_iter()
            .filter_map(|source| {
                source.credential.map(|credential| SkillSourceCredentialRequest {
                    source: source.name,
                    repository: source.repository,
                    credential,
                })
            })
            .collect())
    }

    async fn stage(
        &self,
        environment_ref: &str,
        adapters: &[&Arc<dyn AgentMaterialAdapter>],
        environment: &[(String, String)],
        source_token_files: &BTreeMap<String, PathBuf>,
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        if adapters.is_empty() {
            return remove_source_token_files(source_token_files, runner).await;
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
            return remove_source_token_files(source_token_files, runner).await;
        }
        let source = self
            .source
            .clone()
            .ok_or_else(|| format!("contained agent requires generation-pinned skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))?;
        let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&source))
            .await
            .map_err(|error| format!("inspect generation-pinned skill sources task failed: {error}"))??;
        let mut args = vec![
            "-c".to_string(),
            "set -eu\nmanifest=$1\ndestination=$2\ncleanup_tokens=$3\nshift 3\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0\nstaged=\"${destination}.flotilla-staging\"\nsources=\"${destination}.flotilla-sources.$$\"\ntoken_files=\nsucceeded=false\ncleanup() { rm -rf \"$staged\" \"$sources\"; if [ \"$cleanup_tokens\" = true ] || [ \"$succeeded\" != true ]; then for token_file in $token_files; do rm -f \"$token_file\"; done; fi; }\ntrap cleanup EXIT HUP INT TERM\nrm -rf \"$staged\" \"$sources\"\nmkdir -p \"$staged\" \"$sources\"\nwhile [ \"$#\" -gt 0 ]; do\n  name=$1\n  repository=$2\n  revision=$3\n  token_file=$4\n  credential=$5\n  path_count=$6\n  shift 6\n  checkout=\"$sources/$name\"\n  paths_file=\"$sources/$name.paths\"\n  sparse_file=\"$sources/$name.sparse\"\n  : >\"$paths_file\"\n  : >\"$sparse_file\"\n  while [ \"$path_count\" -gt 0 ]; do\n    printf '%s\\n' \"$1\" >>\"$paths_file\"\n    printf '/%s/\\n' \"$1\" >>\"$sparse_file\"\n    shift\n    path_count=$((path_count - 1))\n  done\n  git -C \"$sources\" init --quiet \"$name\" >/dev/null\n  git -C \"$checkout\" remote add origin \"$repository\"\n  git -C \"$checkout\" sparse-checkout set --no-cone --stdin <\"$sparse_file\" >/dev/null\n  if [ -n \"$token_file\" ]; then\n    token_files=\"$token_files $token_file\"\n    export GITHUB_TOKEN_FILE=\"$token_file\"\n    helper='!f() { [ \"$1\" = get ] || exit 0; printf \"username=x-access-token\\npassword=\"; cat \"$GITHUB_TOKEN_FILE\"; printf \"\\n\"; }; f'\n    git -C \"$checkout\" -c credential.helper= -c credential.helper=\"$helper\" fetch --quiet --depth=1 --filter=blob:none --no-tags origin \"$revision\" >/dev/null || { echo \"skill source $name credential $credential fetch failed\" >&2; exit 1; }\n    unset GITHUB_TOKEN_FILE\n  else\n    git -C \"$checkout\" -c credential.helper= fetch --quiet --depth=1 --filter=blob:none --no-tags origin \"$revision\" >/dev/null || { echo \"skill source $name anonymous fetch failed\" >&2; exit 1; }\n  fi\n  test \"$(git -C \"$checkout\" rev-parse FETCH_HEAD)\" = \"$revision\"\n  git -C \"$checkout\" checkout --quiet --detach FETCH_HEAD >/dev/null\n  while IFS= read -r path; do\n    if [ ! -d \"$checkout/$path\" ]; then\n      echo \"skill source $name declared path $path is missing at pinned revision $revision\" >&2\n      exit 1\n    fi\n    find \"$checkout/$path\" -type f -name SKILL.md >\"$sources/skill-files\"\n    if [ ! -s \"$sources/skill-files\" ]; then\n      echo \"skill source $name declared path $path has no SKILL.md at pinned revision $revision\" >&2\n      exit 1\n    fi\n    while IFS= read -r skill_file; do\n      skill_dir=${skill_file%/SKILL.md}\n      skill_name=${skill_dir##*/}\n      target=\"$staged/$skill_name\"\n      if [ -e \"$target\" ]; then\n        echo \"duplicate skill name $skill_name from $repository\" >&2\n        exit 1\n      fi\n      mkdir -p \"$target\"\n      cp -R \"$skill_dir\"/. \"$target\"/\n    done <\"$sources/skill-files\"\n  done <\"$paths_file\"\ndone\ncp \"$manifest\" \"$staged/.flotilla-sources.json\"\nrm -rf \"$destination\"\nmv \"$staged\" \"$destination\"\nrm -rf \"$sources\"\nsucceeded=true\ncleanup\ntrap - EXIT HUP INT TERM".to_string(),
            "flotilla-stage-skills".to_string(),
            format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}"),
            String::new(),
            String::new(),
        ];
        for source in &inspection.sources {
            let token_file = source_token_files.get(&source.name).map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
            args.extend([
                source.name.clone(),
                source.repository.clone(),
                source.revision.clone(),
                token_file,
                source.credential.clone().unwrap_or_default(),
                source.paths.len().to_string(),
            ]);
            args.extend(source.paths.clone());
        }
        let destination_count = destinations.len();
        for (index, (adapter, destination)) in destinations.into_iter().enumerate() {
            args[4] = destination.to_string_lossy().into_owned();
            args[5] = (index + 1 == destination_count).to_string();
            let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
            let result = runner.run("sh", &arg_refs, Path::new("/"), &ChannelLabel::Default).await;
            result.map_err(|error| format!("stage generation-pinned skills for {environment_ref}: {error}"))?;
            info!(environment = environment_ref, adapter, sources = ?inspection.sources, "staged generation-pinned contained agent skills");
        }
        Ok(())
    }
}

async fn remove_source_token_files(source_token_files: &BTreeMap<String, PathBuf>, runner: &dyn CommandRunner) -> Result<(), String> {
    if source_token_files.is_empty() {
        return Ok(());
    }
    let paths = source_token_files.values().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>();
    let mut args = vec!["-f", "--"];
    args.extend(paths.iter().map(String::as_str));
    runner
        .run("rm", &args, Path::new("/"), &ChannelLabel::Default)
        .await
        .map(|_| ())
        .map_err(|error| format!("discard unused skill-source credential files: {error}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillBundleInspection {
    sources: Vec<SkillSource>,
}

/// Validates the supply side of skill staging only: the manifest must pin an
/// arbitrary, well-formed set of sources and may attach a named credential to
/// each source.
/// There is deliberately no required-skill assertion here — what a given crew
/// must have is a per-project/role demand declaration (#1790), validated per
/// crew when that model lands, never a universal list.
fn inspect_skill_sources(source: &Path) -> Result<SkillBundleInspection, String> {
    let manifest_path = source.join(SKILL_BUNDLE_MANIFEST);
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|error| format!("read skill bundle manifest {}: {error}", manifest_path.display()))?;
    let manifest = serde_json::from_str::<SkillBundleManifest>(&manifest)
        .map_err(|error| format!("decode skill bundle manifest {}: {error}", manifest_path.display()))?;
    if manifest.schema_version != 5 || manifest.sources.is_empty() {
        return Err(format!("skill source manifest {} must use schema version 5 and pin at least one source", manifest_path.display()));
    }
    let names = manifest.sources.iter().map(|source| source.name.as_str()).collect::<BTreeSet<_>>();
    if names.len() != manifest.sources.len()
        || manifest.sources.iter().any(|source| {
            source.name.is_empty()
                || source.name == "."
                || source.name == ".."
                || source.name.contains('/')
                || source.name.contains('\\')
                || source.name.chars().any(|character| matches!(character, '\r' | '\n'))
                || source.repository.is_empty()
                || source.credential.as_ref().is_some_and(|credential| {
                    credential.is_empty()
                        || credential.contains('/')
                        || credential.contains('\\')
                        || credential.chars().any(|character| matches!(character, '\r' | '\n'))
                })
                || source.paths.is_empty()
                || source.paths.iter().collect::<BTreeSet<_>>().len() != source.paths.len()
                || source.paths.iter().any(|path| {
                    path.is_empty()
                        || path.starts_with('/')
                        || path.ends_with('/')
                        || path.contains('\\')
                        || path.chars().any(|character| matches!(character, '\r' | '\n' | '*' | '?' | '[' | ']'))
                        || path.split('/').any(|component| component.is_empty() || component == "." || component == "..")
                })
        })
    {
        return Err(format!("skill source manifest {} has invalid or duplicate source entries", manifest_path.display()));
    }
    if manifest.sources.iter().any(|source| source.revision.len() != 40 || !source.revision.bytes().all(|byte| byte.is_ascii_hexdigit())) {
        return Err(format!("skill source manifest {} must pin every source to a full commit SHA", manifest_path.display()));
    }
    Ok(SkillBundleInspection { sources: manifest.sources })
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
            MaterialLeaseOutcome::Leased { unit, .. } => {
                let home = self.homes_dir.join(&holder_ref.name).join(CODEX_ADAPTER_ID);
                fs::create_dir_all(&home).await.map_err(|error| format!("create persistent Codex home {}: {error}", home.display()))?;
                let source = PathBuf::from(&unit.directory).join("auth.json");
                let destination = home.join("auth.json");
                fs::copy(&source, &destination)
                    .await
                    .map_err(|error| format!("install leased Codex login {} into {}: {error}", source.display(), destination.display()))?;
                fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
                    .await
                    .map_err(|error| format!("protect leased Codex login {}: {error}", destination.display()))?;
                Ok(AgentMaterialOutcome::Ready(AgentMaterialDelivery {
                    mount: ProvisionedMount::new(home, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw),
                    preflight: AgentMaterialPreflight {
                        command: "codex".to_string(),
                        args: vec!["login".to_string(), "status".to_string()],
                        failure_context: "Codex login preflight failed".to_string(),
                    },
                }))
            }
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
            r#"{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"https://github.com/flotilla-org/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111","credential":"github-skills-fork"},{"name":"rjw-skills","repository":"https://github.com/rjwittams/rjw-skills.git","revision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","paths":["plugins/rjw-sdlc/skills"]}]}"#,
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
        assert_eq!(
            deliveries[0].mount,
            ProvisionedMount::new(
                temp.path().join(".local/share/flotilla/agent-homes/env-a/codex"),
                CONTAINER_CODEX_HOME,
                ProvisionedMountMode::Rw,
            )
        );
        assert_eq!(
            std::fs::read(deliveries[0].mount.host_path.join("auth.json")).expect("copied auth"),
            std::fs::read(slot.join("auth.json")).expect("slot auth")
        );
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

        registry
            .stage_skills(
                "crew-alice",
                &required,
                &environment,
                &BTreeMap::from([("mattpocock-skills".to_string(), PathBuf::from("/tmp/skills-token"))]),
                &runner,
            )
            .await
            .expect("stage pinned skills");

        let calls = runner.0.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sh");
        assert!(calls[0].1.contains(&format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}")));
        assert!(calls[0].1.contains(&"/tmp/flotilla-config/credentials/claude-max/claude/skills".to_string()));
        assert!(calls[0].1.contains(&"https://github.com/flotilla-org/mattpocock-skills.git".to_string()));
        assert!(calls[0].1.contains(&"1111111111111111111111111111111111111111".to_string()));
        assert!(calls[0].1.contains(&"plugins/rjw-sdlc/skills".to_string()));
        assert!(calls[0].1.contains(&"/tmp/skills-token".to_string()));
        assert!(calls[0].1[1].contains("fetch --quiet --depth=1 --filter=blob:none --no-tags"));
        assert!(calls[0].1[1].contains("credential.helper="), "public fetches must clear the project credential helper");
        assert!(calls[0].1[1].contains("anonymous fetch failed"));
        assert!(calls[0].1[1].contains("init --quiet"));
        assert!(!calls[0].1[1].contains("2>&1"), "git failures must retain their stderr diagnostics");
        assert!(calls[0].1[1].contains("sparse-checkout set --no-cone --stdin"));
        assert!(calls[0].1[1].contains("skill source $name declared path $path is missing at pinned revision $revision"));
        assert!(calls[0].1[1].contains("skill source $name declared path $path has no SKILL.md at pinned revision $revision"));
        assert!(!calls[0].1[1].contains("required"), "staging must carry no skill-name policy; demand validation is #1790's contract");
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

        registry.stage_skills("crew-codex", &required, &environment, &BTreeMap::new(), &runner).await.expect("stage pinned skills");

        let calls = runner.0.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sh");
        assert!(calls[0].1.contains(&format!("{CONTAINER_CODEX_HOME}/skills")));
        assert!(calls[0].1.contains(&"https://github.com/flotilla-org/mattpocock-skills.git".to_string()));
        assert!(calls[0].1.contains(&"1111111111111111111111111111111111111111".to_string()));
    }

    #[tokio::test]
    async fn pinned_skills_are_staged_to_every_required_adapter_destination() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string(), CODEX_ADAPTER_ID.to_string()]);
        let claude_skills = "/tmp/flotilla-config/credentials/claude-max/claude/skills";
        let codex_skills = format!("{CONTAINER_CODEX_HOME}/skills");
        let environment = vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "/tmp/flotilla-config/credentials/claude-max/claude".to_string()),
            ("CODEX_HOME".to_string(), CONTAINER_CODEX_HOME.to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/run/flotilla/config/credentials/gitconfig".to_string()),
            ("GITHUB_TOKEN_FILE".to_string(), "/run/flotilla/config/credentials/github-app/token".to_string()),
        ];
        let runner = RecordingRunner::default();

        registry
            .stage_skills("crew-mixed", &required, &environment, &BTreeMap::new(), &runner)
            .await
            .expect("stage pinned skills for both adapters");

        let calls = runner.0.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 2);
        let destinations =
            calls.iter().map(|(_, args)| args.get(4).expect("staging call must include its destination").as_str()).collect::<BTreeSet<_>>();
        assert_eq!(destinations, BTreeSet::from([claude_skills, codex_skills.as_str()]));
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

        assert!(!registry.will_stage_skills(&required, &environment, &runner).await.expect("resolve external Codex skill staging"));

        registry
            .stage_skills(
                "crew-codex",
                &required,
                &environment,
                &BTreeMap::from([("mattpocock-skills".to_string(), PathBuf::from("/tmp/unused-skill-token"))]),
                &runner,
            )
            .await
            .expect("skip external Codex home");

        assert_eq!(runner.0.lock().expect("recording runner lock should be healthy").as_slice(), &[("rm".to_string(), vec![
            "-f".to_string(),
            "--".to_string(),
            "/tmp/unused-skill-token".to_string()
        ])]);
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
    fn skill_manifest_rejects_duplicate_or_traversing_source_names() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        std::fs::write(&manifest_path, manifest.replace(r#""name":"rjw-skills""#, r#""name":"mattpocock-skills""#))
            .expect("write duplicate-name fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("duplicate source name must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, manifest.replace(r#""name":"rjw-skills""#, r#""name":"../rjw-skills""#))
            .expect("write traversing-name fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("path-traversing source name must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, manifest.replace("github-skills-fork", "../credential"))
            .expect("write traversing credential fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("path-traversing credential must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");
    }

    #[test]
    fn skill_manifest_validates_declared_paths_and_applies_the_default() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        let inspection = inspect_skill_sources(&skills).expect("inspect default and declared paths");
        assert_eq!(inspection.sources[0].paths, ["skills"]);
        assert_eq!(inspection.sources[1].paths, ["plugins/rjw-sdlc/skills"]);

        for invalid in [
            "",
            "../skills",
            "plugins/../skills",
            "/skills",
            "skills/",
            r"plugins\skills",
            "skills\nother",
            "skills/*",
            "skills/[ab]",
            "skills?",
        ] {
            let encoded = serde_json::to_string(invalid).expect("encode invalid fixture path");
            std::fs::write(&manifest_path, manifest.replace(r#"["plugins/rjw-sdlc/skills"]"#, &format!("[{encoded}]")))
                .expect("write invalid-path fixture manifest");
            let error = inspect_skill_sources(&skills).expect_err("invalid source path must fail validation");
            assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error for {invalid:?}: {error}");
        }

        std::fs::write(&manifest_path, manifest.replace(r#"["plugins/rjw-sdlc/skills"]"#, r#"["skills","skills"]"#))
            .expect("write duplicate-path fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("duplicate source paths must fail validation");
        assert!(error.contains("invalid or duplicate source entries"), "unexpected validation error: {error}");
    }

    #[test]
    fn skill_manifest_accepts_any_source_count() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        std::fs::write(
            &manifest_path,
            r#"{"schema_version":5,"sources":[{"name":"only","repository":"https://example.com/only.git","revision":"1111111111111111111111111111111111111111"}]}"#,
        )
        .expect("write single-source manifest");
        let inspection = inspect_skill_sources(&skills).expect("single source must validate");
        assert_eq!(inspection.sources.len(), 1);

        std::fs::write(
            &manifest_path,
            r#"{"schema_version":5,"sources":[{"name":"one","repository":"https://example.com/one.git","revision":"1111111111111111111111111111111111111111"},{"name":"two","repository":"https://example.com/two.git","revision":"2222222222222222222222222222222222222222"},{"name":"three","repository":"https://example.com/three.git","revision":"3333333333333333333333333333333333333333"}]}"#,
        )
        .expect("write three-source manifest");
        let inspection = inspect_skill_sources(&skills).expect("three sources must validate");
        assert_eq!(inspection.sources.len(), 3);
    }

    #[test]
    fn skill_manifest_allows_credentials_on_arbitrary_sources() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");

        std::fs::write(
            &manifest_path,
            manifest.replace("https://github.com/flotilla-org/mattpocock-skills.git", "https://github.com/another-owner/private.git"),
        )
        .expect("write relocated credentialed source fixture manifest");
        let inspection = inspect_skill_sources(&skills).expect("credentialed source repository remains manifest data");
        assert_eq!(inspection.sources.len(), 2);
    }

    #[test]
    fn skill_manifest_rejects_unpinned_revisions() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");

        for unpinned in ["main", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaz", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
            std::fs::write(&manifest_path, manifest.replace("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", unpinned))
                .expect("write unpinned fixture manifest");
            let error = inspect_skill_sources(&skills).expect_err("a revision that is not a full SHA must fail validation");
            assert!(error.contains("full commit SHA"), "unexpected validation error for {unpinned}: {error}");
        }
    }

    #[test]
    fn skill_manifest_rejects_a_superseded_schema_version() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let manifest_path = skills.join(SKILL_BUNDLE_MANIFEST);
        let manifest = std::fs::read_to_string(&manifest_path).expect("read fixture manifest");
        std::fs::write(&manifest_path, manifest.replace(r#""schema_version":5"#, r#""schema_version":4"#))
            .expect("write superseded-schema fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("schema version 4 must fail validation");
        assert!(error.contains("schema version 5"), "unexpected validation error: {error}");

        std::fs::write(&manifest_path, r#"{"schema_version":5,"sources":[]}"#).expect("write empty-source fixture manifest");
        let error = inspect_skill_sources(&skills).expect_err("empty source set must fail validation");
        assert!(error.contains("at least one source"), "unexpected validation error: {error}");
    }

    #[tokio::test]
    async fn duplicate_numeric_codex_slots_keep_one_unit_and_warn_with_both_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool_dir = temp.path().join("codex-pool");
        let slot_zero = write_named_slot(&pool_dir, "slot-0", 0);
        let slot_zero_padded = write_named_slot(&pool_dir, "slot-00", 0);
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new("root-a"));
        let adapter =
            CodexMaterialAdapter::new(Arc::new(MaterialPoolManager::new(backend, "flotilla")), pool_dir, temp.path().join("homes"), true);
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

        write_slot(&pool, 1);
        let delivery = registry.prepare("env-b", &required, &BTreeMap::new()).await.expect("lease new unit");
        assert_eq!(delivery[0].mount.host_path.as_path(), temp.path().join(".local/share/flotilla/agent-homes/env-b/codex"));
    }

    #[tokio::test]
    async fn codex_release_preserves_session_home_and_reacquire_installs_login_again() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pool = temp.path().join(".config/flotilla/credentials/codex-pool");
        write_slot(&pool, 0);
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        let delivery = registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("initial lease");
        let codex_home = delivery[0].mount.host_path.as_path();
        std::fs::create_dir_all(codex_home.join("sessions/2026/09/13")).expect("real rollout tree");
        std::fs::write(codex_home.join("sessions/2026/09/13/rollout.jsonl"), "{\"type\":\"session_meta\"}\n").expect("rollout");
        std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5\"\n").expect("config");

        registry.release("env-a").await.expect("release login");
        assert!(!codex_home.join("auth.json").exists(), "released home must not retain pooled login material");
        let resumed = registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("reacquire login");

        assert_eq!(resumed[0].mount.host_path.as_path(), codex_home);
        assert!(codex_home.join("auth.json").exists());
        assert_eq!(std::fs::read_to_string(codex_home.join("config.toml")).expect("preserved config"), "model = \"gpt-5\"\n");
        assert_eq!(
            std::fs::read_to_string(codex_home.join("sessions/2026/09/13/rollout.jsonl")).expect("preserved rollout"),
            "{\"type\":\"session_meta\"}\n"
        );
    }

    #[tokio::test]
    async fn environment_home_removal_deletes_persistent_agent_state_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let environment_home = temp.path().join(".local/share/flotilla/agent-homes/env-a");
        let session = environment_home.join("codex/sessions/2026/09/16/rollout.jsonl");
        std::fs::create_dir_all(session.parent().expect("session has parent")).expect("session directory");
        std::fs::write(&session, "{\"type\":\"session_meta\"}\n").expect("session state");

        registry.remove_environment_home("env-a").await.expect("remove environment home");
        assert!(!environment_home.exists(), "environment deletion must remove persistent agent state");

        registry.remove_environment_home("env-a").await.expect("repeat environment home removal");
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
