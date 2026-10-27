use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_environment::environment::runner::CONTAINED_CODEX_HOME;
use flotilla_environment::environment::runner::CONTAINED_WRITABLE_CONFIG_BASE;
use flotilla_environment::environment::runner::{CONTAINED_CODEX_AUTH_FILE, HARNESS_AUTH_FILES};
use flotilla_environment::environment::ProvisionedMount;
use flotilla_environment::environment::ProvisionedMountMode;
use flotilla_environment::environment::{EntryKind, VesselAgent};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use flotilla_vcs::providers::skill_source::stage_git_skill_sources;
use flotilla_vcs::providers::skill_source::STAGE_DIAGNOSTIC_PREFIX;
use flotilla_vcs::providers::skill_source::STAGE_RETRYABLE_PREFIX;
use flotilla_vcs::providers::skill_source::STAGE_SOURCE_PREFIX;
use futures::future::BoxFuture;
use tokio::{fs, io::AsyncWriteExt};
use tracing::{info, warn};
use url::Url;

use crate::{
    codex_central::codex_central_auth_path,
    vessel_config::{agent_environment_fragment, Fragment},
};

const CODEX_ADAPTER_ID: &str = "codex";
const CLAUDE_CODE_ADAPTER_ID: &str = "claude-code";
pub const FLOTILLA_SKILLS_DIR_ENV: &str = "FLOTILLA_SKILLS_DIR";
/// Generation-provided, credential-free `CODEX_HOME` template that seeds each
/// crew's writable scratch (`config.toml`, `skills/`, static defaults). The
/// generation-side artifact and its `fleet-install` wiring are
/// flotilla-org/flotilla#1913; this is the consumer contract it satisfies.
/// Absent, a crew simply starts from an empty scratch directory.
pub(crate) const FLOTILLA_CODEX_HOME_TEMPLATE_ENV: &str = "FLOTILLA_CODEX_HOME_TEMPLATE";
const SKILL_BUNDLE_MANIFEST: &str = ".flotilla-sources.json";
const CONTAINER_SKILLS_SOURCE: &str = "/run/flotilla/skills";
pub const CONTAINER_CODEX_HOME: &str = CONTAINED_CODEX_HOME;
/// The one credential file inside a crew's `CODEX_HOME`. Everything else under
/// that directory is per-crew scratch Codex may write freely.
const CODEX_AUTH_FILE: &str = CONTAINED_CODEX_AUTH_FILE;
/// Codex must never rewrite the delivered credential: a crew's copy is a
/// read-only snapshot of the central login, kept fresh by the host refresher.
const CODEX_AUTH_MODE: u32 = 0o400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMaterialPreflight {
    pub command: String,
    pub args: Vec<String>,
    pub failure_context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMaterialDelivery {
    /// Host snapshot and guest destination. Docker pushes these files after
    /// hello; preceding providers can still lower this descriptor to a mount.
    pub mount: ProvisionedMount,
    pub preflight: AgentMaterialPreflight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSourceCredentialRequest {
    pub source: String,
    pub repository: String,
    pub revision: String,
    pub credential: String,
}

/// The home variable and directory name a harness's crews use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessHome {
    /// The variable that points the harness at its home (`CLAUDE_CONFIG_DIR`).
    pub variable: &'static str,
    /// The harness's directory name inside a requester's home (`claude`).
    pub name: &'static str,
}

/// The login a crew links into its own home: from the vessel's delivered home, or,
/// without one, the target's ambient login. Credentials are linked, never copied, so
/// a refresh in one place reaches every crew (#1906).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessLogin {
    base: Option<PathBuf>,
}

impl HarnessLogin {
    /// Content identity of what the crew's home links to.
    pub fn digest(&self) -> String {
        match &self.base {
            Some(base) => format!("delivered:{}", base.display()),
            None => "ambient".to_string(),
        }
    }

    /// Link the login into `home` through the vessel's agent. A delivered home's static
    /// config seeds the crew once; later edits there never overwrite the crew's own.
    /// Credentials are linked, never copied, so a refresh reaches every crew. Without a
    /// delivered home, the target's ambient login is linked only when the home has
    /// none, and the host user's configuration is never copied or written.
    pub async fn link(&self, home: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        let failed = |error: String| format!("link harness login into {}: {error}", home.display());
        let Some(base) = &self.base else {
            let ambient = agent
                .commands()
                .run(
                    "sh",
                    &["-c", r#"printf '%s' "${CODEX_HOME:-$HOME/.codex}""#, "flotilla-ambient-home"],
                    Path::new("/"),
                    &ChannelLabel::Default,
                )
                .await
                .map_err(failed)?;
            let (link, login) = (home.join("auth.json"), Path::new(ambient.trim()).join("auth.json"));
            if agent.stat(&link).await.map_err(failed)?.is_none()
                && matches!(agent.stat(&login).await.map_err(failed)?, Some(EntryKind::File | EntryKind::Symlink))
            {
                agent.symlink(&link, &login).await.map_err(failed)?;
            }
            return Ok(());
        };
        for file in ["config.toml", "settings.json", ".claude.json"] {
            if agent.stat(&home.join(file)).await.map_err(failed)?.is_none() {
                if let Some(bytes) = agent.read(&base.join(file), STATIC_CONFIG_LIMIT).await.map_err(failed)? {
                    agent.write(&home.join(file), bytes, 0o600).await.map_err(failed)?;
                }
            }
        }
        for file in HARNESS_AUTH_FILES {
            // A delivered login may itself be a link to the delivered material.
            if matches!(agent.stat(&base.join(file)).await.map_err(failed)?, Some(EntryKind::File | EntryKind::Symlink)) {
                agent.symlink(&home.join(file), &base.join(file)).await.map_err(failed)?;
            }
        }
        Ok(())
    }
}

/// A harness's static config file is small; anything larger is not one to seed.
const STATIC_CONFIG_LIMIT: u64 = 1024 * 1024;

/// A crew's frozen skill selection with the authorized sources it is fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTree {
    sources: Vec<SkillSource>,
    selected: Vec<flotilla_resources::SkillCatalogEntry>,
}

impl SkillTree {
    /// Content identity: every source at its pinned revision, and every selected path.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let identity = serde_json::json!({"sources": self.sources, "selected": self.selected});
        format!("{:x}", Sha256::digest(identity.to_string().as_bytes()))
    }

    /// The source credentials staging needs, one per credentialed source.
    pub fn credentials(&self) -> Vec<SkillSourceCredentialRequest> {
        self.sources
            .iter()
            .filter_map(|source| {
                source.credential.clone().map(|credential| SkillSourceCredentialRequest {
                    source: source.name.clone(),
                    repository: source.repository.clone(),
                    revision: source.revision.clone(),
                    credential,
                })
            })
            .collect()
    }
}

#[async_trait]
trait AgentMaterialAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment>;
    fn config_home_variable(&self) -> &'static str;
    fn is_managed_config_home(&self, config_home: &Path, config_base: &Path) -> bool;
    fn externally_managed_home_opts_out_of_skills(&self) -> bool;

    /// Whether this harness's crews run in their requesters' bound homes (#3907).
    fn requester_bound(&self) -> bool {
        false
    }

    /// Directory name of this harness's home under a host environment's
    /// flotilla-owned agent home.
    fn host_home_name(&self) -> &'static str;

    fn requires_skills_for_prepare(&self, environment: &BTreeMap<String, String>) -> bool {
        !self.externally_managed_home_opts_out_of_skills() || !environment.contains_key(self.config_home_variable())
    }

    async fn prepare(&self, environment_ref: &str, environment: &BTreeMap<String, String>)
        -> Result<Option<AgentMaterialDelivery>, String>;
}

pub struct AgentMaterialRegistry {
    homes_dir: PathBuf,
    codex_central_auth_path: PathBuf,
    adapters: BTreeMap<&'static str, Arc<dyn AgentMaterialAdapter>>,
    skills: SkillBundle,
    capture_locks: tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl AgentMaterialRegistry {
    pub fn new(env: Arc<dyn EnvVars>) -> Self {
        Self::with_codex_delivery_support(env, cfg!(target_os = "linux"))
    }

    /// Supply the placement host's Codex delivery capability explicitly.
    /// This seam is intended for tests and platform composition. Tests can model
    /// Linux delivery on any host; production composition must retain the
    /// Linux-only policy and should use [`Self::new`].
    pub fn with_codex_delivery_support(env: Arc<dyn EnvVars>, codex_delivery_supported: bool) -> Self {
        let home = env.get("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/flotilla"));
        let homes_dir = home.join(".local/share/flotilla/agent-homes");
        let codex_central_auth_path = codex_central_auth_path(&*env);
        let skills = SkillBundle::new(env.get(FLOTILLA_SKILLS_DIR_ENV).map(PathBuf::from));
        let codex: Arc<dyn AgentMaterialAdapter> = Arc::new(CodexMaterialAdapter::new(
            codex_central_auth_path.clone(),
            env.get(FLOTILLA_CODEX_HOME_TEMPLATE_ENV).map(PathBuf::from),
            homes_dir.clone(),
            codex_delivery_supported,
        ));
        let claude_code: Arc<dyn AgentMaterialAdapter> = Arc::new(ClaudeCodeMaterialAdapter);
        let mut adapters = BTreeMap::from([(codex.id(), codex), (claude_code.id(), Arc::clone(&claude_code))]);
        // The canary exercises normal skill staging without login material.
        // Its private config home uses the same layout as Claude crews.
        if env.get("FLOTILLA_FLEET_CANARY").as_deref() == Some("1") {
            adapters.insert("fleet-canary", claude_code);
        }
        Self { homes_dir, codex_central_auth_path, adapters, skills, capture_locks: Default::default() }
    }

