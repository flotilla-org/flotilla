use std::{
    collections::{BTreeMap, BTreeSet},
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
use tokio::{fs, io::AsyncWriteExt};
use tracing::info;

use crate::{
    codex_central::codex_central_auth_path,
    vessel_config::{agent_environment_fragment, Fragment},
};

const CODEX_ADAPTER_ID: &str = "codex";
const CLAUDE_CODE_ADAPTER_ID: &str = "claude-code";
pub(crate) const FLOTILLA_SKILLS_DIR_ENV: &str = "FLOTILLA_SKILLS_DIR";
/// Generation-provided, credential-free `CODEX_HOME` template that seeds each
/// crew's writable scratch (`config.toml`, `skills/`, static defaults). The
/// generation-side artifact and its `fleet-install` wiring are
/// flotilla-org/flotilla#1913; this is the consumer contract it satisfies.
/// Absent, a crew simply starts from an empty scratch directory.
pub(crate) const FLOTILLA_CODEX_HOME_TEMPLATE_ENV: &str = "FLOTILLA_CODEX_HOME_TEMPLATE";
const SKILL_BUNDLE_MANIFEST: &str = ".flotilla-sources.json";
const CONTAINER_SKILLS_SOURCE: &str = "/run/flotilla/skills";
pub(crate) const CONTAINER_CODEX_HOME: &str = CONTAINED_CODEX_HOME;
/// The one credential file inside a crew's `CODEX_HOME`. Everything else under
/// that directory is per-crew scratch Codex may write freely.
const CODEX_AUTH_FILE: &str = "auth.json";
/// Codex must never rewrite the delivered credential: a crew's copy is a
/// read-only snapshot of the central login, kept fresh by the host refresher.
const CODEX_AUTH_MODE: u32 = 0o400;

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

#[async_trait]
trait AgentMaterialAdapter: Send + Sync {
    fn id(&self) -> &'static str;
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

    async fn prepare(&self, environment_ref: &str, environment: &BTreeMap<String, String>)
        -> Result<Option<AgentMaterialDelivery>, String>;
}

pub(crate) struct AgentMaterialRegistry {
    homes_dir: PathBuf,
    codex_central_auth_path: PathBuf,
    adapters: BTreeMap<&'static str, Arc<dyn AgentMaterialAdapter>>,
    skills: SkillBundle,
}

impl AgentMaterialRegistry {
    pub(crate) fn new(env: Arc<dyn EnvVars>) -> Self {
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"));
        let homes_dir = home.join(".local/share/flotilla/agent-homes");
        let codex_central_auth_path = codex_central_auth_path(&*env);
        let skills = SkillBundle::new(env.get(FLOTILLA_SKILLS_DIR_ENV).map(PathBuf::from));
        let codex: Arc<dyn AgentMaterialAdapter> = Arc::new(CodexMaterialAdapter::new(
            codex_central_auth_path.clone(),
            env.get(FLOTILLA_CODEX_HOME_TEMPLATE_ENV).map(PathBuf::from),
            homes_dir.clone(),
            cfg!(any(target_os = "linux", test)),
        ));
        let claude_code: Arc<dyn AgentMaterialAdapter> = Arc::new(ClaudeCodeMaterialAdapter);
        Self {
            homes_dir,
            codex_central_auth_path,
            adapters: BTreeMap::from([(codex.id(), codex), (claude_code.id(), claude_code)]),
            skills,
        }
    }

    pub(crate) async fn prepare(
        &self,
        environment_ref: &str,
        required_adapters: &BTreeSet<String>,
        environment: &BTreeMap<String, String>,
    ) -> Result<Vec<AgentMaterialDelivery>, String> {
        let mut deliveries = Vec::new();
        for adapter_id in required_adapters {
            let Some(adapter) = self.adapters.get(adapter_id.as_str()) else {
                continue;
            };
            if let Some(delivery) = adapter.prepare(environment_ref, environment).await? {
                deliveries.push(delivery);
            }
        }
        if required_adapters
            .iter()
            .filter_map(|adapter_id| self.adapters.get(adapter_id.as_str()))
            .any(|adapter| adapter.requires_skills_for_prepare(environment))
        {
            deliveries.push(self.skills.prepare().await?);
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

    /// Drops the delivered credential copy for an environment that is being
    /// discarded or torn down. Nothing is returned to a pool — the central
    /// login is not scarce — this only avoids leaving a token behind in a home
    /// whose environment no longer exists.
    pub(crate) async fn discard_delivered_credentials(&self, environment_ref: &str) -> Result<(), String> {
        let auth = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID).join(CODEX_AUTH_FILE);
        match fs::remove_file(&auth).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove delivered Codex credential {}: {error}", auth.display())),
        }
    }

    /// Names the credential a Codex crew was handed, for operator-facing
    /// failure reporting. Static material means every crew names the same
    /// central login rather than a leased slot.
    pub(crate) fn codex_credential_source(&self) -> &Path {
        &self.codex_central_auth_path
    }

    /// Re-copies the central `auth.json` into an already-provisioned crew home
    /// so a long-lived vessel never runs on a credential older than the last
    /// refresher tick. Returns whether the delivered copy changed.
    pub(crate) async fn refresh_delivered_credentials(&self, environment_ref: &str) -> Result<bool, String> {
        let home = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID);
        if !fs::try_exists(&home).await.map_err(|error| format!("inspect Codex home {}: {error}", home.display()))? {
            return Ok(false);
        }
        let credential = read_central_credential(&self.codex_central_auth_path).await?;
        install_read_only_copy(&credential, &home.join(CODEX_AUTH_FILE)).await
    }

    pub(crate) async fn remove_environment_home(&self, environment_ref: &str) -> Result<(), String> {
        let home = self.homes_dir.join(environment_ref);
        match fs::remove_dir_all(&home).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove persistent agent home {}: {error}", home.display())),
        }
    }
}

