use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_environment::environment::runner::CONTAINED_CODEX_HOME;
use flotilla_environment::environment::runner::{CONTAINED_CODEX_AUTH_FILE, HARNESS_AUTH_FILES};
use flotilla_environment::environment::ProvisionedMount;
use flotilla_environment::environment::ProvisionedMountMode;
use flotilla_environment::environment::{EntryKind, VesselAgent};
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use futures::future::BoxFuture;
use tokio::{fs, io::AsyncWriteExt};

use crate::{
    codex_central::codex_central_auth_path,
    skill_trees::{inspect_skill_sources, SkillTree, SkillTreeError},
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

/// The home variable and directory name a harness's crews use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessHome {
    /// The variable that points the harness at its home (`CLAUDE_CONFIG_DIR`).
    pub variable: &'static str,
    /// The harness's directory name inside a requester's home (`claude`).
    pub name: &'static str,
}

/// The login a crew's own home holds: a home credential delivery prepared (a model API
/// key), linked so a refresh there reaches every crew, or, without one, a read-only copy of
/// the owning daemon's central login (#4037). A target's ambient login is never linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessLogin {
    /// The home credential delivery prepared, whose login files the crew's home links.
    Delivered(PathBuf),
    /// The central login's crew copy (see [`crew_copy`]).
    Central(Vec<u8>),
}

/// Marks a login need whose digest names a central-login copy, so rotation finds the homes
/// holding one.
const CENTRAL_LOGIN_DIGEST: &str = "central:";

impl HarnessLogin {
    /// Content identity of what the crew's home holds: a central copy by its bytes, so a
    /// rotation changes it.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        match self {
            Self::Delivered(base) => format!("delivered:{}", base.display()),
            Self::Central(copy) => format!("{CENTRAL_LOGIN_DIGEST}{:x}", Sha256::digest(copy)),
        }
    }

    /// Whether a login need's recorded `digest` names a central copy.
    pub fn is_central(digest: &str) -> bool {
        digest.starts_with(CENTRAL_LOGIN_DIGEST)
    }

    /// Deliver the login into `home` through the vessel's agent. A delivered home's static
    /// config seeds the crew once; later edits there never overwrite the crew's own, and its
    /// credentials are linked, never copied. A central copy replaces whatever `auth.json`
    /// the home held, a link included, atomically and read-only.
    pub async fn deliver(&self, home: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        let failed = |error: String| format!("deliver harness login into {}: {error}", home.display());
        let base = match self {
            Self::Delivered(base) => base,
            Self::Central(copy) => return agent.write(&home.join(CODEX_AUTH_FILE), copy.clone(), CODEX_AUTH_MODE).await.map_err(failed),
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

#[async_trait]
trait AgentMaterialAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn fragment(&self, environment: &BTreeMap<String, String>) -> Option<Fragment>;
    fn config_home_variable(&self) -> &'static str;
    /// Whether `config_home` is a home credential delivery prepared under `config_base`,
    /// whose login a crew links rather than taking the central login's copy.
    fn is_delivered_login_home(&self, config_home: &Path, config_base: &Path) -> bool;
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
                self.skills.prepare().await?;
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

    /// The login a crew of `adapter` holds in its own home, when its harness has one to
    /// share: the home the vessel's credential delivery prepared (`environment` is the
    /// resolved launch environment), otherwise the central login's crew copy. Never one
    /// whose crews run on a bound account (#3907).
    pub async fn harness_login(
        &self,
        adapter: &str,
        environment: &[(String, String)],
        config_base: &Path,
    ) -> Result<Option<HarnessLogin>, String> {
        let Some(material) = self.adapters.get(adapter) else { return Ok(None) };
        if material.requester_bound() {
            return Ok(None);
        }
        let variable = material.config_home_variable();
        let delivered = environment.iter().rev().find(|(name, _)| name == variable).map(|(_, home)| PathBuf::from(home)).filter(|home| {
            // `starts_with` compares components lexically, so a `..` could climb out of
            // the base while appearing inside it.
            home.is_absolute()
                && home.components().all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
                && material.is_delivered_login_home(home, config_base)
        });
        match delivered {
            Some(base) => Ok(Some(HarnessLogin::Delivered(base))),
            None => self.central_login().await.map(Some),
        }
    }

    /// The central login's crew copy as it stands now, for a launch or a rotation.
    pub async fn central_login(&self) -> Result<HarnessLogin, String> {
        read_crew_copy(&self.codex_central_auth_path).await.map(HarnessLogin::Central)
    }

    /// A crew's frozen skill selection, resolved once against this generation's
    /// authorized sources (ADR 0052): the tree the access layer delivers into the
    /// crew's home, and the sources its archive is built from.
    pub async fn skill_tree(&self, selected: &[flotilla_resources::SkillCatalogEntry]) -> Result<SkillTree, SkillTreeError> {
        SkillTree::resolve(self.skills.source.as_deref(), selected).await
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
            let credential = read_crew_copy(&self.codex_central_auth_path).await?;
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
/// ever touches its single-use refresh token. A crew's copy carries none
/// ([`crew_copy`]), so it can never refresh, and its file is read-only
/// (flotilla-org/flotilla#1906, #4037). The delivery is a *copy* rather than a mount of
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

/// The central login at `source`, as a crew copy.
async fn read_crew_copy(source: &Path) -> Result<Vec<u8>, String> {
    let central = fs::read(source).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("central Codex credential {} is not provisioned on this host", source.display())
        } else {
            format!("read central Codex credential {}: {error}", source.display())
        }
    })?;
    crew_copy(&central).map_err(|error| format!("central Codex credential {}: {error}", source.display()))
}