    pub fn prepare<'a>(
        &'a self,
        environment_ref: &'a str,
        required_adapters: &'a BTreeSet<String>,
        environment: &'a BTreeMap<String, String>,
    ) -> BoxFuture<'a, Result<Vec<AgentMaterialDelivery>, String>> {
        Box::pin(async move {
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
        })
    }

    /// The home a crew of `adapter` runs in, when this registry manages its material.
    pub fn harness_home(&self, adapter: &str) -> Option<HarnessHome> {
        self.adapters
            .get(adapter)
            .map(|material| HarnessHome { variable: material.config_home_variable(), name: material.host_home_name() })
    }

    /// The login a crew of `adapter` links into its own home, when its harness has one
    /// to share: the managed home the vessel's credential delivery resolved
    /// (`environment` is the resolved launch environment), otherwise the target's
    /// ambient login. Never one whose crews run on a bound account (#3907).
    pub fn harness_login(&self, adapter: &str, environment: &[(String, String)], config_base: &Path) -> Option<HarnessLogin> {
        let material = self.adapters.get(adapter)?;
        if material.requester_bound() {
            return None;
        }
        let variable = material.config_home_variable();
        let base = environment.iter().rev().find(|(name, _)| name == variable).map(|(_, home)| PathBuf::from(home)).filter(|home| {
            // `starts_with` compares components lexically, so a `..` could climb out of
            // the base while appearing inside it.
            home.is_absolute()
                && home.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
                && material.is_managed_config_home(home, config_base)
        });
        Some(HarnessLogin { base })
    }

    /// A crew's frozen skill selection, resolved against this generation's authorized
    /// sources (ADR 0052): the tree the access layer delivers into the crew's home.
    pub async fn skill_tree(&self, selected: &[flotilla_resources::SkillCatalogEntry]) -> Result<SkillTree, String> {
        let sources = if selected.is_empty() { Ok(Vec::new()) } else { self.skills.selected_sources(selected).await };
        match sources.and_then(|sources| {
            one_repository_per_credential(&sources).map_err(|(source, error)| format!("{error}\n{STAGE_SOURCE_PREFIX}{source}"))?;
            Ok(sources)
        }) {
            Ok(sources) => Ok(SkillTree { sources, selected: selected.to_vec() }),
            Err(error) => Err(self.report_skill_failure(&error, selected).await),
        }
    }

    /// Warn with the failing source, its frozen revision and its credential, and return the
    /// error without its source marker.
    async fn report_skill_failure(&self, error: &str, selected: &[flotilla_resources::SkillCatalogEntry]) -> String {
        let source_name = error.lines().rev().find_map(|line| line.strip_prefix(STAGE_SOURCE_PREFIX));
        let source = match self.skills.source.clone() {
            Some(path) => tokio::task::spawn_blocking(move || inspect_skill_sources(&path))
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|inspection| inspection.sources.into_iter().find(|source| Some(source.name.as_str()) == source_name)),
            None => None,
        };
        let revision = selected
            .iter()
            .find(|entry| Some(entry.source.as_str()) == source_name)
            .map(|entry| entry.revision.as_str())
            .or_else(|| source.as_ref().map(|source| source.revision.as_str()));
        warn!(
            source = source_name.unwrap_or("skill-bundle"),
            revision = revision.unwrap_or("unresolved"),
            credential = source.as_ref().and_then(|source| source.credential.as_deref()).unwrap_or("none"),
            %error,
            "skill staging failed"
        );
        error.lines().filter(|line| !line.starts_with(STAGE_SOURCE_PREFIX)).collect::<Vec<_>>().join("\n")
    }

    /// Place `tree` at `destination` through `runner`, fetching each source at its
    /// pinned revision in the vessel. Removes the source token files afterwards,
    /// whether or not staging succeeds.
    pub async fn stage_skill_tree(
        &self,
        tree: &SkillTree,
        destination: &Path,
        source_token_files: &BTreeMap<String, PathBuf>,
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        match self.skills.stage(tree, destination, source_token_files, runner).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(error) = remove_source_token_files(source_token_files, runner).await {
                    warn!(%error, "failed to clean skill-source tokens after staging error");
                }
                Err(self.report_skill_failure(&error, &tree.selected).await)
            }
        }
    }

    pub fn fragments(&self, required_adapters: &BTreeSet<String>, environment: &BTreeMap<String, String>) -> Vec<Fragment> {
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
    pub fn discard_delivered_credentials<'a>(&'a self, environment_ref: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let auth = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID).join(CODEX_AUTH_FILE);
            match fs::remove_file(&auth).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("remove delivered Codex credential {}: {error}", auth.display())),
            }
        })
    }

    /// Names the credential a Codex crew was handed, for operator-facing
    /// failure reporting. Static material means every crew names the same
    /// central login rather than a leased slot.
    pub fn codex_credential_source(&self) -> &Path {
        &self.codex_central_auth_path
    }

    /// Only this registry's own host directory establishes Shared Codex transport.
    /// A user mount at the same guest path must retain pushed rotation/capture.
    pub fn codex_home_is_shared(&self, environment_ref: &str, mounts: &[ProvisionedMount]) -> bool {
        if !matches!(Path::new(environment_ref).components().collect::<Vec<_>>().as_slice(), [Component::Normal(_)]) {
            return false;
        }
        let home = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID);
        mounts.iter().any(|mount| {
            mount.mode == ProvisionedMountMode::Rw
                && mount.environment_path.as_path() == Path::new(CONTAINER_CODEX_HOME)
                && mount.host_path.as_path() == home
        })
    }

    /// Re-copies the central `auth.json` into an already-provisioned crew home
    /// so a long-lived vessel never runs on a credential older than the last
    /// refresher tick. Returns whether the delivered copy changed.
    pub fn refresh_delivered_credentials<'a>(&'a self, environment_ref: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let home = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID);
            if !fs::try_exists(&home).await.map_err(|error| format!("inspect Codex home {}: {error}", home.display()))? {
                return Ok(false);
            }
            let credential = read_central_credential(&self.codex_central_auth_path).await?;
            install_read_only_copy(&credential, &home.join(CODEX_AUTH_FILE)).await
        })
    }

    /// Rotate a Pushed credential through the environment's runner (the
    /// resident agent on Docker). Host staging is not mounted into new vessels.
    /// Re-send unchanged bytes too, so a previously interrupted push converges.
    pub fn refresh_delivered_credentials_through<'a>(
        &'a self,
        environment_ref: &'a str,
        runner: &'a dyn CommandRunner,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            if !matches!(Path::new(environment_ref).components().collect::<Vec<_>>().as_slice(), [Component::Normal(_)]) {
                return Err("environment identity must name one directory".into());
            }

            let source = self.homes_dir.join(environment_ref).join(CODEX_ADAPTER_ID).join(CODEX_AUTH_FILE);
            if !fs::try_exists(&source).await.map_err(|e| e.to_string())? {
                return Ok(false);
            }
            let changed = self.refresh_delivered_credentials(environment_ref).await?;
            let bytes = fs::read(&source).await.map_err(|e| e.to_string())?;
            let contents = std::str::from_utf8(&bytes).map_err(|_| "Codex credential must be UTF-8 JSON")?;
            runner.write_file_with_mode(&Path::new(CONTAINER_CODEX_HOME).join(CODEX_AUTH_FILE), contents, CODEX_AUTH_MODE).await?;
            Ok(changed)
        })
    }

    /// Capture guest session/scratch records through the agent before container
    /// teardown. The archive excludes credentials, and is kept with the existing
    /// environment-home archive rather than reading guest files via host mounts.
    pub fn capture_environment_home<'a>(
        &'a self,
        environment_ref: &'a str,
        runner: &'a dyn CommandRunner,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if !matches!(Path::new(environment_ref).components().collect::<Vec<_>>().as_slice(), [Component::Normal(_)]) {
                return Err("environment identity must name one directory".into());
            }

            let home = self.homes_dir.join(environment_ref);
            if !fs::try_exists(home.join(CODEX_ADAPTER_ID)).await.map_err(|e| e.to_string())? {
                return Ok(());
            }
            let capture_lock = self.capture_locks.lock().await.entry(environment_ref.into()).or_default().clone();
            let _capture = capture_lock.lock().await;
            let mut entries = fs::read_dir(&home).await.map_err(|e| e.to_string())?;
            while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name
                    .strip_prefix(".guest-codex-")
                    .and_then(|name| name.strip_suffix(".tar"))
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                    && entry.file_type().await.map_err(|e| e.to_string())?.is_file()
                {
                    fs::remove_file(entry.path()).await.map_err(|e| e.to_string())?;
                }
            }
            let temporary = home.join(format!(".guest-codex-{}.tar", uuid::Uuid::new_v4()));
            fs::write(&temporary, []).await.map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await.map_err(|e| e.to_string())?;
            }
            let result = runner
                .run_to_file("tar", &["-C", CONTAINER_CODEX_HOME, "--exclude=auth.json", "-cf", "-", "."], Path::new("/"), &temporary)
                .await;
            if let Err(error) = result {
                let _ = fs::remove_file(&temporary).await;
                return Err(format!("capture Codex sessions through agent: {error}"));
            }
            fs::rename(&temporary, home.join("guest-codex.tar")).await.map_err(|e| e.to_string())
        })
    }

    pub fn archive_environment_home<'a>(&'a self, convoy_ref: &'a str, environment_ref: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            for identity in [convoy_ref, environment_ref] {
                if !matches!(Path::new(identity).components().collect::<Vec<_>>().as_slice(), [std::path::Component::Normal(_)]) {
                    return Err("archive identity must name one directory".into());
                }
            }
            self.discard_delivered_credentials(environment_ref).await?;
            let home = self.homes_dir.join(environment_ref);
            if !fs::try_exists(&home).await.map_err(|error| error.to_string())? {
                return Ok(());
            }
            let archive_root = self.homes_dir.parent().ok_or("agent homes parent missing")?.join("session-archive");
            fs::create_dir_all(&archive_root).await.map_err(|error| format!("create session archive root: {error}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&archive_root, std::fs::Permissions::from_mode(0o700))
                    .await
                    .map_err(|error| format!("protect session archive root: {error}"))?;
            }
            let archive = archive_root.join(convoy_ref);
            fs::create_dir_all(&archive).await.map_err(|error| format!("create session archive: {error}"))?;
            let mut destination = archive.join(environment_ref);
            loop {
                // Reserve an empty destination exclusively. Rename replaces only our
                // reservation, never a pre-existing archive, even during concurrent cleanup.
                match fs::create_dir(&destination).await {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        destination =
                            archive.join(format!("{environment_ref}-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()));
                    }
                    Err(error) => return Err(format!("reserve session archive: {error}")),
                }
            }
            // Retention starts at teardown, rather than the last session write.
            // A failed reset must preserve the home for retry: otherwise an old
            // home could be expired immediately after successful archival.
            let result = async {
                let directory = fs::File::open(&home).await?;
                directory.into_std().await.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
                // Both roots must be on the same filesystem: do not fall back to
                // copying on EXDEV, since teardown promises an atomic move.
                fs::rename(&home, &destination).await
            }
            .await;
            if let Err(error) = result {
                let _ = fs::remove_dir(&destination).await;
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("archive persistent agent home {}: {error}", home.display()));
                }
            }
            Ok(())
        })
    }
}

/// Delivers the host's single central Codex login — the `auth.json` the daemon
/// refresher keeps fresh (`crates/flotilla-credentials/src/codex_central.rs`) — as a
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SkillSource {
    name: String,
    repository: String,
    revision: String,
    credential: Option<String>,
    #[serde(default = "default_skill_source_paths")]
    paths: Vec<String>,
}