/// Delivers the host's single central Codex login — the `auth.json` the daemon
/// refresher keeps fresh (`crates/flotilla-daemon/src/codex_central.rs`) — as a
/// per-crew read-only copy.
///
/// There is no pool: one login serves every crew because exactly one refresher
/// ever touches its single-use refresh token, so a crew handed a comfortably
/// fresh access token never refreshes and never writes `auth.json`
/// (flotilla-org/flotilla#1906). The delivery is a *copy* rather than a mount of
/// the central file because the refresher rotates it by atomic temp+rename; a
/// bind-mount would pin the pre-rotation inode and rot.
///
/// `CODEX_HOME` is split accordingly: `auth.json` is credential material owned
/// by the host, and everything else under the home — `config.toml`, `skills/`,
/// `*.sqlite`, `history`, `sessions/` — is per-crew scratch Codex writes
/// freely, seeded from a credential-free generation template
/// ([`FLOTILLA_CODEX_HOME_TEMPLATE_ENV`], flotilla-org/flotilla#1913).
struct CodexMaterialAdapter {
    central_auth_path: PathBuf,
    home_template: Option<PathBuf>,
    homes_dir: PathBuf,
    supported: bool,
}

impl CodexMaterialAdapter {
    fn new(central_auth_path: PathBuf, home_template: Option<PathBuf>, homes_dir: PathBuf, supported: bool) -> Self {
        Self { central_auth_path, home_template, homes_dir, supported }
    }

    /// Seeds the credential-free parts of a crew's `CODEX_HOME` from the
    /// generation template, without ever overwriting scratch the crew already
    /// wrote (a resumed crew keeps its `sessions/`, history, and any local
    /// `config.toml` edits) and without importing a credential if a template
    /// were ever built with one.
    async fn seed_scratch(&self, home: &Path) -> Result<(), String> {
        let Some(template) = self.home_template.as_deref() else {
            return Ok(());
        };
        let mut entries = match fs::read_dir(template).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(format!("Codex home template {} declared by {FLOTILLA_CODEX_HOME_TEMPLATE_ENV} is missing", template.display()))
            }
            Err(error) => return Err(format!("read Codex home template {}: {error}", template.display())),
        };
        while let Some(entry) =
            entries.next_entry().await.map_err(|error| format!("read Codex home template {}: {error}", template.display()))?
        {
            let name = entry.file_name();
            if name == CODEX_AUTH_FILE {
                return Err(format!("Codex home template {} must be credential-free, but carries {CODEX_AUTH_FILE}", template.display()));
            }
            let destination = home.join(&name);
            if fs::try_exists(&destination).await.map_err(|error| format!("inspect {}: {error}", destination.display()))? {
                continue;
            }
            copy_template_entry(&entry.path(), &destination).await?;
        }
        Ok(())
    }
}