/// A crew's copy of the central login (#4037): the central `auth.json` with its refresh
/// token blanked, so the refresher alone ever holds it. codex-rs requires the field but
/// accepts it empty (`TokenData::refresh_token` in `codex-rs/login/src/token_data.rs`,
/// verified at openai/codex@133c11db), loading the copy as ChatGPT auth that runs on its
/// access token. A refresh it then attempts sends the empty token, which the endpoint
/// rejects, and `logout` revokes nothing (`codex-rs/login/src/auth/revoke.rs`). However long
/// pushes fail, a copy fails closed at its access token's expiry and can never consume,
/// rotate or revoke the central lineage. Docker and SSH-host crews take the same copy.
fn crew_copy(central: &[u8]) -> Result<Vec<u8>, String> {
    let mut auth: serde_json::Value = serde_json::from_slice(central).map_err(|_| "is not valid JSON".to_string())?;
    if let Some(tokens) = auth.get_mut("tokens").and_then(serde_json::Value::as_object_mut) {
        tokens.insert("refresh_token".to_string(), serde_json::Value::String(String::new()));
    }
    // A login without `tokens` (an API-key `auth.json`) holds no refresh token to withhold.
    let mut copy = serde_json::to_vec_pretty(&auth).map_err(|error| format!("encode crew copy: {error}"))?;
    copy.push(b'\n');
    Ok(copy)
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

/// The generation's skill supply. Only its manifest is read, on the daemon
/// host; the trees are fetched there and pushed at launch (#3663).
#[derive(Debug, Clone)]
struct SkillBundle {
    source: Option<PathBuf>,
}

impl SkillBundle {
    fn new(source: Option<PathBuf>) -> Self {
        Self { source }
    }

    fn path(&self) -> Result<&Path, String> {
        self.source
            .as_deref()
            .ok_or_else(|| format!("contained agent requires generation-pinned skill sources declared by {FLOTILLA_SKILLS_DIR_ENV}"))
    }

    /// Refuse a contained agent before its container exists when the
    /// generation declares no usable skill supply.
    async fn prepare(&self) -> Result<(), String> {
        let source = self.path()?.to_path_buf();
        tokio::task::spawn_blocking(move || inspect_skill_sources(&source))
            .await
            .map_err(|error| format!("inspect generation-pinned skill bundle task failed: {error}"))?
            .map(drop)
    }
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

    fn is_delivered_login_home(&self, config_home: &Path, config_base: &Path) -> bool {
        config_home.starts_with(config_base.join("agent-homes"))
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
        let credential = read_crew_copy(&self.central_auth_path).await?;
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

    fn is_delivered_login_home(&self, config_home: &Path, config_base: &Path) -> bool {
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
    use std::{os::unix::fs::PermissionsExt, path::Path};

    use flotilla_discovery_testkit::TestEnvVars;
    use flotilla_provider_runtime::{ChannelLabel, CommandOutput};

    use super::*;
    use crate::skill_trees::SKILL_BUNDLE_MANIFEST;

    /// Process boundary: real commands on this host, with the contained writable base and
    /// the contained Codex home both under a temporary directory.
    #[derive(Clone)]
    struct ContainedRunner {
        config_base: PathBuf,
    }

    #[async_trait]
    impl CommandRunner for ContainedRunner {
        async fn run(&self, cmd: &str, args: &[&str], cwd: &Path, label: &ChannelLabel) -> Result<String, String> {
            let args = args
                .iter()
                .map(|arg| match Path::new(arg).strip_prefix(CONTAINER_CODEX_HOME) {
                    Ok(relative) => self.config_base.join("codex").join(relative).display().to_string(),
                    Err(_) => (*arg).to_string(),
                })
                .collect::<Vec<_>>();
            let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
            flotilla_provider_runtime::ProcessCommandRunner.run(cmd, &refs, cwd, label).await
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

        async fn write_file_with_mode(&self, path: &Path, content: &str, mode: u32) -> Result<(), String> {
            flotilla_provider_runtime::ProcessCommandRunner.write_file_with_mode(path, content, mode).await
        }
    }

    /// Writes the host's central `auth.json` at exactly the path
    /// [`codex_central_auth_path`] derives from `HOME`, so the tests bind to the
    /// same contract the daemon refresher writes to.
    fn write_central_auth(home: &Path, access_token: &str) -> PathBuf {
        let path = home.join(".config/flotilla/credentials/codex-central/auth.json");
        std::fs::create_dir_all(path.parent().expect("central credential directory")).expect("create central credential directory");
        std::fs::write(&path, format!("{{\"tokens\":{{\"access_token\":\"{access_token}\",\"refresh_token\":\"central-refresh\"}}}}"))
            .expect("write central auth");
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
                assert_eq!(deliveries.len(), 1);
                assert_eq!(
                    std::fs::read(deliveries[0].mount.host_path.as_path().join(CODEX_AUTH_FILE)).expect("delivered auth"),
                    crew_copy(&std::fs::read(registry.codex_credential_source()).expect("central auth")).expect("crew copy"),
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

        assert_eq!(deliveries.len(), 1, "skills are pushed at launch, not mounted");
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
            crew_copy(&std::fs::read(&central).expect("central credential")).expect("crew copy"),
            "the crew must receive the refresher's auth.json less its refresh token"
        );
        assert_eq!(delivered_auth_mode(&delivered), CODEX_AUTH_MODE, "the delivered credential must be read-only");
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
                std::fs::read(home.join(CODEX_AUTH_FILE)).expect("delivered credential"),
                crew_copy(&std::fs::read(registry.codex_credential_source()).expect("central credential")).expect("crew copy")
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
        assert!(deliveries.is_empty(), "the canary needs no login, and skills are pushed at launch");
        assert!(!temp.path().join(".local/share/flotilla/agent-homes").exists());
    }

    // #3655, #3964, #4037: a Codex crew links the login of the vessel's delivered home only
    // when credential delivery prepared that home under the target's writable base. Every
    // other home, the contained Codex home and a personal, escaping or absent one included,
    // takes the central login's crew copy; the target's ambient login is never linked.
    // Claude crews run on their bound account and hold no login.
    #[tokio::test]
    async fn harness_login_links_only_a_credential_delivered_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_central_auth(temp.path(), "access-token-one");
        let registry = registry(temp.path());
        let central = registry.central_login().await.expect("central crew copy");
        let base = Path::new("/host/state/flotilla");
        let delivered = "/host/state/flotilla/agent-homes/codex-key/codex";
        for (home, expected) in [
            (Some(delivered), HarnessLogin::Delivered(PathBuf::from(delivered))),
            (Some(CONTAINER_CODEX_HOME), central.clone()),
            (Some("/home/crew/.codex"), central.clone()),
            (Some("/host/state/flotilla/agent-homes/../../../home/crew/.codex"), central.clone()),
            (Some("relative/codex"), central.clone()),
            (None, central.clone()),
        ] {
            let environment = home.map(|home| vec![("CODEX_HOME".to_string(), home.to_string())]).unwrap_or_default();
            assert_eq!(registry.harness_login(CODEX_ADAPTER_ID, &environment, base).await.expect("login"), Some(expected), "{home:?}");
        }
        assert!(HarnessLogin::is_central(&central.digest()));
        assert_eq!(registry.harness_login(CLAUDE_CODE_ADAPTER_ID, &[], base).await.expect("no login"), None);
        assert_eq!(registry.harness_login("unmanaged-harness", &[], base).await.expect("no login"), None);
    }

    // #4037 contract 3: a crew's copy can never refresh the central lineage, however long
    // pushes fail: it carries no refresh token, and it keeps everything else Codex reads.
    #[test]
    fn a_crew_copy_never_carries_the_central_refresh_token() {
        let central = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"access","refresh_token":"central-refresh","id_token":"id","account_id":"acct"},"last_refresh":"2026-10-27T00:00:00Z"}"#;

        let copy: serde_json::Value = serde_json::from_slice(&crew_copy(central).expect("crew copy")).expect("JSON copy");

        let mut expected: serde_json::Value = serde_json::from_slice(central).expect("JSON central");
        expected["tokens"]["refresh_token"] = serde_json::Value::String(String::new());
        assert_eq!(copy, expected);
        assert!(crew_copy(b"not json").is_err(), "an unreadable central login is never delivered");
    }

    // Issue #2672: crews sharing a harness and vessel each link the vessel's delivered
    // login: static config seeds a crew once, a crew's own edits survive, and a refreshed
    // login reaches every crew.
    #[tokio::test]
    async fn crews_in_one_vessel_share_the_login() {
        let temp = tempfile::tempdir().expect("tempdir");
        let registry = registry(temp.path());
        let runner = ContainedRunner { config_base: temp.path().join("config") };
        let base = runner.config_base.join("agent-homes/model-api/codex");
        std::fs::create_dir_all(&base).expect("base home");
        std::fs::write(base.join("settings.json"), "{}").expect("settings");
        std::fs::write(base.join("auth.json"), "first-token").expect("auth");
        let login = registry
            .harness_login(CODEX_ADAPTER_ID, &[("CODEX_HOME".into(), base.display().to_string())], &runner.config_base)
            .await
            .expect("login")
            .expect("Codex links a login");
        let home = |role: &str| runner.config_base.join("agent-homes/requesters/shared").join(role).join("codex");
        // The vessel's file facilities over this process's own shell and files.
        let agent = flotilla_environment::environment::vessel_ops::RunnerVesselAgent(Arc::new(runner.clone()));
        let crews = ["coder", "reviewer"];
        for role in crews {
            std::fs::create_dir_all(home(role)).expect("crew home");
            login.deliver(&home(role), &agent).await.expect("link login");
        }
        for role in crews {
            assert_eq!(std::fs::read_to_string(home(role).join("settings.json")).expect("settings"), "{}");
            assert_eq!(std::fs::read_to_string(home(role).join("auth.json")).expect("linked auth"), "first-token");
        }
        std::fs::write(base.join("settings.json"), "base changed").expect("base update");
        std::fs::write(home("coder").join("settings.json"), "crew edited").expect("crew edit");
        login.deliver(&home("coder"), &agent).await.expect("relink");
        assert_eq!(
            std::fs::read_to_string(home("coder").join("settings.json")).expect("crew settings"),
            "crew edited",
            "base changes never overwrite crew scratch"
        );
        let replacement = base.join("auth.new");
        std::fs::write(&replacement, "refreshed-token").expect("new auth");
        std::fs::rename(replacement, base.join("auth.json")).expect("atomic refresh");
        for role in crews {
            assert_eq!(std::fs::read_to_string(home(role).join("auth.json")).expect("refreshed linked auth"), "refreshed-token");
        }
    }
}