/// A standing convoy retains its admission pin after a generation roll. The
/// current supply still authorizes the repository, credential and path roots;
/// only the revision comes from the durable selection (ADR 0052).
fn resolve_frozen_sources(sources: &[SkillSource], selected: &[flotilla_resources::SkillCatalogEntry]) -> Result<Vec<SkillSource>, String> {
    let mut resolved = BTreeMap::new();
    for entry in selected {
        let source = sources.iter().find(|source| source.name == entry.source);
        let credential = source.and_then(|source| source.credential.as_deref()).unwrap_or("none");
        let context = format!("skill source {} revision {} credential {}", entry.source, entry.revision, credential);
        let resolve = || -> Result<SkillSource, String> {
            let source = source.ok_or_else(|| format!("{context}: source is not declared by this generation"))?;
            if entry.revision.len() != 40 || !entry.revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{context}: frozen revision must be a full commit SHA"));
            }
            flotilla_resources::validate_skill_ref(&entry.name).map_err(|error| format!("{context}: {error}"))?;
            flotilla_resources::crew_defaults::validate_skill_path(&entry.path).map_err(|error| format!("{context}: {error}"))?;
            if entry.repository != source.repository
                && flotilla_resources::crew_defaults::source_repository_path(&source.repository) != Some(entry.repository.as_str())
            {
                return Err(format!(
                    "{context}: frozen repository {} differs from authorized source {}",
                    entry.repository, source.repository
                ));
            }
            if !source.paths.iter().any(|path| flotilla_resources::crew_defaults::path_within_source(&entry.path, path)) {
                return Err(format!("{context}: frozen skill {} is outside the authorized source paths", entry.path));
            }
            let mut source = source.clone();
            source.revision = entry.revision.clone();
            Ok(source)
        };
        let source = resolve().map_err(|error| format!("{error}\n{STAGE_SOURCE_PREFIX}{}", entry.source))?;
        if resolved.keys().any(|(name, revision)| name == &source.name && revision != &source.revision) {
            return Err(format!(
                "{context}: one crew cannot select two revisions of the same source\n{STAGE_SOURCE_PREFIX}{}",
                entry.source
            ));
        }
        resolved.insert((source.name.clone(), source.revision.clone()), source);
    }
    Ok(resolved.into_values().collect())
}

/// Validate all frozen crews aboard one vessel with provisioning's combined
/// source authority, then stage each crew separately to retain name isolation.
pub fn validate_frozen_vessel_skills<'a>(
    source: &'a Path,
    crews: &'a BTreeMap<String, Vec<flotilla_resources::SkillCatalogEntry>>,
    declared_credentials: &'a BTreeMap<String, flotilla_resources::CredentialSpecSpec>,
    credential_tokens: &'a BTreeMap<String, PathBuf>,
    runner: &'a dyn CommandRunner,
) -> BoxFuture<'a, Result<(), String>> {
    Box::pin(async move {
        let all = crews.values().flatten().cloned().collect::<Vec<_>>();
        let sources = resolve_frozen_sources(&inspect_skill_sources(source)?.sources, &all)?;
        authorize_frozen_sources(&sources, declared_credentials)?;
        for (crew, selected) in crews {
            if !selected.is_empty() {
                stage_frozen_skills(&sources, selected, credential_tokens, runner)
                    .await
                    .map_err(|error| format!("crew {crew}: {error}"))?;
            }
        }
        Ok(())
    })
}

fn authorize_frozen_sources(
    sources: &[SkillSource],
    declared_credentials: &BTreeMap<String, flotilla_resources::CredentialSpecSpec>,
) -> Result<(), String> {
    for source in sources {
        if let Some(credential) = &source.credential {
            let spec = declared_credentials.get(credential).ok_or_else(|| {
                format!("skill source {} revision {}: credential {credential} has no declared CredentialSpec", source.name, source.revision)
            })?;
            crate::credential::skill_source_repository(credential, spec, &source.repository)
                .map_err(|error| format!("skill source {} revision {}: {error}", source.name, source.revision))?;
        }
    }
    one_repository_per_credential(sources).map_err(|(_, error)| error)
}

/// A source credential is minted narrowed to one repository, so sources sharing a
/// credential must share their repository.
fn one_repository_per_credential(sources: &[SkillSource]) -> Result<(), (String, String)> {
    let mut credential_repositories = BTreeMap::new();
    for source in sources {
        if let Some(credential) = &source.credential {
            if credential_repositories.insert(credential, &source.repository).is_some_and(|previous| previous != &source.repository) {
                let error = format!(
                    "skill source {} credential {credential}: one credential cannot be narrowed to multiple source repositories",
                    source.name
                );
                return Err((source.name.clone(), error));
            }
        }
    }
    Ok(())
}

/// Candidate-side pre-roll probe. Uses the same supply authorization and exact
/// revision staging as provisioning, without touching a crew's staged skills.
/// Token files are explicit operator inputs, keyed by the declared credential;
/// ambient Git credentials are disabled by the staging implementation.
#[cfg(test)]
async fn validate_frozen_skills(
    source: &Path,
    selected: &[flotilla_resources::SkillCatalogEntry],
    declared_credentials: &BTreeMap<String, flotilla_resources::CredentialSpecSpec>,
    credential_tokens: &BTreeMap<String, PathBuf>,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let sources = resolve_frozen_sources(&inspect_skill_sources(source)?.sources, selected)?;
    authorize_frozen_sources(&sources, declared_credentials)?;
    stage_frozen_skills(&sources, selected, credential_tokens, runner).await
}

/// Stage one crew from the vessel's already resolved and authorized source set.
/// Filter before staging to keep other crews' sources and names isolated.
async fn stage_frozen_skills(
    sources: &[SkillSource],
    selected: &[flotilla_resources::SkillCatalogEntry],
    credential_tokens: &BTreeMap<String, PathBuf>,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let sources = sources.iter().filter(|source| selected.iter().any(|entry| entry.source == source.name)).collect::<Vec<_>>();
    let scratch = tempfile::tempdir().map_err(|error| format!("create frozen-skill probe directory: {error}"))?;
    let mut args = vec![
        "flotilla-stage-skills".into(),
        format!("json:{}", serde_json::json!({"schema_version": 5, "sources": sources})),
        scratch.path().join("selected").to_string_lossy().into_owned(),
        "false".into(),
        scratch.path().join("cache").to_string_lossy().into_owned(),
    ];
    for (index, source) in sources.iter().enumerate() {
        let token = if let Some(credential) = &source.credential {
            let path = credential_tokens.get(credential).ok_or_else(|| {
                format!(
                    "skill source {} revision {}: credential {credential} requires an explicit probe token file",
                    source.name, source.revision
                )
            })?;
            // Staging deletes tokens after failed probes; protect the operator's
            // original by supplying a private, disposable copy.
            let copy = scratch.path().join(format!("token-{index}"));
            std::fs::copy(path, &copy).map_err(|error| format!("copy probe token for {credential}: {error}"))?;
            copy.to_string_lossy().into_owned()
        } else {
            String::new()
        };
        args.extend([
            source.name.clone(),
            source.repository.clone(),
            source.revision.clone(),
            token,
            source.credential.clone().unwrap_or_default(),
            source.paths.len().to_string(),
        ]);
        args.extend(source.paths.clone());
        let entries = selected.iter().filter(|entry| entry.source == source.name).collect::<Vec<_>>();
        args.push(entries.len().to_string());
        for entry in entries {
            args.extend([entry.name.clone(), entry.path.clone()]);
        }
    }
    stage_git_skill_sources(runner, &args).await.map(|_| ())
}

fn default_skill_source_paths() -> Vec<String> {
    vec!["skills".to_string()]
}