/// Recursive copy of one template entry, used only for the credential-free
/// scratch seed. Iterative so the future is `Send` without boxing.
async fn copy_template_entry(source: &Path, destination: &Path) -> Result<(), String> {
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = pending.pop() {
        let metadata = fs::metadata(&source).await.map_err(|error| format!("inspect Codex home template {}: {error}", source.display()))?;
        if metadata.is_dir() {
            fs::create_dir_all(&destination).await.map_err(|error| format!("seed Codex scratch {}: {error}", destination.display()))?;
            let mut entries =
                fs::read_dir(&source).await.map_err(|error| format!("read Codex home template {}: {error}", source.display()))?;
            while let Some(entry) =
                entries.next_entry().await.map_err(|error| format!("read Codex home template {}: {error}", source.display()))?
            {
                pending.push((entry.path(), destination.join(entry.file_name())));
            }
        } else {
            fs::copy(&source, &destination)
                .await
                .map_err(|error| format!("seed Codex scratch {} from {}: {error}", destination.display(), source.display()))?;
        }
    }
    Ok(())
}

async fn read_central_credential(source: &Path) -> Result<Vec<u8>, String> {
    fs::read(source).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("central Codex credential {} is not provisioned on this host", source.display())
        } else {
            format!("read central Codex credential {}: {error}", source.display())
        }
    })
}

/// Installs `contents` at `destination` as a read-only copy, replacing any
/// previous copy atomically.
///
/// Atomic temp+rename rather than an in-place write for two reasons: the
/// previous copy is mode `0400` and so cannot be opened for writing by its own
/// owner, and the destination directory is bind-mounted into a live crew
/// container, where a half-written `auth.json` would be observable. Returns
/// whether the delivered bytes changed.
async fn install_read_only_copy(contents: &[u8], destination: &Path) -> Result<bool, String> {
    if fs::read(destination).await.is_ok_and(|delivered| delivered == contents) {
        return Ok(false);
    }
    let directory = destination.parent().unwrap_or_else(|| Path::new("."));
    let temp_path = directory.join(format!(".{}.{}.tmp", CODEX_AUTH_FILE, uuid::Uuid::new_v4()));
    let result = async {
        // `.mode()` on the open, not a follow-up `set_permissions`, so the copy
        // is never briefly writable or group/other readable.
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(CODEX_AUTH_MODE).open(&temp_path).await?;
        file.write_all(contents).await?;
        file.sync_all().await
    }
    .await;
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path).await;
        return Err(format!("write Codex credential copy {}: {error}", destination.display()));
    }
    fs::rename(&temp_path, destination)
        .await
        .map_err(|error| format!("install Codex credential copy {}: {error}", destination.display()))?;
    Ok(true)
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
            "set -eu\nmanifest=$1\ndestination=$2\ncleanup_tokens=$3\nshift 3\nexport GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0\nstaged=\"${destination}.flotilla-staging\"\nsources=\"${destination}.flotilla-sources.$$\"\ntoken_files=\nsucceeded=false\ncleanup() { rm -rf \"$staged\" \"$sources\"; if [ \"$cleanup_tokens\" = true ] || [ \"$succeeded\" != true ]; then for token_file in $token_files; do rm -f \"$token_file\"; done; fi; }\ntrap cleanup EXIT HUP INT TERM\nrm -rf \"$staged\" \"$sources\"\nmkdir -p \"$staged\" \"$sources\"\nwhile [ \"$#\" -gt 0 ]; do\n  name=$1\n  repository=$2\n  revision=$3\n  token_file=$4\n  credential=$5\n  path_count=$6\n  shift 6\n  checkout=\"$sources/$name\"\n  paths_file=\"$sources/$name.paths\"\n  sparse_file=\"$sources/$name.sparse\"\n  : >\"$paths_file\"\n  : >\"$sparse_file\"\n  while [ \"$path_count\" -gt 0 ]; do\n    printf '%s\\n' \"$1\" >>\"$paths_file\"\n    printf '/%s/\\n' \"$1\" >>\"$sparse_file\"\n    shift\n    path_count=$((path_count - 1))\n  done\n  git -C \"$sources\" init --quiet \"$name\" >/dev/null\n  git -C \"$checkout\" remote add origin \"$repository\"\n  git -C \"$checkout\" sparse-checkout set --no-cone --stdin <\"$sparse_file\" >/dev/null\n  if [ -n \"$token_file\" ]; then\n    token_files=\"$token_files $token_file\"\n    export GITHUB_TOKEN_FILE=\"$token_file\"\n    helper='!f() { [ \"$1\" = get ] || exit 0; printf \"username=x-access-token\\npassword=\"; cat \"$GITHUB_TOKEN_FILE\"; printf \"\\n\"; }; f'\n    git -C \"$checkout\" config credential.helper \"$helper\"\n    git -C \"$checkout\" fetch --quiet --depth=1 --filter=blob:none --no-tags origin \"$revision\" >/dev/null || { echo \"skill source $name credential $credential fetch failed at pinned revision $revision\" >&2; exit 1; }\n  else\n    git -C \"$checkout\" -c credential.helper= fetch --quiet --depth=1 --filter=blob:none --no-tags origin \"$revision\" >/dev/null || { echo \"skill source $name anonymous fetch failed at pinned revision $revision\" >&2; exit 1; }\n  fi\n  test \"$(git -C \"$checkout\" rev-parse FETCH_HEAD)\" = \"$revision\"\n  git -C \"$checkout\" checkout --quiet --detach FETCH_HEAD >/dev/null\n  while IFS= read -r path; do\n    if [ ! -d \"$checkout/$path\" ]; then\n      echo \"skill source $name declared path $path is missing at pinned revision $revision\" >&2\n      exit 1\n    fi\n    find \"$checkout/$path\" -type f -name SKILL.md >\"$sources/skill-files\"\n    if [ ! -s \"$sources/skill-files\" ]; then\n      echo \"skill source $name declared path $path has no SKILL.md at pinned revision $revision\" >&2\n      exit 1\n    fi\n    while IFS= read -r skill_file; do\n      skill_dir=${skill_file%/SKILL.md}\n      skill_name=${skill_dir##*/}\n      target=\"$staged/$skill_name\"\n      if [ -e \"$target\" ]; then\n        echo \"duplicate skill name $skill_name from $repository\" >&2\n        exit 1\n      fi\n      mkdir -p \"$target\"\n      cp -R \"$skill_dir\"/. \"$target\"/\n    done <\"$sources/skill-files\"\n  done <\"$paths_file\"\n  if [ -n \"$token_file\" ]; then\n    unset GITHUB_TOKEN_FILE\n  fi\ndone\ncp \"$manifest\" \"$staged/.flotilla-sources.json\"\nrm -rf \"$destination\"\nmv \"$staged\" \"$destination\"\nrm -rf \"$sources\"\nsucceeded=true\ncleanup\ntrap - EXIT HUP INT TERM".to_string(),
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
            result.map_err(|error| skill_stage_error(environment_ref, &error))?;
            info!(environment = environment_ref, adapter, sources = ?inspection.sources, "staged generation-pinned contained agent skills");
        }
        Ok(())
    }
}

fn skill_stage_error(environment_ref: &str, stderr: &str) -> String {
    // The stage script emits its actionable failure last. Git can write hints
    // and fetch progress first, which otherwise hides the failure in convoy
    // summaries that show only the beginning of the message.
    let reason = stderr.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("skill staging command failed");
    format!("stage generation-pinned skills for {environment_ref}: {reason}")
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

    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment> {
        (!environment.contains_key("CODEX_HOME"))
            .then(|| agent_environment_fragment("CODEX_HOME", CONTAINER_CODEX_HOME, "agent-material/codex codex-central".to_string()))
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

    async fn prepare(
        &self,
        environment_ref: &str,
        environment: &BTreeMap<String, String>,
    ) -> Result<Option<AgentMaterialDelivery>, String> {
        if environment.contains_key("CODEX_HOME") {
            return Ok(None);
        }
        if !self.supported {
            return Err("Codex login material delivery is supported only on Linux placement hosts".to_string());
        }
        // Read the host credential before touching the filesystem, so a host
        // whose refresher has never run fails without leaving a crew home behind.
        let credential = read_central_credential(&self.central_auth_path).await?;
        let home = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID);
        fs::create_dir_all(&home).await.map_err(|error| format!("create persistent Codex home {}: {error}", home.display()))?;
        self.seed_scratch(&home).await?;
        install_read_only_copy(&credential, &home.join(CODEX_AUTH_FILE)).await?;
        Ok(Some(AgentMaterialDelivery {
            mount: ProvisionedMount::new(home, CONTAINER_CODEX_HOME, ProvisionedMountMode::Rw),
            preflight: AgentMaterialPreflight {
                command: "codex".to_string(),
                args: vec!["login".to_string(), "status".to_string()],
                failure_context: "Codex login preflight failed".to_string(),
            },
        }))
    }
}