#[derive(Debug, Clone)]
struct SkillBundle {
    source: Option<PathBuf>,
    staging_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SkillBundle {
    fn new(source: Option<PathBuf>) -> Self {
        Self { source, staging_lock: Arc::new(tokio::sync::Mutex::new(())) }
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

    async fn selected_sources(&self, selected: &[flotilla_resources::SkillCatalogEntry]) -> Result<Vec<SkillSource>, String> {
        let path =
            self.source.clone().ok_or_else(|| format!("contained agent requires skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))?;
        let inspection = tokio::task::spawn_blocking(move || inspect_skill_sources(&path))
            .await
            .map_err(|error| format!("inspect skill sources task failed: {error}"))??;
        resolve_frozen_sources(&inspection.sources, selected)
    }

    /// Stage `tree` at `destination` through `runner`. Removes the token files once
    /// staging succeeds; the caller removes them after a failure.
    async fn stage(
        &self,
        tree: &SkillTree,
        destination: &Path,
        source_token_files: &BTreeMap<String, PathBuf>,
        runner: &dyn CommandRunner,
    ) -> Result<(), String> {
        let config_base = runner
            .writable_config_base(None, Path::new(CONTAINED_WRITABLE_CONFIG_BASE))
            .await
            .map_err(|error| format!("resolve contained agent skill base: {error}"))?;
        let manifest = serde_json::json!({"schema_version": 5, "sources": tree.sources});
        let mut args = vec![
            "flotilla-stage-skills".to_string(),
            format!("json:{manifest}"),
            destination.to_string_lossy().into_owned(),
            "true".to_string(),
            config_base.join("skill-source-cache").to_string_lossy().into_owned(),
        ];
        for source in &tree.sources {
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
            let entries =
                tree.selected.iter().filter(|entry| entry.source == source.name && entry.revision == source.revision).collect::<Vec<_>>();
            args.push(entries.len().to_string());
            for entry in entries {
                args.extend([entry.name.clone(), entry.path.clone()]);
            }
        }
        for attempt in 1..=3 {
            // Each attempt owns the cache and destination swap, but backoff must
            // leave the lock free for other crews. Cancellation releases the lock.
            let result = {
                let _staging_guard = self.staging_lock.lock().await;
                stage_git_skill_sources(runner, &args).await
            };
            match result {
                Ok(_) => break,
                Err(error) if error.lines().any(|line| line == STAGE_RETRYABLE_PREFIX) && attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(attempt)).await;
                }
                Err(error) => return Err(skill_stage_error(&destination.to_string_lossy(), &error)),
            }
        }
        info!(destination = %destination.display(), selected = ?tree.selected, "staged generation-pinned agent skills");
        Ok(())
    }
}

fn skill_stage_error(environment_ref: &str, stderr: &str) -> String {
    // A git child can write to the shared stderr pipe after the script reports
    // its failure, so prefer the script's marked diagnostic regardless of order.
    let reason = stderr
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(STAGE_DIAGNOSTIC_PREFIX))
        .or_else(|| stderr.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("skill staging command failed");
    let details = stderr.lines().filter(|line| *line != STAGE_RETRYABLE_PREFIX).collect::<Vec<_>>().join("\n");
    let details = details.trim();
    if details.is_empty() {
        format!("stage generation-pinned skills for {environment_ref}: {reason}")
    } else {
        format!("stage generation-pinned skills for {environment_ref}: {reason}\n{details}")
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
/// receives is resolved at admission from CrewDefaults, Project, and dispatch
/// declarations, and checked against the candidate catalog before a roll.
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
                || (source.repository.contains("://")
                    && Url::parse(&source.repository).map_or(true, |url| {
                        url.password().is_some() || (matches!(url.scheme(), "http" | "https") && !url.username().is_empty())
                    }))
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

    fn is_managed_config_home(&self, config_home: &Path, config_base: &Path) -> bool {
        config_home.starts_with(Path::new(CONTAINER_CODEX_HOME)) || config_home.starts_with(config_base.join("agent-homes"))
    }

    fn host_home_name(&self) -> &'static str {
        "codex"
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

    fn host_home_name(&self) -> &'static str {
        "claude"
    }

    fn requester_bound(&self) -> bool {
        true
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
pub(crate) mod tests {
    use std::{io, os::unix::fs::PermissionsExt, path::Path, process::Command, sync::Mutex};

    use flotilla_discovery_testkit::TestEnvVars;
    use flotilla_provider_runtime::CommandOutput;

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
    // Boundary double: records process argv and stdin separately.
    struct RecordingRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        inputs: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl CommandRunner for RecordingRunner {
        async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            self.inputs.lock().expect("recording input lock").push(input.to_vec());
            self.run(cmd, args, cwd, label).await
        }

        async fn run(&self, cmd: &str, args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            self.calls
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
        ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
            self.run(cmd, args, cwd, label).await.map(|stdout| flotilla_provider_runtime::CommandOutput {
                stdout,
                stderr: String::new(),
                exit_code: Some(0),
            })
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[derive(Clone)]
    pub(crate) struct PromisorRunner {
        pub(crate) config_base: PathBuf,
        skills_source: PathBuf,
        path: String,
    }

    #[async_trait]
    impl CommandRunner for PromisorRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            self.run_with_input(cmd, args, cwd, label, &[]).await
        }

        async fn run_with_input(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel, input: &[u8]) -> Result<String, String> {
            let container_manifest = format!("{CONTAINER_SKILLS_SOURCE}/{SKILL_BUNDLE_MANIFEST}");
            let source_manifest = self.skills_source.join(SKILL_BUNDLE_MANIFEST).to_string_lossy().into_owned();
            let args = args
                .iter()
                .map(|arg| {
                    if *arg == container_manifest {
                        source_manifest.clone()
                    } else if let Ok(relative) = Path::new(arg).strip_prefix(CONTAINER_CODEX_HOME) {
                        self.config_base.join("codex").join(relative).display().to_string()
                    } else {
                        (*arg).to_string()
                    }
                })
                .collect::<Vec<_>>();
            let mut env_args = vec![
                format!("PATH={}", self.path),
                format!("FLOTILLA_TEST_FETCH_LOG={}", self.config_base.parent().expect("config parent").join("fetches").display()),
                cmd.to_string(),
            ];
            env_args.extend(args);
            let refs = env_args.iter().map(String::as_str).collect::<Vec<_>>();
            flotilla_provider_runtime::ProcessCommandRunner.run_with_input("env", &refs, cwd, label, input).await
        }

        async fn run_output(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<CommandOutput, String> {
            match self.run(cmd, args, cwd, label).await {
                Ok(stdout) => Ok(CommandOutput { stdout, stderr: String::new(), exit_code: Some(0) }),
                Err(stderr) => Ok(CommandOutput { stdout: String::new(), stderr, exit_code: Some(1) }),
            }
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }

        async fn writable_config_base(&self, _preferred: Option<&Path>, _fallback: &Path) -> Result<PathBuf, String> {
            Ok(self.config_base.clone())
        }

        async fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
            flotilla_provider_runtime::ProcessCommandRunner.write_file(path, content).await
        }

        async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
            flotilla_provider_runtime::ProcessCommandRunner.write_file_with_mode(path, content, mode).await
        }
    }

    pub(crate) fn promisor_runner(root: &Path) -> PromisorRunner {
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
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.init-secret" ]; then
      cat "$FLOTILLA_TEST_FETCH_LOG.init-secret" >&2
      exit 1
    fi
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
    printf '%s\n' fetch >>"$FLOTILLA_TEST_FETCH_LOG"
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.drop-token" ]; then
      rm -f -- "$(cat "$FLOTILLA_TEST_FETCH_LOG.drop-token")"
      helper=$(cat "$checkout/.git/credential-helper")
      credentials=$(sh -c "${helper#\!} get")
      case "$credentials" in
        *'password=test-token'*) ;;
        *) echo 'anonymous fetch attempted' >&2; exit 128;;
      esac
    fi
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.capture-token" ]; then
      helper=$(cat "$checkout/.git/credential-helper")
      credentials=$(sh -c "${helper#\!} get")
      case "$credentials" in
        *'password='*) ;;
        *) echo 'credential helper did not return a password' >&2; exit 128;;
      esac
      token=${credentials##*password=}
      [ -n "$token" ] || { echo 'anonymous fetch attempted' >&2; exit 128; }
      printf '%s\n' "$token" >>"$FLOTILLA_TEST_FETCH_LOG.tokens"
    fi
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.transient" ]; then
      rm "$FLOTILLA_TEST_FETCH_LOG.transient"
      echo 'fatal: TLS connection reset by peer' >&2
      exit 128
    fi
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.fail" ]; then
      printf 'fatal: credential %s rejected by remote\n' "$(cat "$GITHUB_TOKEN_FILE")" >&2
      exit 75
    fi
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.auth" ]; then
      printf 'fatal: HTTP/2 %s credential %s rejected by remote\n' "$(cat "$FLOTILLA_TEST_FETCH_LOG.auth")" "$(cat "$GITHUB_TOKEN_FILE")" >&2
      exit 128
    fi
    eval "revision=\${$#}"
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.wrong-revision" ]; then
      revision=2222222222222222222222222222222222222222
    fi
    printf '%s' "$revision" >"$checkout/.git/FETCH_HEAD"
    ;;
  rev-parse)
    cat "$checkout/.git/FETCH_HEAD"
    ;;
  checkout)
    if [ -f "$FLOTILLA_TEST_FETCH_LOG.checkout-auth" ]; then
      echo "fatal: could not read Username for 'https://github.com': terminal prompts disabled" >&2
      exit 128
    fi
    if [ ! -s "$checkout/.git/credential-helper" ] || [ -z "${GITHUB_TOKEN_FILE:-}" ] || [ ! -s "$GITHUB_TOKEN_FILE" ]; then
      echo "fatal: could not read Username for 'https://github.com': terminal prompts disabled" >&2
      echo "fatal: could not fetch promised blob from promisor remote" >&2
      exit 1
    fi
    mkdir -p "$checkout/skills/private-folder"
    printf '%s\n' '---' 'name: private-source' 'description: test skill' '---' '# Private source' >"$checkout/skills/private-folder/SKILL.md"
    mkdir -p "$checkout/skills/unselected"
    printf '%s\n' '---' 'name: unselected' 'description: test skill' '---' >"$checkout/skills/unselected/SKILL.md"
    mkdir -p "$checkout/plugins/private-folder"
    printf '%s\n' '---' 'name: private-source' 'description: test skill' '---' '# Private source' >"$checkout/plugins/private-folder/SKILL.md"
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

    fn test_registry(env: Arc<dyn EnvVars>) -> AgentMaterialRegistry {
        AgentMaterialRegistry::with_codex_delivery_support(env, true)
    }

    fn registry(home: &Path) -> AgentMaterialRegistry {
        let skills = write_skill_sources(home);
        test_registry(Arc::new(TestEnvVars::new([
            ("HOME", home.to_string_lossy().into_owned()),
            (FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned()),
        ])))
    }

    // Shared transport requires an owned source, the exact guest home, and Rw.
    // Exhaust those three booleans, empty/duplicate mounts, and invalid identities.
    #[test]
    fn shared_codex_transport_requires_the_registrys_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let registry = registry(root.path());
        for owned in [false, true] {
            for exact in [false, true] {
                for writable in [false, true] {
                    let source = if owned { registry.homes_dir.join("work/codex") } else { root.path().join("user-home") };
                    let target = if exact { CONTAINER_CODEX_HOME } else { "/different/home" };
                    let mode = if writable { ProvisionedMountMode::Rw } else { ProvisionedMountMode::Ro };
                    let mount = ProvisionedMount::new(source, target, mode);
                    assert_eq!(registry.codex_home_is_shared("work", &[mount.clone(), mount]), owned && exact && writable);
                }
            }
        }
        assert!(!registry.codex_home_is_shared("work", &[]));
        for invalid in ["", "/", "..", "work/../other"] {
            let mount = ProvisionedMount::new(
                registry.homes_dir.join(invalid).join(CODEX_ADAPTER_ID),
                CONTAINER_CODEX_HOME,
                ProvisionedMountMode::Rw,
            );
            assert!(!registry.codex_home_is_shared(invalid, &[mount]));
        }
    }

    fn registry_with_home_template(home: &Path, template: &Path) -> AgentMaterialRegistry {
        let skills = write_skill_sources(home);
        test_registry(Arc::new(TestEnvVars::new([
            ("HOME", home.to_string_lossy().into_owned()),
            (FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned()),
            (FLOTILLA_CODEX_HOME_TEMPLATE_ENV, template.to_string_lossy().into_owned()),
        ])))
    }

    fn delivered_auth_mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("delivered credential metadata").permissions().mode() & 0o777
    }

    // The injected capability controls delivery on every test host. Exhaust both
    // boolean values and both managed/external home cases; concurrency is irrelevant
    // to this immutable policy input.
    #[tokio::test]
    async fn codex_delivery_obeys_injected_support() {
        for supported in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            write_central_auth(temp.path(), "test-access-token");
            let skills = write_skill_sources(temp.path());
            let env = Arc::new(TestEnvVars::new([
                ("HOME", temp.path().display().to_string()),
                (FLOTILLA_SKILLS_DIR_ENV, skills.display().to_string()),
            ]));
            let registry = AgentMaterialRegistry::with_codex_delivery_support(env, supported);
            let required = BTreeSet::from([CODEX_ADAPTER_ID.to_string()]);
            let result = registry.prepare("crew", &required, &BTreeMap::new()).await;
            if supported {
                let deliveries = result.expect("injected Linux support permits delivery");
                assert_eq!(deliveries.len(), 2);
                assert_eq!(
                    std::fs::read(deliveries[0].mount.host_path.as_path().join(CODEX_AUTH_FILE)).expect("delivered auth"),
                    std::fs::read(registry.codex_credential_source()).expect("central auth"),
                );
            } else {
                assert_eq!(
                    result.expect_err("unsupported host refuses delivery"),
                    "Codex login material delivery is supported only on Linux placement hosts"
                );
                assert!(!temp.path().join(".local/share/flotilla/agent-homes").exists());
            }
            let external = BTreeMap::from([("CODEX_HOME".to_string(), "/image/codex".to_string())]);
            assert!(registry.prepare("external", &required, &external).await.expect("external home bypasses delivery").is_empty());
        }
    }

    // The production constructor retains Linux-only support even when test-support
    // is enabled. A missing credential distinguishes support from platform refusal.
    #[tokio::test]
    async fn codex_production_support_matches_the_placement_host() {
        let temp = tempfile::tempdir().expect("tempdir");
        let env = Arc::new(TestEnvVars::new([("HOME", temp.path().display().to_string())]));
        let registry = AgentMaterialRegistry::new(env);
        let error = registry
            .prepare("crew", &BTreeSet::from([CODEX_ADAPTER_ID.to_string()]), &BTreeMap::new())
            .await
            .expect_err("no central credential");
        if cfg!(target_os = "linux") {
            assert!(error.contains("is not provisioned on this host"), "{error}");
        } else {
            assert_eq!(error, "Codex login material delivery is supported only on Linux placement hosts");
        }
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

    // Glue: a literal dash pathname must reach the shell as a pathname rather
    // than its stdin sentinel; normalization must not change other arguments.
    #[tokio::test]
    async fn skill_manifest_dash_path_is_not_stdin() {
        let runner = RecordingRunner::default();
        let args = ["flotilla-stage-skills", "-", "/skills", "false", "/cache"].map(str::to_string);
        stage_git_skill_sources(&runner, &args).await.expect("stage pathname");
        let calls = runner.calls.lock().expect("recorded calls");
        assert_eq!(&calls[0].1[3..], &["flotilla-stage-skills", "./-", "/skills", "false", "/cache"]);
        assert!(runner.inputs.lock().expect("recorded input").is_empty());
    }

    /// The host behind an agentless SSH runner, recording each command; its
    /// writable config base is `/host/state/flotilla`.
    impl AgentMaterialRegistry {
        /// Stage each manifest source's `private-folder` into the `skills` directory of
        /// every required harness's home `environment` names, through the access layer's
        /// staging entry point.
        pub(crate) async fn stage_test_skills(
            &self,
            _crew: &str,
            required_adapters: &BTreeSet<String>,
            environment: &[(String, String)],
            source_token_files: &BTreeMap<String, PathBuf>,
            runner: &dyn CommandRunner,
        ) -> Result<(), String> {
            let inspection = inspect_skill_sources(self.skills.source.as_ref().expect("test source"))?;
            let selected = inspection
                .sources
                .iter()
                .map(|source| flotilla_resources::SkillCatalogEntry {
                    source: source.name.clone(),
                    repository: source.repository.clone(),
                    revision: source.revision.clone(),
                    name: "private-source".to_string(),
                    path: format!("{}/private-folder", source.paths[0]),
                })
                .collect::<Vec<_>>();
            let tree = self.skill_tree(&selected).await?;
            for adapter in required_adapters {
                let variable = self.harness_home(adapter).expect("managed harness").variable;
                if let Some((_, home)) = environment.iter().find(|(name, _)| name == variable) {
                    self.stage_skill_tree(&tree, &Path::new(home).join("skills"), source_token_files, runner).await?;
                }
            }
            Ok(())
        }
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
            .stage_test_skills(
                "crew-alice",
                &required,
                &environment,
                &BTreeMap::from([("mattpocock-skills".to_string(), PathBuf::from("/tmp/skills-token"))]),
                &runner,
            )
            .await
            .expect("stage pinned skills");

        let calls = runner.calls.lock().expect("recording runner lock should be healthy");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sh");
        assert_eq!(calls[0].1[4], "-", "inline manifest is consumed through stdin");
        assert!(!calls[0].1.iter().any(|arg| arg.starts_with("json:")));
        let inputs = runner.inputs.lock().expect("recorded stdin");
        let manifest: SkillBundleManifest = serde_json::from_slice(&inputs[0]).expect("manifest");
        assert_eq!(manifest.sources[0].revision, "1".repeat(40));
        assert!(calls[0].1.contains(&"/tmp/flotilla-config/credentials/claude-max/claude/skills".to_string()));
        assert!(calls[0].1.contains(&"https://github.com/flotilla-org/mattpocock-skills.git".to_string()));
        assert!(calls[0].1.contains(&"1111111111111111111111111111111111111111".to_string()));
        assert!(calls[0].1.contains(&"plugins/rjw-sdlc/skills".to_string()));
        assert!(calls[0].1.contains(&"/tmp/skills-token".to_string()));
        assert!(calls[0].1[1].contains("fetch --quiet --depth=1 --filter=blob:none --no-tags"));
        assert!(calls[0].1[1].contains("credential.helper="), "public fetches must clear the project credential helper");
        assert!(calls[0].1[1].contains("pinned revision does not exist"));
        assert!(calls[0].1[1].contains("init --quiet"));
        assert!(calls[0].1[1].contains("2>\"$sources/fetch.stderr\""), "git failures must retain their stderr diagnostics");
        assert!(calls[0].1[1].contains("sparse-checkout set --no-cone --stdin"));
        assert!(calls[0].1[1].contains("skill source $name declared path $path is missing at pinned revision $revision"));
        assert!(calls[0].1[1].contains("skill source $name declared path $path has no SKILL.md at pinned revision $revision"));
        assert!(!calls[0].1[1].contains("required"), "staging must carry no skill-name policy; demand validation is #1790's contract");
    }

    #[test]
    fn skill_stage_error_leads_with_declared_path_failure() {
        let stderr = "hint: Using 'master' as the name for the initial branch.\nFrom https://github.com/flotilla-org/cleat\n * branch 0f23944 -> FETCH_HEAD\nflotilla-stage-skills: skill source cleat declared path skills is missing at pinned revision 0f23944\n";
        let message = skill_stage_error("crew-work", stderr);
        assert!(message.starts_with(
            "stage generation-pinned skills for crew-work: skill source cleat declared path skills is missing at pinned revision 0f23944"
        ));
        assert!(message.contains("From https://github.com/flotilla-org/cleat"));
    }

    #[test]
    fn skill_stage_error_prefers_script_diagnostic_over_late_git_stderr() {
        let stderr = "fatal: early git failure\nflotilla-stage-skills: skill source missing-source anonymous fetch failed at pinned revision 0000000000000000000000000000000000000000\nfatal: git upload-pack: not our ref 0000000000000000000000000000000000000000\n";
        let message = skill_stage_error("crew-fetch", stderr);
        assert!(message.starts_with("stage generation-pinned skills for crew-fetch: skill source missing-source anonymous fetch failed"));
        assert!(message.contains("fatal: git upload-pack: not our ref"));
    }

    #[test]
    fn skill_stage_error_uses_last_nonempty_line_without_script_diagnostic() {
        let message = skill_stage_error("crew-fetch", "fatal: early git failure\nfatal: final git failure\n");
        assert_eq!(
            message,
            "stage generation-pinned skills for crew-fetch: fatal: final git failure\nfatal: early git failure\nfatal: final git failure"
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
            let registry = test_registry(Arc::new(TestEnvVars::new([(FLOTILLA_SKILLS_DIR_ENV, skills.to_string_lossy().into_owned())])));
            let environment = vec![("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())];
            let error = registry
                .stage_test_skills("crew-fetch", &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]), &environment, &tokens, &runner)
                .await
                .expect_err("fetch of an absent revision must fail");
            assert!(!error.is_empty(), "fetch failure must produce a diagnostic");
            assert!(error.contains("missing-source"), "diagnostic must name the source: {error}");
            assert!(error.contains(revision), "diagnostic must name the pinned revision: {error}");
            assert!(error.contains("pinned revision does not exist"), "absent revision must be terminal: {error}");
            assert!(error.contains("command: git -C"), "diagnostic must name the Git command: {error}");
            assert!(error.contains("exit code: 128"), "diagnostic must include Git's exit code: {error}");
            assert!(error.contains("fatal:"), "diagnostic must retain Git stderr: {error}");
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
            .stage_test_skills(
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

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_staging_reuses_one_pinned_fetch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(registry(temp.path()));
        let runner = Arc::new(promisor_runner(temp.path()));
        let required = BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]);
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("write manifest");
        let results = futures::future::join_all((0..4).map(|index| {
            let destination = runner.config_base.join(format!("claude-{index}"));
            let required = required.clone();
            let registry = Arc::clone(&registry);
            let runner = Arc::clone(&runner);
            let token_file = temp.path().join(format!("source-{index}.token"));
            std::fs::write(&token_file, "test-token").expect("write token");
            let tokens = BTreeMap::from([("private-skills".to_string(), token_file)]);
            tokio::spawn(async move {
                registry
                    .stage_test_skills(
                        &format!("crew-{index}"),
                        &required,
                        &[("CLAUDE_CONFIG_DIR".to_string(), destination.to_string_lossy().into_owned())],
                        &tokens,
                        &*runner,
                    )
                    .await
            })
        }))
        .await;
        assert!(results.iter().all(|result| result.as_ref().is_ok_and(Result::is_ok)), "concurrent staging failed: {results:?}");
        let fetches = std::fs::read_to_string(temp.path().join("fetches")).expect("fake fetch log");
        assert_eq!(fetches.lines().count(), 1, "one fetch for the pinned source");
        for index in 0..4 {
            assert!(runner.config_base.join(format!("claude-{index}/skills/private-source/SKILL.md")).is_file());
        }
        let error = registry
            .stage_test_skills(
                "crew-without-private-credential",
                &required,
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("unauthorized").to_string_lossy().into_owned())],
                &BTreeMap::new(),
                &*runner,
            )
            .await
            .expect_err("private cache must still require a credential");
        assert!(error.contains("credential private-skills is unavailable"), "unexpected error: {error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_dispatches_recover_from_one_git_128_fetch_failure_without_losing_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(registry(temp.path()));
        let runner = Arc::new(promisor_runner(temp.path()));
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"https://github.com/example/mattpocock-skills.git","revision":"1111111111111111111111111111111111111111","credential":"github-skills-fork"}]}"#,
        )
        .expect("manifest");
        std::fs::write(temp.path().join("fetches.transient"), "").expect("one failing fetch");
        let results = futures::future::join_all((0..6).map(|index| {
            let registry = Arc::clone(&registry);
            let runner = Arc::clone(&runner);
            let token_file = temp.path().join(format!("source-{index}.token"));
            std::fs::write(&token_file, format!("token-{index}\n")).expect("unique minted token");
            tokio::spawn(async move {
                let result = registry
                    .stage_test_skills(
                        &format!("crew-{index}"),
                        &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                        &[(
                            "CLAUDE_CONFIG_DIR".to_string(),
                            runner.config_base.join(format!("claude-{index}")).to_string_lossy().into_owned(),
                        )],
                        &BTreeMap::from([("mattpocock-skills".to_string(), token_file.clone())]),
                        &*runner,
                    )
                    .await;
                (result, token_file)
            })
        }))
        .await;
        for (index, task) in results.into_iter().enumerate() {
            let (result, token_file) = task.expect("staging task");
            result.expect("each concurrent crew must stage");
            assert!(!token_file.exists(), "crew {index} must clean its own token after staging");
            assert!(runner.config_base.join(format!("claude-{index}/skills/private-source/SKILL.md")).is_file());
        }
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 2);
    }

    #[tokio::test]
    async fn credentialed_fetch_survives_original_token_file_disappearing_before_git_reads_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let runner = promisor_runner(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let token_file = temp.path().join("one-shot.token");
        std::fs::write(&token_file, "test-token").expect("token");
        std::fs::write(temp.path().join("fetches.drop-token"), token_file.to_string_lossy().as_bytes()).expect("drop marker");
        registry
            .stage_test_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file)]),
                &runner,
            )
            .await
            .expect("staging keeps its own credential copy through fetch");
        let empty_token = temp.path().join("empty.token");
        std::fs::write(&empty_token, "").expect("empty token fixture");
        let error = registry
            .stage_test_skills(
                "crew-with-empty-token",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("empty").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), empty_token)]),
                &runner,
            )
            .await
            .expect_err("empty token must stop staging before fetch");
        assert!(error.contains("credential private-skills is unavailable"), "unexpected error: {error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 1);
    }

    #[tokio::test]
    async fn transient_skill_fetch_retries_and_failure_redacts_credential() {
        for should_recover in [true, false] {
            let temp = tempfile::tempdir().expect("tempdir");
            let registry = registry(temp.path());
            let runner = promisor_runner(temp.path());
            let skills = registry.skills.source.as_ref().expect("generation source");
            std::fs::write(
                skills.join(SKILL_BUNDLE_MANIFEST),
                r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
            )
            .expect("write manifest");
            let token_file = temp.path().join("source.token");
            std::fs::write(&token_file, "secret-for-redaction").expect("write token");
            let marker = if should_recover { "fetches.transient" } else { "fetches.fail" };
            std::fs::write(temp.path().join(marker), "").expect("write fake fetch marker");
            let result = registry
                .stage_test_skills(
                    "crew-private",
                    &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                    &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                    &BTreeMap::from([("private-skills".to_string(), token_file)]),
                    &runner,
                )
                .await;
            let fetches = std::fs::read_to_string(temp.path().join("fetches")).expect("fake fetch log");
            if should_recover {
                result.expect("second fetch should recover");
                assert_eq!(fetches.lines().count(), 2);
            } else {
                let error = result.expect_err("persistent fetch failure should surface");
                assert_eq!(fetches.lines().count(), 3, "retry ceiling must be bounded");
                assert!(error.contains("command: git -C"), "missing Git command: {error}");
                assert!(error.contains("exit code: 75"), "missing Git exit code: {error}");
                assert!(error.contains("fatal: credential [redacted credential] rejected by remote"), "missing redacted stderr: {error}");
                assert!(!error.contains("secret-for-redaction"), "credential leaked into status: {error}");
            }
        }
    }

    #[tokio::test]
    async fn skill_fetch_auth_failure_is_terminal_and_redacts_newline_terminated_token() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let runner = promisor_runner(temp.path());
        std::fs::write(temp.path().join("fetches.auth"), "401").expect("auth marker");
        let token_file = temp.path().join("source.token");
        std::fs::write(&token_file, "secret-for-redaction\n").expect("token");
        let error = registry
            .stage_test_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file.clone())]),
                &runner,
            )
            .await
            .expect_err("401 must terminate staging");
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 1);
        assert!(error.contains("authentication or authorization failed"), "{error}");
        assert!(error.contains("fatal: HTTP/2 401 credential [redacted credential] rejected by remote"), "{error}");
        assert!(!error.contains("secret-for-redaction"), "{error}");
        assert!(!token_file.exists(), "failed staging cleans the token");

        std::fs::write(temp.path().join("fetches.auth"), "403").expect("authorization marker");
        std::fs::write(&token_file, "secret-for-redaction\n").expect("second token");
        let error = registry
            .stage_test_skills(
                "crew-private-403",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude-403").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file)]),
                &runner,
            )
            .await
            .expect_err("403 must terminate staging");
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 2);
        assert!(error.contains("fatal: HTTP/2 403 credential [redacted credential] rejected by remote"), "{error}");
    }

    #[tokio::test]
    async fn skill_checkout_prompt_failure_is_terminal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let runner = promisor_runner(temp.path());
        std::fs::write(temp.path().join("fetches.checkout-auth"), "").expect("checkout auth marker");
        let token_file = temp.path().join("source.token");
        std::fs::write(&token_file, "test-token").expect("token");
        let error = registry
            .stage_test_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file)]),
                &runner,
            )
            .await
            .expect_err("disabled credential prompt is terminal");
        assert!(error.contains("authentication or authorization failed"), "{error}");
        assert!(error.contains("could not read Username"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 1);
    }

    #[tokio::test]
    async fn skill_stage_redacts_early_git_stderr() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let runner = promisor_runner(temp.path());
        std::fs::write(temp.path().join("fetches.init-secret"), "fatal: secret\\for-redaction\n").expect("init marker");
        let token_file = temp.path().join("source.token");
        std::fs::write(&token_file, "secret\\for-redaction\n").expect("token");
        let error = registry
            .stage_test_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file)]),
                &runner,
            )
            .await
            .expect_err("init failure must surface");
        assert!(error.contains("fatal: [redacted credential]"), "{error}");
        assert!(!error.contains("secret\\for-redaction"), "{error}");
    }

    #[tokio::test]
    async fn skill_cache_rebuilds_when_source_changes_or_ready_marker_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        let manifest = skills.join(SKILL_BUNDLE_MANIFEST);
        let runner = promisor_runner(temp.path());
        let cache = runner.config_base.join("skill-source-cache/private-skills-1111111111111111111111111111111111111111");
        let cases = [
            ("https://github.com/example/one.git", "skills", 1),
            ("https://github.com/example/two.git", "skills", 2),
            ("https://github.com/example/two.git", "plugins", 3),
            ("https://github.com/example/two.git", "plugins", 4),
        ];
        for (index, (repository, path, expected_fetches)) in cases.into_iter().enumerate() {
            std::fs::write(
                &manifest,
                format!(
                    r#"{{"schema_version":5,"sources":[{{"name":"private-skills","repository":"{repository}","revision":"1111111111111111111111111111111111111111","credential":"private-skills","paths":["{path}"]}}]}}"#
                ),
            )
            .expect("manifest");
            if index == 3 {
                std::fs::remove_file(cache.join(".flotilla-ready")).expect("remove ready marker");
            }
            let token_file = temp.path().join(format!("source-{index}.token"));
            std::fs::write(&token_file, "test-token").expect("token");
            registry
                .stage_test_skills(
                    &format!("crew-{index}"),
                    &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                    &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join(format!("claude-{index}")).to_string_lossy().into_owned())],
                    &BTreeMap::from([("private-skills".to_string(), token_file)]),
                    &runner,
                )
                .await
                .expect("stage source");
            assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), expected_fetches);
            assert!(cache.join(".flotilla-ready").is_file());
        }
    }

    #[tokio::test]
    async fn skill_stage_rejects_a_fetch_of_the_wrong_revision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let runner = promisor_runner(temp.path());
        std::fs::write(temp.path().join("fetches.wrong-revision"), "").expect("wrong revision marker");
        let token_file = temp.path().join("source.token");
        std::fs::write(&token_file, "test-token").expect("token");
        let error = registry
            .stage_test_skills(
                "crew-private",
                &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join("claude").to_string_lossy().into_owned())],
                &BTreeMap::from([("private-skills".to_string(), token_file)]),
                &runner,
            )
            .await
            .expect_err("wrong revision must fail");
        assert!(error.contains("fetch returned the wrong pinned revision 1111111111111111111111111111111111111111"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transient_backoff_does_not_hold_the_staging_mutex() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = Arc::new(registry(temp.path()));
        let skills = registry.skills.source.as_ref().expect("generation source");
        std::fs::write(
            skills.join(SKILL_BUNDLE_MANIFEST),
            r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#,
        )
        .expect("manifest");
        let runner = Arc::new(promisor_runner(temp.path()));
        std::fs::write(temp.path().join("fetches.transient"), "").expect("transient marker");
        let stage = |name: &'static str, registry: Arc<AgentMaterialRegistry>, runner: Arc<PromisorRunner>, root: PathBuf| async move {
            let token_file = root.join(format!("{name}.token"));
            std::fs::write(&token_file, "test-token").expect("token");
            registry
                .stage_test_skills(
                    name,
                    &BTreeSet::from([CLAUDE_CODE_ADAPTER_ID.to_string()]),
                    &[("CLAUDE_CONFIG_DIR".to_string(), runner.config_base.join(name).to_string_lossy().into_owned())],
                    &BTreeMap::from([("private-skills".to_string(), token_file)]),
                    &*runner,
                )
                .await
        };
        let first = tokio::spawn(stage("crew-first", Arc::clone(&registry), Arc::clone(&runner), temp.path().to_path_buf()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !temp.path().join("fetches").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first fetch started");
        let guard = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(guard) = registry.skills.staging_lock.try_lock() {
                    return guard;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("staging lock should be available during backoff");
        assert!(!first.is_finished(), "the first crew must still be backing off when the lock becomes available");
        drop(guard);
        stage("crew-second", Arc::clone(&registry), Arc::clone(&runner), temp.path().to_path_buf())
            .await
            .expect("second crew stages while first backs off");
        first.await.expect("first task").expect("first staging");
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
    fn skill_manifest_accepts_ssh_usernames_but_rejects_http_userinfo_and_passwords() {
        let bundle = tempfile::tempdir().expect("tempdir");
        let skills = write_skill_sources(bundle.path());
        let path = skills.join(SKILL_BUNDLE_MANIFEST);
        let original = std::fs::read_to_string(&path).expect("manifest");
        let repository = "https://github.com/rjwittams/rjw-skills.git";
        std::fs::write(&path, original.replace(repository, "ssh://git@github.com/rjwittams/rjw-skills.git")).expect("SSH manifest");
        inspect_skill_sources(&skills).expect("ordinary SSH username is allowed");
        for invalid in [
            "https://user:pass@github.com/rjwittams/rjw-skills.git",
            "https://user@github.com/rjwittams/rjw-skills.git",
            "ssh://git:pass@github.com/rjwittams/rjw-skills.git",
        ] {
            std::fs::write(&path, original.replace(repository, invalid)).expect("invalid manifest");
            inspect_skill_sources(&skills).expect_err("embedded secret or HTTP userinfo is rejected");
        }
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

    // A central rotation is Pushed through the selected runner with final
    // read-only permissions. Unchanged staging is re-sent after an interrupted
    // push, so the guest converges without sharing the host credential inode.
    #[tokio::test]
    async fn codex_rotation_uses_the_agent_runner_and_retries_unchanged_staging() {
        use std::sync::Mutex;
        struct DeliveryRunner(Mutex<Vec<(PathBuf, String, u32)>>);
        #[async_trait]
        impl CommandRunner for DeliveryRunner {
            async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
                panic!("credential bytes must use the files seam")
            }
            async fn run_output(
                &self,
                _: &str,
                _: &[&str],
                _: &Path,
                _: &ChannelLabel,
            ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
                unreachable!()
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
            async fn write_file_with_mode(&self, path: &Path, bytes: &str, mode: u32) -> Result<(), String> {
                self.0.lock().unwrap().push((path.into(), bytes.into(), mode));
                Ok(())
            }
        }
        let temp = tempfile::tempdir().unwrap();
        write_central_auth(temp.path(), "first");
        let registry = registry(temp.path());
        registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.into()]), &BTreeMap::new()).await.unwrap();
        let runner = DeliveryRunner(Mutex::new(Vec::new()));
        for token in ["first", "rotated", "rotated"] {
            write_central_auth(temp.path(), token);
            registry.refresh_delivered_credentials_through("env-a", &runner).await.unwrap();
            let writes = runner.0.lock().unwrap();
            let (path, bytes, mode) = writes.last().unwrap();
            assert_eq!(path, &Path::new(CONTAINER_CODEX_HOME).join(CODEX_AUTH_FILE));
            assert_eq!(*mode, CODEX_AUTH_MODE);
            assert!(bytes.contains(token));
        }
        assert_eq!(runner.0.lock().unwrap().len(), 3);
        assert!(registry.refresh_delivered_credentials_through("../other-vessel", &runner).await.is_err());
    }

    // Session capture uses the runner stream, excludes credentials, and keeps
    // the previous complete archive when a guest read fails so teardown retries.
    #[tokio::test]
    async fn guest_session_capture_publishes_only_complete_archives() {
        struct ArchiveRunner(bool);
        #[async_trait]
        impl CommandRunner for ArchiveRunner {
            async fn run(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
                unreachable!()
            }
            async fn run_output(
                &self,
                _: &str,
                _: &[&str],
                _: &Path,
                _: &ChannelLabel,
            ) -> Result<flotilla_provider_runtime::CommandOutput, String> {
                unreachable!()
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
            // Fake only the guest process boundary; archive publication is real.
            async fn run_to_file(&self, command: &str, args: &[&str], cwd: &Path, path: &Path) -> Result<(), String> {
                assert_eq!(command, "tar");
                assert_eq!(args, ["-C", CONTAINER_CODEX_HOME, "--exclude=auth.json", "-cf", "-", "."]);
                assert_eq!(cwd, Path::new("/"));
                tokio::fs::write(path, b"session archive").await.unwrap();
                if self.0 {
                    Err("guest disconnected".into())
                } else {
                    Ok(())
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        write_central_auth(temp.path(), "secret");
        let registry = registry(temp.path());
        registry.prepare("env-a", &BTreeSet::from([CODEX_ADAPTER_ID.into()]), &BTreeMap::new()).await.unwrap();
        let home = registry.homes_dir.join("env-a");
        std::fs::write(home.join(format!(".guest-codex-{}.tar", uuid::Uuid::new_v4())), "partial from previous process").unwrap();
        registry.capture_environment_home("env-a", &ArchiveRunner(false)).await.unwrap();
        assert_eq!(std::fs::read(home.join("guest-codex.tar")).unwrap(), b"session archive");
        assert!(registry.capture_environment_home("env-a", &ArchiveRunner(true)).await.is_err());
        assert_eq!(std::fs::read(home.join("guest-codex.tar")).unwrap(), b"session archive");
        assert!(!std::fs::read_dir(&home).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".guest-")));
        assert!(registry.capture_environment_home("../other", &ArchiveRunner(false)).await.is_err());
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
    async fn environment_home_archival_preserves_sessions_and_collisions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let environment_home = temp.path().join(".local/share/flotilla/agent-homes/env-a");
        let session = environment_home.join("codex/sessions/2026/09/16/rollout.jsonl");
        std::fs::create_dir_all(session.parent().expect("session has parent")).expect("session directory");
        std::fs::write(&session, "{\"type\":\"session_meta\"}\n").expect("session state");

        // Delivered login material is stripped while session contents survive.
        std::fs::write(environment_home.join("codex/auth.json"), "credential").unwrap();
        registry.archive_environment_home("convoy-a", "env-a").await.expect("archive environment home");
        assert!(!environment_home.exists(), "teardown must move the persistent agent home");

        // Teardown preserves session contents; repeated environments never overwrite an archive.
        let archive = temp.path().join(".local/share/flotilla/session-archive/convoy-a");
        assert_eq!(
            std::fs::read_to_string(archive.join("env-a/codex/sessions/2026/09/16/rollout.jsonl")).unwrap(),
            "{\"type\":\"session_meta\"}\n"
        );
        std::fs::create_dir_all(&environment_home).unwrap();
        std::fs::write(environment_home.join("second-session"), "second").unwrap();
        registry.archive_environment_home("convoy-a", "env-a").await.unwrap();
        assert_eq!(std::fs::read_dir(&archive).unwrap().count(), 2);
        let suffixed = std::fs::read_dir(&archive)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap() != "env-a")
            .unwrap();
        assert_eq!(std::fs::read_to_string(suffixed.join("second-session")).unwrap(), "second");
        assert!(!archive.join("env-a/codex/auth.json").exists());
        assert_eq!(std::fs::metadata(archive.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        for invalid in ["../escape", "..", "/", ""] {
            assert!(registry.archive_environment_home("convoy-a", invalid).await.is_err());
            assert!(registry.archive_environment_home(invalid, "env-a").await.is_err());
        }
        assert!(archive.join("env-a/codex/sessions/2026/09/16/rollout.jsonl").exists());
        registry.archive_environment_home("convoy-a", "env-a").await.expect("idempotent cleanup");
    }

    #[tokio::test]
    async fn missing_generation_skill_sources_fail_before_agent_container_creation() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = test_registry(Arc::new(TestEnvVars::new([("HOME", temp.path().to_string_lossy().into_owned())])));

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
    // The canary must stage ordinary skills without reading a model login,
    // and its material adapter must be absent from ordinary daemon registries.
    #[tokio::test]
    async fn canary_material_is_opt_in_and_credential_free() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(!registry(temp.path()).adapters.contains_key("fleet-canary"));
        let skills = write_skill_sources(temp.path());
        let registry = test_registry(Arc::new(TestEnvVars::new([
            ("HOME", temp.path().display().to_string()),
            (FLOTILLA_SKILLS_DIR_ENV, skills.display().to_string()),
            ("FLOTILLA_FLEET_CANARY", "1".to_string()),
        ])));
        let deliveries = registry
            .prepare("canary", &BTreeSet::from(["fleet-canary".to_string()]), &BTreeMap::new())
            .await
            .expect("no model login is required");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].mount.environment_path.as_path(), Path::new(CONTAINER_SKILLS_SOURCE));
        assert!(!temp.path().join(".local/share/flotilla/agent-homes").exists());
    }

    // Only the selected frontmatter names land in a crew's skills, even when the pinned
    // source also supplies an unrelated skill.
    #[tokio::test]
    async fn only_frozen_skills_land_in_the_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let bundle = registry.skills.source.as_ref().expect("source");
        std::fs::write(bundle.join(SKILL_BUNDLE_MANIFEST), r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#).expect("manifest");
        let runner = promisor_runner(temp.path());
        let token = temp.path().join("token");
        std::fs::write(&token, "test-token").expect("token");
        let tree = registry.skill_tree(&[crew_skill("private-source", "skills/private-folder")]).await.expect("resolve");
        let destination = runner.config_base.join("agent-homes/requesters/convoy/coder/claude/skills");
        registry
            .stage_skill_tree(&tree, &destination, &BTreeMap::from([("private-skills".to_string(), token.clone())]), &runner)
            .await
            .expect("stage selected");
        let names = std::fs::read_dir(&destination)
            .expect("staged directory")
            .filter_map(|entry| {
                let entry = entry.expect("entry");
                entry.file_type().expect("file type").is_dir().then(|| entry.file_name())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, [std::ffi::OsString::from("private-source")], "only the frozen selection is installed");
        assert!(!token.exists(), "the source token is removed after staging");
    }

    // A source credential is minted narrowed to one repository, so a tree whose sources
    // share a credential across two repositories is refused before anything is delivered.
    #[tokio::test]
    async fn one_credential_is_not_narrowed_to_two_repositories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let bundle = registry.skills.source.as_ref().expect("source");
        let source = |name: &str| serde_json::json!({"name": name, "repository": format!("https://github.com/example/{name}.git"), "revision": "1".repeat(40), "credential": "private-skills"});
        let manifest = serde_json::json!({"schema_version": 5, "sources": [source("private-skills"), source("other-skills")]});
        std::fs::write(bundle.join(SKILL_BUNDLE_MANIFEST), manifest.to_string()).expect("manifest");
        let other = flotilla_resources::SkillCatalogEntry {
            source: "other-skills".into(),
            repository: "example/other-skills".into(),
            ..crew_skill("other-source", "skills/other-folder")
        };
        let error = registry
            .skill_tree(&[crew_skill("private-source", "skills/private-folder"), other])
            .await
            .expect_err("one credential, two repositories");
        assert!(error.contains("cannot be narrowed to multiple source repositories"), "{error}");
        registry
            .skill_tree(&[crew_skill("private-source", "skills/private-folder")])
            .await
            .expect("one repository per credential resolves");
    }

    fn crew_skill(name: &str, path: &str) -> flotilla_resources::SkillCatalogEntry {
        flotilla_resources::SkillCatalogEntry {
            source: "private-skills".into(),
            repository: "example/private-skills".into(),
            revision: "1".repeat(40),
            name: name.into(),
            path: path.into(),
        }
    }

    // Candidate probing preserves the governor's old revision while using the
    // current supply authority. Missing authority/tokens and a remote serving
    // another revision refuse; failed staging must preserve operator token files.
    #[tokio::test]
    async fn pre_roll_skill_probe_checks_authority_pin_and_preserves_operator_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let bundle = registry.skills.source.as_ref().expect("source");
        std::fs::write(
            bundle.join(SKILL_BUNDLE_MANIFEST),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 5,
                "sources": [{
                    "name": "private-skills",
                    "repository": "https://github.com/example/private-skills.git",
                    "revision": "2".repeat(40),
                    "credential": "private-skills",
                }],
            }))
            .expect("manifest JSON"),
        )
        .expect("new supply");
        // Subprocess-boundary stand-in enforces the Git staging CLI, supplies
        // lazy blobs and records the exact revision fetched without networking.
        let runner = promisor_runner(temp.path());
        let selected = vec![crew_skill("private-source", "skills/private-folder")];
        let spec = serde_json::from_value(serde_json::json!({
            "consumer": {"adapter": "github-app", "installation_id": 1},
            "source": {
                "kind": "github-app",
                "app_id_path": "/test/app-id",
                "private_key_path": "/test/key",
            },
            "lifecycle": "refreshable",
        }))
        .expect("credential spec");
        let declared = BTreeMap::from([("private-skills".into(), spec)]);
        let missing = validate_frozen_skills(bundle, &selected, &BTreeMap::new(), &BTreeMap::new(), &runner)
            .await
            .expect_err("undeclared credential");
        assert!(missing.contains("no declared CredentialSpec"));
        let missing = validate_frozen_skills(bundle, &selected, &declared, &BTreeMap::new(), &runner).await.expect_err("token required");
        assert!(missing.contains("explicit probe token"));
        let token = temp.path().join("operator-token");
        std::fs::write(&token, "test-token").expect("token");
        let tokens = BTreeMap::from([("private-skills".into(), token.clone())]);
        let mut invalid = declared.clone();
        invalid.get_mut("private-skills").expect("spec").lifecycle = flotilla_resources::CredentialLifecycle::Static;
        assert!(validate_frozen_skills(bundle, &selected, &invalid, &tokens, &runner)
            .await
            .expect_err("candidate cannot mint static App credentials")
            .contains("refreshable"));
        let crews = BTreeMap::from([("coder".into(), selected.clone()), ("reviewer".into(), selected.clone())]);
        validate_frozen_vessel_skills(bundle, &crews, &declared, &tokens, &runner).await.expect("shared names remain isolated by crew");
        let mut incompatible = crews.clone();
        incompatible.get_mut("reviewer").expect("reviewer")[0].revision = "3".repeat(40);
        assert!(validate_frozen_vessel_skills(bundle, &incompatible, &declared, &tokens, &runner)
            .await
            .expect_err("incompatible shared source pins")
            .contains("two revisions"));
        validate_frozen_skills(bundle, &selected, &declared, &tokens, &runner).await.expect("old pin remains satisfiable");
        assert_eq!(std::fs::read_to_string(&token).expect("operator token"), "test-token");
        std::fs::write(temp.path().join("fetches.wrong-revision"), "").expect("remote serves wrong pin");
        let error = validate_frozen_skills(bundle, &selected, &declared, &tokens, &runner).await.expect_err("exact revision required");
        assert!(error.contains("revision"), "{error}");
        assert_eq!(std::fs::read_to_string(token).expect("operator token survives refusal"), "test-token");
    }

    // #2875: failures before the Git script carry the same named context and
    // underlying cause in both returned status material and the warning.
    #[tokio::test]
    async fn skill_resolution_warning_reports_the_frozen_source_and_cause() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let bundle = registry.skills.source.as_ref().expect("source");
        std::fs::write(bundle.join(SKILL_BUNDLE_MANIFEST), r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"2222222222222222222222222222222222222222","credential":"private-skills"}]}"#).expect("manifest");
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogCaptureWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let error =
            registry.skill_tree(&[crew_skill("private-source", "skills-other/private-folder")]).await.expect_err("unauthorized path");
        assert!(error.contains("outside the authorized source paths"));
        let log = String::from_utf8(output.lock().expect("log").clone()).expect("UTF-8 log");
        for expected in [
            "skill staging failed",
            "private-skills",
            "1111111111111111111111111111111111111111",
            "credential=",
            "outside the authorized source paths",
        ] {
            assert!(log.contains(expected), "missing {expected}: {log}");
        }
        assert!(log.contains("credential=private-skills") || log.contains("credential=\"private-skills\""), "{log}");
        assert!(!log.contains("source=unknown") && !log.contains("revision=unknown"), "{log}");
    }

    // A bundle-level failure must not blame an unrelated selected source.
    #[tokio::test]
    async fn skill_bundle_warning_does_not_attribute_a_missing_bundle_to_selected_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        std::fs::remove_dir_all(registry.skills.source.as_ref().expect("source")).expect("remove bundle");
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = LogCaptureWriter(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        registry.skill_tree(&[crew_skill("private-source", "skills/private-folder")]).await.expect_err("missing bundle");
        let log = String::from_utf8(output.lock().expect("log").clone()).expect("UTF-8 log");
        assert!(log.contains("source=skill-bundle") || log.contains("source=\"skill-bundle\""), "{log}");
        assert!(!log.contains("source=private-skills") && !log.contains("source=\"private-skills\""), "{log}");
    }

    // #2875: retaining a frozen revision never authorizes another repository,
    // an undeclared source, path traversal, or an unpinned ref. Diagnostics name
    // the source, revision, credential and concrete refusal before any mint.
    #[hegel::test]
    fn frozen_skill_resolution_preserves_source_authority(tc: hegel::TestCase) {
        let case = tc.draw(hegel::generators::integers::<u8>().min_value(0).max_value(5));
        let source = SkillSource {
            name: "private-skills".into(),
            repository: "https://github.com/example/private-skills.git".into(),
            revision: "2".repeat(40),
            credential: Some("private-skills".into()),
            paths: vec!["skills".into()],
        };
        let mut entry = crew_skill("private-source", "skills/private-folder");
        match case {
            0 => entry.repository = "other/private-skills".into(),
            1 => entry.path = "skills/../escape".into(),
            2 => entry.path = "skills-other/private-folder".into(),
            3 => entry.revision = "main".into(),
            4 => entry.source = "undeclared".into(),
            5 => entry.name = "../escape".into(),
            _ => unreachable!(),
        }
        let error = resolve_frozen_sources(&[source], std::slice::from_ref(&entry)).expect_err("untrusted frozen material refused");
        assert!(error.contains(&entry.source) && error.contains(&entry.revision) && error.contains("credential"), "{error}");
        assert!(
            error.contains("differs")
                || error.contains("invalid")
                || error.contains("outside")
                || error.contains("full commit SHA")
                || error.contains("not declared"),
            "{error}"
        );
    }

    // #2875: a standing governor's frozen selection survives a reboot into a
    // newer generation, retaining its revision and narrowed source credential.
    // #3655, #3964: a Codex crew links the login of the vessel's delivered home only when
    // that home is flotilla-managed under the target's writable base or the contained Codex
    // home; a personal, escaping or absent home falls back to the target's ambient login.
    // Claude crews run on their bound account and link no login.
    #[test]
    fn harness_login_links_only_a_managed_delivered_home() {
        let registry = test_registry(Arc::new(TestEnvVars::new([("HOME", "/controller-home")])));
        let base = Path::new("/host/state/flotilla");
        let login = |adapter: &str, delivered: Option<&str>| {
            let environment = delivered.map(|home| vec![("CODEX_HOME".to_string(), home.to_string())]).unwrap_or_default();
            registry.harness_login(adapter, &environment, base).map(|login| login.digest())
        };
        for (delivered, expected) in [
            (Some("/host/state/flotilla/agent-homes/codex-key/codex"), "delivered:/host/state/flotilla/agent-homes/codex-key/codex"),
            (Some(CONTAINER_CODEX_HOME), "delivered:/tmp/flotilla-codex"),
            (Some("/home/crew/.codex"), "ambient"),
            (Some("/host/state/flotilla/agent-homes/../../../home/crew/.codex"), "ambient"),
            (Some("relative/codex"), "ambient"),
            (None, "ambient"),
        ] {
            assert_eq!(login(CODEX_ADAPTER_ID, delivered).as_deref(), Some(expected), "{delivered:?}");
        }
        assert_eq!(login(CLAUDE_CODE_ADAPTER_ID, None), None);
        assert_eq!(login("unmanaged-harness", None), None);
    }

    // #2875: a governor's frozen selection survives a reboot into a newer generation,
    // retaining its revision and narrowed source credential.
    #[tokio::test]
    async fn governor_frozen_skills_resolve_after_generation_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let bundle = registry.skills.source.as_ref().expect("source");
        std::fs::write(bundle.join(SKILL_BUNDLE_MANIFEST), r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"2222222222222222222222222222222222222222","credential":"private-skills"}]}"#).expect("new generation manifest");
        let runner = promisor_runner(temp.path());
        let home = runner.config_base.join("agent-homes/requesters/standing/governor/claude");
        let tree = registry.skill_tree(&[crew_skill("private-source", "skills/private-folder")]).await.expect("resolve frozen tree");
        let requests = tree.credentials();
        assert_eq!(requests.len(), 1, "frozen revision still needs its source credential");
        assert_eq!(requests[0].revision, "1".repeat(40));
        let token = temp.path().join("token");
        std::fs::write(&token, "test-token").expect("token");
        registry
            .stage_skill_tree(&tree, &home.join("skills"), &BTreeMap::from([("private-skills".into(), token)]), &runner)
            .await
            .expect("stage frozen revision after restart");
        assert!(home.join("skills/private-source/SKILL.md").is_file());
        let staged: SkillBundleManifest =
            serde_json::from_slice(&std::fs::read(home.join("skills").join(SKILL_BUNDLE_MANIFEST)).expect("staged manifest"))
                .expect("manifest");
        assert_eq!(staged.sources[0].revision, "1".repeat(40), "staged provenance retains the admission pin");
    }

    // Issue #2672: crews sharing a harness and vessel have disjoint homes, each holding
    // exactly its frozen selection, including none, and reusing one pinned fetch. Each
    // links the vessel's delivered login: static config seeds a crew once, a crew's own
    // edits survive, and a refreshed login reaches every crew.
    #[tokio::test]
    async fn crews_in_one_vessel_see_only_their_own_skills_and_share_the_login() {
        for api_key in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let registry = registry(temp.path());
            let bundle = registry.skills.source.as_ref().expect("source");
            std::fs::write(bundle.join(SKILL_BUNDLE_MANIFEST), r#"{"schema_version":5,"sources":[{"name":"private-skills","repository":"https://github.com/example/private-skills.git","revision":"1111111111111111111111111111111111111111","credential":"private-skills"}]}"#).expect("manifest");
            let runner = promisor_runner(temp.path());
            let base = if api_key { runner.config_base.join("agent-homes/model-api/codex") } else { runner.config_base.join("codex") };
            std::fs::create_dir_all(&base).expect("base home");
            std::fs::write(base.join("settings.json"), "{}").expect("settings");
            std::fs::write(base.join("auth.json"), "first-token").expect("auth");
            let delivered = if api_key { base.display().to_string() } else { CONTAINER_CODEX_HOME.to_string() };
            let login = registry
                .harness_login(CODEX_ADAPTER_ID, &[("CODEX_HOME".into(), delivered)], &runner.config_base)
                .expect("Codex links a login");
            let home = |role: &str| runner.config_base.join("agent-homes/requesters/shared").join(role).join("codex");
            // The vessel's file facilities over this process's own shell and files.
            let agent = flotilla_environment::environment::vessel_ops::RunnerVesselAgent(Arc::new(runner.clone()));
            let crews = [
                ("coder", vec![crew_skill("private-source", "skills/private-folder")]),
                ("reviewer", vec![crew_skill("unselected", "skills/unselected")]),
                ("observer", Vec::new()),
            ];
            for (role, selected) in &crews {
                let token = temp.path().join(format!("token-{role}"));
                std::fs::write(&token, "test-token").expect("token");
                let tree = registry.skill_tree(selected).await.expect("resolve");
                registry
                    .stage_skill_tree(&tree, &home(role).join("skills"), &BTreeMap::from([("private-skills".into(), token)]), &runner)
                    .await
                    .expect("stage crew");
                login.link(&home(role), &agent).await.expect("link login");
            }
            assert!(home("coder").join("skills/private-source/SKILL.md").is_file());
            let skills = |role: &str| {
                std::fs::read_dir(home(role).join("skills"))
                    .expect("skills")
                    .filter_map(|entry| entry.ok().filter(|entry| entry.file_type().expect("type").is_dir()).map(|entry| entry.file_name()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(skills("coder"), ["private-source"]);
            assert_eq!(skills("reviewer"), ["unselected"]);
            assert!(skills("observer").is_empty());
            assert!(!base.join("skills").exists(), "no vessel-wide skill union");
            assert_eq!(
                std::fs::read_to_string(temp.path().join("fetches")).expect("fetch log").lines().count(),
                1,
                "crews reuse the pinned source cache"
            );
            for (role, _) in &crews {
                assert_eq!(std::fs::read_to_string(home(role).join("settings.json")).expect("settings"), "{}");
                assert_eq!(std::fs::read_to_string(home(role).join("auth.json")).expect("linked auth"), "first-token");
            }
            std::fs::write(base.join("settings.json"), "base changed").expect("base update");
            std::fs::write(home("coder").join("settings.json"), "crew edited").expect("crew edit");
            login.link(&home("coder"), &agent).await.expect("relink");
            assert_eq!(
                std::fs::read_to_string(home("coder").join("settings.json")).expect("crew settings"),
                "crew edited",
                "base changes never overwrite crew scratch"
            );
            let replacement = base.join("auth.new");
            std::fs::write(&replacement, "refreshed-token").expect("new auth");
            std::fs::rename(replacement, base.join("auth.json")).expect("atomic refresh");
            for (role, _) in &crews {
                assert_eq!(std::fs::read_to_string(home(role).join("auth.json")).expect("refreshed linked auth"), "refreshed-token");
            }
        }
    }
}