struct ClaudeCodeMaterialAdapter;

#[async_trait]
impl AgentMaterialAdapter for ClaudeCodeMaterialAdapter {
    fn id(&self) -> &'static str {
        CLAUDE_CODE_ADAPTER_ID
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

    async fn prepare(
        &self,
        _environment_ref: &str,
        _environment: &BTreeMap<String, String>,
    ) -> Result<Option<AgentMaterialDelivery>, String> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::{io, os::unix::fs::PermissionsExt, path::Path, process::Command, sync::Mutex};

    use flotilla_core::providers::{discovery::test_support::TestEnvVars, CommandOutput};

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

    struct PromisorRunner {
        config_base: PathBuf,
        skills_source: PathBuf,
        path: String,
    }

    #[async_trait]
    impl CommandRunner for PromisorRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            let container_manifest = format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}");
            let source_manifest = self.skills_source.join(SKILL_BUNDLE_MANIFEST).to_string_lossy().into_owned();
            let args = args
                .iter()
                .map(|arg| if *arg == container_manifest { source_manifest.clone() } else { (*arg).to_string() })
                .collect::<Vec<_>>();
            let output = Command::new(cmd)
                .args(&args)
                .current_dir(cwd)
                .env("PATH", &self.path)
                .output()
                .map_err(|error| format!("run {cmd}: {error}"))?;
            if output.status.success() {
                String::from_utf8(output.stdout).map_err(|error| format!("decode {cmd} stdout: {error}"))
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            match self.run(cmd, args, cwd, label).await {
                Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), success: true }),
                Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, success: false }),
            }
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }

        async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
            Ok(self.config_base.clone())
        }
    }

    fn promisor_runner(root: &Path) -> PromisorRunner {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("create fake Git bin");
        let git = bin.join("git");
        std::fs::write(
            &git,
            r#"#!/bin/sh
set -eu
test "$1" = -C
checkout=$2
shift 2
case "$1" in
  init)
    name=$3
    mkdir -p "$checkout/$name/.git"
    ;;
  remote|sparse-checkout)
    ;;
  config)
    test "$2" = credential.helper
    printf '%s' "$3" >"$checkout/.git/credential-helper"
    ;;
  -c|fetch)
    while [ "$1" = -c ]; do shift 2; done
    test "$1" = fetch
    eval "revision=\${$#}"
    printf '%s' "$revision" >"$checkout/.git/FETCH_HEAD"
    ;;
  rev-parse)
    cat "$checkout/.git/FETCH_HEAD"
    ;;
  checkout)
    if [ ! -s "$checkout/.git/credential-helper" ] || [ -z "${GITHUB_TOKEN_FILE:-}" ] || [ ! -s "$GITHUB_TOKEN_FILE" ]; then
      echo "fatal: could not read Username for 'https://github.com': terminal prompts disabled" >&2
      echo "fatal: could not fetch promised blob from promisor remote" >&2
      exit 1
    fi
    mkdir -p "$checkout/skills/private-source"
    printf '%s\n' '# Private source' >"$checkout/skills/private-source/SKILL.md"
    ;;
  *)
    echo "unexpected fake git command: $*" >&2
    exit 1
    ;;
esac
"#,
        )
        .expect("write fake Git");
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).expect("make fake Git executable");
        let system_path = std::env::var("PATH").expect("test process PATH");
        PromisorRunner {
            config_base: root.join("config"),
            skills_source: root.join("generation/skills"),
            path: format!("{}:{system_path}", bin.display()),
        }
    }

    /// Writes the host's central `auth.json` at exactly the path
    /// [`codex_central_auth_path`] derives from `HOME`, so the tests bind to the
    /// same contract the daemon refresher writes to.
    fn write_central_auth(home: &Path, access_token: &str) -> PathBuf {
        let path = home.join(".config/flotilla/credentials/codex-central/auth.json");
        std::fs::create_dir_all(path.parent().expect("central credential directory")).expect("create central credential directory");
        std::fs::write(&path, format!("{{\"tokens\":{{\"access_token\":\"{access_token}\"}}}}")).expect("write central auth");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("protect central auth");
        path
    }

    fn write_home_template(root: &Path) -> PathBuf {
        let template = root.join("generation/codex-home");
        std::fs::create_dir_all(template.join("skills/codex-only")).expect("create Codex home template");
        std::fs::write(template.join("config.toml"), "model = \"gpt-5-codex\"\n").expect("write template config");
        std::fs::write(template.join("skills/codex-only/SKILL.md"), "# Codex only\n").expect("write template skill");
        template
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
        AgentMaterialRegistry::new(Arc::new(TestEnvVars::new([
            ("HOME", home.to_string_lossy().into_owned()),
            (FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned()),
        ])))
    }

    fn registry_with_home_template(home: &Path, template: &Path) -> AgentMaterialRegistry {
        let skills = write_skill_sources(home);
        AgentMaterialRegistry::new(Arc::new(TestEnvVars::new([
            ("HOME", home.to_string_lossy().into_owned()),
            (FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned()),
            (FLOTILLA_CODEX_HOME_TEMPLATE_ENV, template.to_string_lossy().into_owned()),
        ])))
    }

    fn delivered_auth_mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("delivered credential metadata").permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn codex_adapter_delivers_a_read_only_copy_of_the_central_credential() {
        let temp = tempfile::tempdir().expect("tempdir");
        let central = write_central_auth(temp.path(), "access-token-one");
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
        let delivered = deliveries[0].mount.host_path.as_path().join(CODEX_AUTH_FILE);
        assert_eq!(
            std::fs::read(&delivered).expect("delivered credential"),
            std::fs::read(&central).expect("central credential"),
            "the crew must receive the refresher's own auth.json byte for byte"
        );
        assert_eq!(delivered_auth_mode(&delivered), CODEX_AUTH_MODE, "the delivered credential must be read-only");
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
        assert!(composed.contents.contains("# fragment: agent-material/codex codex-central"));
    }

    #[tokio::test]
    async fn one_central_credential_serves_every_crew_without_contention() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        let mut homes = BTreeSet::new();
        for environment_ref in ["crew-alice", "crew-bob", "crew-carol"] {
            let deliveries = registry.prepare(environment_ref, &required, &BTreeMap::new()).await.expect("prepare crew");
            homes.insert(deliveries[0].mount.host_path.as_path().to_path_buf());
        }

        assert_eq!(homes.len(), 3, "every crew must get its own writable Codex home");
        for home in &homes {
            assert_eq!(
                std::fs::read_to_string(home.join(CODEX_AUTH_FILE)).expect("delivered credential"),
                std::fs::read_to_string(temp.path().join(".config/flotilla/credentials/codex-central/auth.json"))
                    .expect("central credential")
            );
        }
    }

    #[tokio::test]
    async fn an_unprovisioned_central_credential_fails_with_an_actionable_message() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());

        let error = registry
            .prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("a host without a central Codex login cannot deliver one");

        assert!(error.contains("codex-central/auth.json"), "the failure must name the central path: {error}");
        assert!(error.contains("not provisioned"), "the failure must say the host lacks the credential: {error}");
        assert!(
            !temp.path().join(".local/share/flotilla/agent-homes/env-a").exists(),
            "a host that cannot deliver a credential must not leave a crew home behind"
        );
        assert_eq!(
            registry.codex_credential_source(),
            temp.path().join(".config/flotilla/credentials/codex-central/auth.json"),
            "operator-facing failures name the one central login, not a per-crew slot"
        );
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

    #[test]
    fn skill_stage_error_leads_with_declared_path_failure() {
        let stderr = "hint: Using 'master' as the name for the initial branch.\nFrom https://github.com/flotilla-org/cleat\n * branch 0f23944 -> FETCH_HEAD\nskill source cleat declared path skills is missing at pinned revision 0f23944\n";
        let message = skill_stage_error("crew-work", stderr);
        assert_eq!(
            message,
            "stage generation-pinned skills for crew-work: skill source cleat declared path skills is missing at pinned revision 0f23944"
        );
    }

    #[tokio::test]
    async fn failed_skill_source_fetch_names_source_and_pinned_revision() {
        for credentialed in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let repository = temp.path().join("repository");
            let init = Command::new("git").args(["init", "--quiet"]).arg(&repository).output().expect("initialize local repository");
            assert!(init.status.success(), "git init failed: {}", String::from_utf8_lossy(&init.stderr));

            let skills = temp.path().join("generation/skills");
            std::fs::create_dir_all(&skills).expect("create skill bundle");
            let revision = "0000000000000000000000000000000000000000";
            let credential = if credentialed { r#", "credential": "local-test""# } else { "" };
            std::fs::write(
                skills.join(SKILL_BUNDLE_MANIFEST),
                format!(
                    r#"{{"schema_version":5,"sources":[{{"name":"missing-source","repository":"{}","revision":"{revision}"{credential}}}]}}"#,
                    repository.display()
                ),
            )
            .expect("write skill bundle manifest");
            let token_file = temp.path().join("source.token");
            let tokens = if credentialed {
                std::fs::write(&token_file, "unused-test-token").expect("write source token");
                BTreeMap::from([("missing-source".to_string(), token_file.clone())])
            } else {
                BTreeMap::new()
            };
            let runner = PromisorRunner {
                config_base: temp.path().join("config"),
                skills_source: skills.clone(),
                path: std::env::var("PATH").expect("test process PATH"),
            };
            let registry =
                AgentMaterialRegistry::new(Arc::new(TestEnvVars::new([(FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned())])));
            let environment = vec![("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())];
            let error = registry
                .stage_skills("crew-fetch", &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]), &environment, &tokens, &runner)
                .await
                .expect_err("fetch of an absent revision must fail");
            assert!(!error.is_empty(), "fetch failure must produce a diagnostic");
            assert!(error.contains("missing-source"), "diagnostic must name the source: {error}");
            assert!(error.contains(revision), "diagnostic must name the pinned revision: {error}");
            assert!(error.contains("fetch failed"), "diagnostic must identify the failed operation: {error}");
            if credentialed {
                assert!(!token_file.exists(), "failed staging must remove the source token");
            }
        }
    }

    #[tokio::test]
    async fn credentialed_partial_clone_keeps_credentials_for_lazy_checkout_fetch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation skill source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("write private source manifest");
        let runner = promisor_runner(temp.path());
        let destination = runner.config_base.join("claude/skills");
        let token_file = temp.path().join("private-skills.token");
        std::fs::write(&token_file, "secret-token").expect("write source token");

        registry
            .stage_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file.clone())]),
                &runner,
            )
            .await
            .expect("stage private partial-clone source through lazy checkout fetch");

        assert!(destination.join("private-source/SKILL.md").is_file());
        assert!(!token_file.exists(), "one-shot source token must be cleaned after materialization");
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
    async fn redelivery_replaces_a_stale_copy_and_preserves_per_crew_scratch() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        let delivery = registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("initial delivery");
        let codex_home = delivery[0].mount.host_path.as_path().to_path_buf();
        std::fs::create_dir_all(codex_home.join("sessions/2026/09/13")).expect("real rollout tree");
        std::fs::write(codex_home.join("sessions/2026/09/13/rollout.jsonl"), "{\"type\":\"session_meta\"}\n").expect("rollout");
        std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5\"\n").expect("config");

        assert!(
            !registry.refresh_delivered_credentials("env-a").await.expect("idempotent redelivery"),
            "an unchanged central credential must not rewrite the copy"
        );
        write_central_auth(temp.path(), "access-token-two");
        assert!(registry.refresh_delivered_credentials("env-a").await.expect("redeliver rotated credential"));

        let delivered = codex_home.join(CODEX_AUTH_FILE);
        assert!(
            std::fs::read_to_string(&delivered).expect("delivered credential").contains("access-token-two"),
            "a long-lived crew must end up on the refresher's current token"
        );
        assert_eq!(delivered_auth_mode(&delivered), CODEX_AUTH_MODE, "redelivery must keep the copy read-only");
        assert_eq!(std::fs::read_to_string(codex_home.join("config.toml")).expect("preserved config"), "model = \"gpt-5\"\n");
        assert_eq!(
            std::fs::read_to_string(codex_home.join("sessions/2026/09/13/rollout.jsonl")).expect("preserved rollout"),
            "{\"type\":\"session_meta\"}\n"
        );
    }

    #[tokio::test]
    async fn redelivery_ignores_an_environment_with_no_codex_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry(temp.path());

        assert!(!registry.refresh_delivered_credentials("env-never-provisioned").await.expect("no home is not an error"));
    }

    #[tokio::test]
    async fn discarding_an_environment_drops_only_its_delivered_credential() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry(temp.path());
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        let delivery = registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("initial delivery");
        let codex_home = delivery[0].mount.host_path.as_path().to_path_buf();
        std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5\"\n").expect("config");

        registry.discard_delivered_credentials("env-a").await.expect("discard delivered credential");

        assert!(!codex_home.join(CODEX_AUTH_FILE).exists(), "a discarded environment must not retain a token");
        assert!(codex_home.join("config.toml").exists(), "credential-free scratch is not credential material");
        registry.discard_delivered_credentials("env-a").await.expect("repeat discard is idempotent");
    }

    #[tokio::test]
    async fn the_generation_home_template_seeds_scratch_without_clobbering_crew_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let template = write_home_template(temp.path());
        let registry = registry_with_home_template(temp.path(), &template);
        let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);

        let delivery = registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("seeded delivery");
        let codex_home = delivery[0].mount.host_path.as_path().to_path_buf();

        assert_eq!(std::fs::read_to_string(codex_home.join("config.toml")).expect("seeded config"), "model = \"gpt-5-codex\"\n");
        assert_eq!(
            std::fs::read_to_string(codex_home.join("skills/codex-only/SKILL.md")).expect("seeded nested template file"),
            "# Codex only\n"
        );

        std::fs::write(codex_home.join("config.toml"), "model = \"crew-override\"\n").expect("crew edit");
        registry.prepare("env-a", &required, &BTreeMap::new()).await.expect("reseeded delivery");

        assert_eq!(
            std::fs::read_to_string(codex_home.join("config.toml")).expect("preserved crew config"),
            "model = \"crew-override\"\n",
            "seeding must never overwrite scratch the crew already owns"
        );
    }

    #[tokio::test]
    async fn a_home_template_carrying_a_credential_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let template = write_home_template(temp.path());
        std::fs::write(template.join(CODEX_AUTH_FILE), "{\"tokens\":{}}").expect("write template credential");
        let registry = registry_with_home_template(temp.path(), &template);

        let error = registry
            .prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("a template carrying auth.json must be refused");

        assert!(error.contains("credential-free"), "unexpected failure: {error}");
    }

    #[tokio::test]
    async fn a_declared_but_missing_home_template_fails_loudly() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry_with_home_template(temp.path(), &temp.path().join("generation/absent-codex-home"));

        let error = registry
            .prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("a declared template that is not on disk is a generation defect");

        assert!(error.contains(FLOTILLA_CODEX_HOME_TEMPLATE_ENV), "unexpected failure: {error}");
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
        write_central_auth(temp.path(), "access-token-one");
        let registry = AgentMaterialRegistry::new(Arc::new(TestEnvVars::new([("HOME", temp.path().to_string_lossy().into_owned())])));

        let error = registry
            .prepare("env-a", &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("contained Claude must require pinned skill sources");

        assert!(error.contains(FLOTILLA_SKILLS_DIR_ENV), "unexpected failure: {error}");

        let codex_error = registry
            .prepare("env-b", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("contained Codex must require pinned skill sources");
        assert!(codex_error.contains(FLOTILLA_SKILLS_DIR_ENV), "unexpected failure: {codex_error}");
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
