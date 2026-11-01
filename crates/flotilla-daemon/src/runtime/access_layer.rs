//! The access layer of vessel delivery (#3964, #3956 Q4): per crew, before its harness
//! starts, the crew's harness home, skills and account material are delivered aboard its
//! vessel through the vessel agent, the same way for every environment kind.
//!
//! The launch path builds one request per crew: an opaque access key, the harness, the
//! home (its key under the target's durable home base), the crew's resolved skill tree and
//! its account material (in a material directory under the target's runtime base, #4012).
//! The planner lowers each need to a mechanism the place offers, and the one executor runs
//! the plan. Launch then points the harness at the home the delivery returned; nothing
//! derives the home again.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_credentials::{
    AgentMaterialRegistry, HarnessHome, HarnessLogin, SkillArchive, SkillSourceTokens, SkillTree, SkillTreeError, SkillTrees,
};
use flotilla_environment::environment::{vessel_ops, EntryKind, VesselAgent};
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    account_access::{requester_home, requester_material, AccessName},
    AccessDelivery, Convoy, Environment, PinUnpack, ResourceError, ResourceObject, Vessel, CONVOY_LABEL,
};
use flotilla_schema::Timestamp;
use flotilla_vessel_delivery::{
    deliver, Access, AccessKey, ContentDigest, CostTable, DeliveryError, HomeBase, HostPathOffer, HostReach, Item, Liveness, Mechanism,
    Need, NeedName, Plan, ProviderMechanisms, Reachability, RelativePath, StepFailure, TreeDelivery, TreeSources,
};

use self::{
    harness::{PinnedHarness, HARNESS_PATH},
    status::AccessStatus,
};

mod harness;
pub(super) mod status;

use super::account_access::{AccountLaunch, BoundAccount};
use super::credentials::agent_material_environment;
use super::state::ControllerRuntimeState;

const HOME: &str = "home";
/// The requester's material directory, on the runtime base: secrets a reboot may clear.
const SECRETS: &str = "secrets";
const SKILLS: &str = "skills";
const ACCOUNT: &str = "account";
const LOGIN: &str = "login";
const HARNESS: &str = "harness";
/// The need a failure is recorded against when it was in planning, not in one need's step.
const PLAN: &str = "plan";

/// One crew's launch, as the access layer needs it.
pub(super) struct AccessLaunch<'a> {
    pub account: AccountLaunch<'a>,
    /// The Vessel resource the crew is aboard.
    pub vessel_ref: &'a str,
    /// The harness adapter the crew runs.
    pub adapter: &'a str,
    /// The account access its harness item declares, if any.
    pub access: Option<&'a AccessName>,
    /// Whether the place already carries the adapter's harness: a composed crew image's
    /// layers, or a cloud image whose interior discovery found one. An SSH host never
    /// does, whatever it has installed.
    pub carries_harness: bool,
}

/// What the access layer delivered for a launch.
#[derive(Debug, Default)]
pub(super) struct AccessDelivered {
    /// The variables that point the harness at what landed: its home, and its account material.
    pub environment: Vec<(String, String)>,
    /// The pinned harness's executable aboard, where the place does not carry it.
    pub executable: Option<PathBuf>,
}

/// Deliver the crew's access layer aboard its vessel through `agent`. `delivered` is the
/// launch environment credential delivery resolved, which names the vessel's delivered
/// harness homes.
pub(super) async fn deliver_access(
    state: &Arc<ControllerRuntimeState>,
    launch: &AccessLaunch<'_>,
    agent: Arc<dyn VesselAgent>,
    delivered: &[(String, String)],
) -> Result<AccessDelivered, String> {
    let material = state.agent_material.as_ref().ok_or("vessel delivery requires the agent material registry")?;
    let Some(harness) = material.harness_home(launch.adapter) else { return Ok(AccessDelivered::default()) };
    let crew = frozen_crew(state, &launch.account, launch.vessel_ref).await?;
    let status = AccessStatus::read(state, &launch.account, &crew.access, launch.adapter).await?;
    status.due(state)?;
    match deliver_crew(state, launch, material, &harness, crew, agent, delivered).await {
        Ok((plan, home, environment, executable)) => {
            status.delivered(state, &plan, &home).await?;
            Ok(AccessDelivered { environment, executable })
        }
        Err(Failed { need, error, permanent }) => {
            status.failed(state, need, &error, permanent).await?;
            Err(error)
        }
    }
}

/// Deliver a requester's account on the daemon's own host, which has no vessel: the daemon
/// writes it through `runner` (#3907), in the layout the access layer uses aboard a vessel.
/// The home is under the daemon's own durable state directory, and the material under the
/// credential store's runtime base (#4012). The daemon's host carries its own harness.
pub(super) async fn deliver_host_account(
    state: &ControllerRuntimeState,
    launch: &AccountLaunch<'_>,
    access: &AccessName,
    runner: &dyn CommandRunner,
) -> Result<AccessDelivered, String> {
    let bound = BoundAccount::resolve(state, launch, access).await?;
    let store = state.credential_store.as_ref().ok_or("host-local credential store unavailable for account delivery")?;
    let home = requester_home(store.home_base(), launch.convoy, launch.role, "claude");
    let dir = requester_material(&store.writable_base(runner).await?, launch.convoy, launch.role, "claude").display().to_string();
    vessel_ops::create_dir(runner, &home, 0o700).await?;
    bound.deliver(state, launch, &dir, runner).await?;
    Ok(AccessDelivered { environment: bound.environment(&home.display().to_string(), &dir), executable: None })
}

/// A delivery that failed at one need: the need it was preparing or delivering, and
/// whether retrying the same delivery cannot help.
struct Failed {
    need: String,
    error: String,
    permanent: bool,
}

fn failed_at(need: &str) -> impl FnOnce(String) -> Failed + '_ {
    move |error| Failed { need: need.to_string(), error, permanent: false }
}

async fn deliver_crew(
    state: &Arc<ControllerRuntimeState>,
    launch: &AccessLaunch<'_>,
    material: &Arc<AgentMaterialRegistry>,
    harness: &HarnessHome,
    crew: FrozenCrew,
    agent: Arc<dyn VesselAgent>,
    delivered: &[(String, String)],
) -> Result<(Plan, String, Vec<(String, String)>, Option<PathBuf>), Failed> {
    let access = AccessKey::new(crew.access);
    let (convoy, role) = (launch.account.convoy, launch.account.role);
    let home = |name: &str, key: std::path::PathBuf, base: HomeBase| -> Result<Need, Failed> {
        Ok(Need {
            name: NeedName::new(name),
            digest: ContentDigest::new(key.display().to_string()),
            item: Item::Home { key: RelativePath::new(key).map_err(failed_at(name))?, base },
        })
    };
    let mut needs = vec![home(HOME, requester_home(Path::new(""), convoy, role, harness.name), HomeBase::Durable)?];
    let mut trees: BTreeMap<NeedName, Arc<dyn TreeDelivery>> = BTreeMap::new();
    let mut tree = |needs: &mut Vec<Need>,
                    name: &str,
                    digest: String,
                    (home, path): (&str, &str),
                    sources: TreeSources,
                    delivery: Arc<dyn TreeDelivery>| {
        needs.push(Need {
            name: NeedName::new(name),
            digest: ContentDigest::new(digest),
            item: Item::Tree { home: NeedName::new(home), path: RelativePath::new(path).expect("a constant relative path"), sources },
        });
        trees.insert(NeedName::new(name), delivery);
    };
    let skills = material.skill_tree(&crew.skills).await.map_err(|SkillTreeError { message, permanent }| Failed {
        need: SKILLS.to_string(),
        error: message,
        permanent,
    })?;
    tree(
        &mut needs,
        SKILLS,
        skills.digest(),
        (HOME, "skills"),
        TreeSources { controller: true, ..TreeSources::default() },
        Arc::new(SkillsDelivery {
            trees: state.skill_trees.clone(),
            tokens: state.credential_store.clone().map(|store| store as Arc<dyn SkillSourceTokens>),
            tree: skills,
        }),
    );
    let bound = match launch.access {
        Some(access) => Some(Arc::new(BoundAccount::resolve(state, &launch.account, access).await.map_err(failed_at(ACCOUNT))?)),
        None => None,
    };
    if let Some(bound) = &bound {
        // The token stays off the durable home: a reboot may clear it, with the record that
        // it landed, and the next launch delivers it again.
        needs.push(home(SECRETS, requester_material(Path::new(""), convoy, role, harness.name), HomeBase::Runtime)?);
        tree(
            &mut needs,
            ACCOUNT,
            bound.digest(state, &launch.account).await.map_err(failed_at(ACCOUNT))?,
            (SECRETS, flotilla_agent::account_access::CLAUDE_OAUTH_TOKEN_FILE),
            TreeSources { controller: true, ..TreeSources::default() },
            Arc::new(AccountDelivery { state: state.clone(), launch: OwnedAccountLaunch::of(&launch.account), bound: bound.clone() }),
        );
    }
    let base = agent.writable_base().await.map_err(failed_at(HOME))?;
    // A container's harness defaults (its contained Codex home) apply only inside it; an
    // SSH host's harness homes are what credential delivery put under its base.
    let resolved = if launch.account.ssh_host {
        delivered.to_vec()
    } else {
        let declared = declared_environment(state, &launch.account).await.map_err(failed_at(LOGIN))?;
        agent_material_environment(material, &[launch.adapter.to_string()].into(), &declared, delivered.iter().cloned())
            .map_err(failed_at(LOGIN))?
    };
    if let Some(login) = material.harness_login(launch.adapter, &resolved, &base).await.map_err(failed_at(LOGIN))? {
        tree(
            &mut needs,
            LOGIN,
            login.digest(),
            (HOME, "auth.json"),
            TreeSources { controller: true, ..TreeSources::default() },
            Arc::new(LoginDelivery(login)),
        );
    }
    // #3909: where the place does not already carry the pinned harness, it lands in the
    // home by digest, through whichever mechanism the planner chooses.
    let pinned = if launch.carries_harness {
        None
    } else {
        let architecture = agent.architecture().await.map_err(failed_at(HARNESS))?;
        PinnedHarness::resolve(launch.adapter, &architecture).map_err(failed_at(HARNESS))?
    };
    if let Some(pinned) = &pinned {
        tree(&mut needs, HARNESS, pinned.sha256.clone(), (HOME, HARNESS_PATH), PinnedHarness::sources(), Arc::new(pinned.clone()));
    }
    let place = launch.account.env_ref;
    let delivered = deliver(&needs, &trees, &place_mechanisms(launch.account.ssh_host), &CostTable::default(), agent.as_ref())
        .await
        .map_err(|error| Failed {
            need: match &error {
                DeliveryError::Step { need, .. } | DeliveryError::NoDelivery(need) | DeliveryError::Unrecordable(need) => need.to_string(),
                DeliveryError::Plan(_) | DeliveryError::Agent(_) => PLAN.to_string(),
            },
            permanent: error.is_permanent(),
            error: format!("deliver access `{access}` aboard `{place}`: {error}"),
        })?;
    for step in &delivered.plan.steps {
        tracing::debug!(%access, place, need = %step.need, mechanism = %step.mechanism, action = ?step.action, "vessel delivery");
    }
    let landed = |need: &str| {
        delivered.homes.get(&NeedName::new(need)).map(|home| home.display().to_string()).ok_or_else(|| Failed {
            need: need.to_string(),
            error: format!("deliver access `{access}` aboard `{place}`: no `{need}` home was delivered"),
            permanent: false,
        })
    };
    let home = landed(HOME)?;
    let mut environment = vec![(harness.variable.to_string(), home.clone())];
    if let Some(bound) = bound {
        let bound = bound.environment(&home, &landed(SECRETS)?);
        environment.retain(|(name, _)| !bound.iter().any(|(bound, _)| bound == name));
        environment.extend(bound);
    }
    let executable = pinned.map(|pinned| pinned.executable(&Path::new(&home).join(HARNESS_PATH)));
    Ok((delivered.plan, home, environment, executable))
}

/// What a place offers vessel delivery beyond the mandatory agent fetch and push. An SSH
/// What a place offers vessel delivery beyond the mandatory agent fetch and push. An SSH
/// host's bases are the host's own storage; a container's are not, so its homes are
/// created through the agent. Both kinds in use reach public sources: neither declares an
/// egress policy. Step 4 of #3956 moves this declaration onto each provider instance.
fn place_mechanisms(ssh_host: bool) -> ProviderMechanisms {
    let host_paths = if ssh_host {
        vec![HostPathOffer { reach: HostReach::WritableBase, access: Access::ReadWrite, liveness: Liveness::Live }]
    } else {
        Vec::new()
    };
    ProviderMechanisms { host_paths, reach: Reachability { vessel_public: true } }
}

/// The crew's frozen entry, as the caller reads it from its convoy: its access key and
/// its skill selection. Delivery itself never reads a convoy.
struct FrozenCrew {
    access: String,
    skills: Vec<flotilla_resources::SkillCatalogEntry>,
}

async fn frozen_crew(state: &ControllerRuntimeState, launch: &AccountLaunch<'_>, vessel_ref: &str) -> Result<FrozenCrew, String> {
    let backend = state.daemon.resource_backend();
    let vessel = backend
        .including_replicas::<Vessel>(launch.namespace)
        .get(vessel_ref)
        .await
        .map_err(|error| format!("read vessel `{vessel_ref}` for crew `{}`: {error}", launch.role))?;
    let convoy = backend
        .including_replicas::<Convoy>(launch.namespace)
        .get(launch.convoy)
        .await
        .map_err(|error| format!("read convoy `{}` for crew `{}`: {error}", launch.convoy, launch.role))?;
    let crew = convoy
        .object
        .status
        .as_ref()
        .and_then(|status| status.workflow_snapshot.as_ref())
        .and_then(|snapshot| snapshot.vessels.iter().find(|vessel_requirement| vessel_requirement.name == vessel.object.spec.vessel_name))
        .and_then(|requirement| requirement.crew.iter().find(|crew| crew.role == launch.role))
        .ok_or_else(|| format!("crew `{}` is missing from convoy `{}`'s frozen vessel `{}`", launch.role, launch.convoy, vessel_ref))?;
    // The crew's Role Address, its access key until the #3108 cut-over.
    let project = convoy.object.spec.project_ref.as_deref().unwrap_or(launch.namespace);
    let access = format!("{project}/{}/{}/{}", launch.convoy, vessel.object.spec.vessel_name, launch.role);
    Ok(FrozenCrew { access, skills: crew.skills.selected.clone() })
}

/// What the vessel's Environment declares for its harnesses, such as an explicit home.
async fn declared_environment(state: &ControllerRuntimeState, launch: &AccountLaunch<'_>) -> Result<BTreeMap<String, String>, String> {
    match state.daemon.resource_backend().including_replicas::<Environment>(launch.namespace).get(launch.env_ref).await {
        Ok(environment) => {
            let spec = environment.object.spec;
            Ok(spec.docker.map(|docker| docker.env).or(spec.cloud_sandbox.map(|cloud| cloud.env)).unwrap_or_default())
        }
        Err(ResourceError::NotFound { .. }) => Ok(BTreeMap::new()),
        Err(error) => Err(error.to_string()),
    }
}

/// The crew's skill tree, fetched on the controlling daemon and pushed into the vessel
/// agent's content cache by digest (#3663). Source credentials are minted here and never
/// reach the vessel. Where the tree sits in the cache is the agent's own.
struct SkillsDelivery {
    trees: Arc<SkillTrees>,
    tokens: Option<Arc<dyn SkillSourceTokens>>,
    tree: SkillTree,
}

#[async_trait]
impl TreeDelivery for SkillsDelivery {
    async fn deliver(&self, mechanism: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), StepFailure> {
        if mechanism != Mechanism::AgentPush {
            return Err(StepFailure::permanent(format!("skills cannot be delivered by {mechanism}")));
        }
        let archive = self.trees.crew_archive(&self.tree, self.tokens.as_deref()).await.map_err(|error| {
            tracing::warn!(%error, permanent = error.permanent, "skill tree preparation failed");
            StepFailure { error: error.message, permanent: error.permanent }
        })?;
        push_tree(&archive, target, agent).await?;
        tracing::info!(target = %target.display(), sha256 = %archive.sha256, "delivered frozen crew skills");
        Ok(())
    }
    /// #4088: landed means the skills link still resolves into a store entry.
    async fn present(&self, target: &Path, agent: &dyn VesselAgent) -> Result<bool, String> {
        Ok(agent.linked_artifact(target).await?.is_some())
    }
}

/// Put `archive` into the agent's content cache and point `target` at it. A real
/// directory at `target`, left by a delivery that predates the cache, is moved aside
/// first: a link cannot replace it.
async fn push_tree(archive: &SkillArchive, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
    let file = archive.to_temp_file()?;
    agent.push_artifact(&archive.sha256, &file, PinUnpack::TarGz).await?;
    if agent.stat(target).await? == Some(EntryKind::Directory) {
        let name = target.file_name().ok_or("a skills tree needs a name")?.to_string_lossy();
        let aside = target.with_file_name(format!("{name}.legacy-{}", uuid::Uuid::new_v4().simple()));
        agent.rename(target, &aside).await?;
    }
    agent.link_artifact(target, &archive.sha256, Path::new("")).await
}

/// An owned copy of the launch's identity, for deliveries the executor holds.
struct OwnedAccountLaunch {
    namespace: String,
    convoy: String,
    role: String,
    env_ref: String,
    ssh_host: bool,
}

impl OwnedAccountLaunch {
    fn of(launch: &AccountLaunch<'_>) -> Self {
        Self {
            namespace: launch.namespace.into(),
            convoy: launch.convoy.into(),
            role: launch.role.into(),
            env_ref: launch.env_ref.into(),
            ssh_host: launch.ssh_host,
        }
    }

    fn borrow(&self) -> AccountLaunch<'_> {
        AccountLaunch {
            namespace: &self.namespace,
            convoy: &self.convoy,
            role: &self.role,
            env_ref: &self.env_ref,
            ssh_host: self.ssh_host,
        }
    }
}

/// The crew's bound account material (#3907), written into its material directory.
struct AccountDelivery {
    state: Arc<ControllerRuntimeState>,
    launch: OwnedAccountLaunch,
    bound: Arc<BoundAccount>,
}

#[async_trait]
impl TreeDelivery for AccountDelivery {
    async fn deliver(&self, _: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), StepFailure> {
        let dir = target.parent().ok_or("account material needs its material directory")?.display().to_string();
        Ok(self.bound.deliver(&self.state, &self.launch.borrow(), &dir, &*agent.commands()).await?)
    }
}

/// The crew's harness login, delivered into its home.
struct LoginDelivery(HarnessLogin);

#[async_trait]
impl TreeDelivery for LoginDelivery {
    async fn deliver(&self, _: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), StepFailure> {
        Ok(self.0.deliver(target.parent().ok_or("a login needs its home")?, agent).await?)
    }
}

/// How long one crew's login push may take before it is recorded as failed, so an unreachable
/// host delays the rest of a rotation pass by at most this.
const LOGIN_PUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// #4037: push the central login's current crew copy into every crew home holding an older
/// one, on every kind of place, through the crew's own vessel agent: the refresher rotates
/// the central login and no crew can refresh its copy. A failed push is recorded as the
/// access's retry, which `convoy explain` shows, and is attempted again once due.
pub(super) async fn redeliver_central_logins(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    let Some(material) = state.agent_material.as_ref() else { return Ok(()) };
    let backend = state.daemon.resource_backend();
    let now = Timestamp::from_datetime(&state.daemon.clock().now());
    let records = backend.using::<AccessDelivery>(namespace).list().await.map_err(|error| format!("list access deliveries: {error}"))?;
    let mut login = None;
    let vessels = tokio::sync::OnceCell::new();
    for record in records.items {
        let Some(landed) = record.spec.needs.get(LOGIN).filter(|landed| HarnessLogin::is_central(&landed.digest)) else { continue };
        let retry = record.spec.retry.as_ref().filter(|retry| retry.need == LOGIN);
        if retry.is_some_and(|retry| retry.next_at > now) {
            continue;
        }
        let login = match &login {
            Some(login) => login,
            None => login.insert(material.central_login().await?),
        };
        let digest = login.digest();
        if landed.digest == digest && retry.is_none() {
            continue;
        }
        let vessels = vessels
            .get_or_try_init(|| async {
                backend
                    .using::<Vessel>(namespace)
                    .list()
                    .await
                    .map(|vessels| vessels.items)
                    .map_err(|error| format!("list vessels: {error}"))
            })
            .await?;
        // A record whose vessel is gone goes with its place or convoy.
        let Some(vessel) = record_vessel(vessels, &record) else {
            tracing::debug!(access = %record.spec.access, place = %record.spec.place, "no live vessel for a central Codex login copy");
            continue;
        };
        let push = async {
            let access = match state.ssh_host.get(&record.spec.place) {
                Some(profile) => Some(state.vessel_access(profile, &vessel.metadata.name).await?),
                None => None,
            };
            let agent = state.vessel_agent(&record.spec.place, access.as_deref()).await?;
            login.deliver(Path::new(&record.spec.home), agent.as_ref()).await
        };
        let pushed = tokio::time::timeout(LOGIN_PUSH_TIMEOUT, push)
            .await
            .unwrap_or_else(|_| Err(format!("timed out after {}s", LOGIN_PUSH_TIMEOUT.as_secs())));
        match &pushed {
            Ok(()) => tracing::info!(access = %record.spec.access, place = %record.spec.place, "pushed the rotated central Codex login"),
            Err(error) => {
                tracing::warn!(access = %record.spec.access, place = %record.spec.place, %error, "failed to push the rotated central Codex login")
            }
        }
        // One record's write losing a race with a launch must not hold back every later crew;
        // the next pass reads it afresh.
        if let Err(error) = status::record_push(state, namespace, &record, LOGIN, &digest, pushed).await {
            tracing::warn!(access = %record.spec.access, place = %record.spec.place, %error, "failed to record a central Codex login push");
        }
    }
    Ok(())
}

/// The live vessel an access record's crew is aboard, by the access key [`frozen_crew`] composes.
fn record_vessel<'a>(vessels: &'a [ResourceObject<Vessel>], record: &ResourceObject<AccessDelivery>) -> Option<&'a ResourceObject<Vessel>> {
    let convoy = record.metadata.labels.get(CONVOY_LABEL)?;
    vessels.iter().find(|vessel| {
        vessel.metadata.deletion_timestamp.is_none()
            && &vessel.spec.convoy_ref == convoy
            && access_names_vessel(&record.spec.access, convoy, &vessel.spec.vessel_name)
    })
}

/// Whether an access key `{project}/{convoy}/{vessel}/{role}` names this convoy's vessel,
/// whatever the project and role hold.
fn access_names_vessel(access: &str, convoy: &str, vessel_name: &str) -> bool {
    access.contains(&format!("/{convoy}/{vessel_name}/"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };

    use flotilla_environment::environment::EnvironmentTool;
    use flotilla_provider_runtime::ProcessCommandRunner;

    use super::*;

    /// A vessel agent whose files, directories and links live in memory, and which records
    /// every call that reaches its content cache or moves an entry.
    #[derive(Default)]
    struct Recording {
        files: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
        directories: Mutex<BTreeSet<PathBuf>>,
        links: Mutex<BTreeSet<PathBuf>>,
        calls: Mutex<Vec<String>>,
        /// Refuse the next link, as an agent connection lost mid-delivery would.
        refuse_link: std::sync::atomic::AtomicBool,
    }

    impl Recording {
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().expect("calls"))
        }
    }

    #[async_trait]
    impl VesselAgent for Recording {
        async fn ready(&self) -> Result<(), String> {
            Ok(())
        }
        async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
            unreachable!("delivery forwards no sockets")
        }
        async fn push_tools(&self, _: &[EnvironmentTool]) -> Result<(), String> {
            unreachable!("delivery pushes no tools")
        }
        fn commands(&self) -> Arc<dyn CommandRunner> {
            unreachable!("skills delivery runs no commands in the vessel")
        }
        async fn writable_base(&self) -> Result<PathBuf, String> {
            Ok("/run/flotilla".into())
        }
        async fn home_base(&self) -> Result<PathBuf, String> {
            Ok("/home/crew/flotilla".into())
        }
        async fn stat(&self, path: &Path) -> Result<Option<EntryKind>, String> {
            Ok(if self.links.lock().expect("links").contains(path) {
                Some(EntryKind::Symlink)
            } else if self.directories.lock().expect("directories").contains(path) {
                Some(EntryKind::Directory)
            } else {
                self.files.lock().expect("files").contains_key(path).then_some(EntryKind::File)
            })
        }
        async fn create_dir(&self, path: &Path, _: u32) -> Result<(), String> {
            self.directories.lock().expect("directories").insert(path.into());
            Ok(())
        }
        async fn read(&self, path: &Path, _: u64) -> Result<Option<Vec<u8>>, String> {
            Ok(self.files.lock().expect("files").get(path).cloned())
        }
        async fn write(&self, path: &Path, bytes: Vec<u8>, _: u32) -> Result<(), String> {
            self.files.lock().expect("files").insert(path.into(), bytes);
            Ok(())
        }
        async fn symlink(&self, _: &Path, _: &Path) -> Result<(), String> {
            unreachable!("skills are linked into the content cache, not to a path")
        }
        async fn rename(&self, from: &Path, to: &Path) -> Result<(), String> {
            let mut directories = self.directories.lock().expect("directories");
            if !directories.remove(from) || !directories.insert(to.into()) {
                return Err(format!("cannot rename {} to {}", from.display(), to.display()));
            }
            self.calls.lock().expect("calls").push(format!("rename {}", from.display()));
            Ok(())
        }
        async fn push_artifact(&self, sha256: &str, _: &Path, unpack: PinUnpack) -> Result<(), String> {
            self.calls.lock().expect("calls").push(format!("push {sha256} {}", unpack.as_str()));
            Ok(())
        }
        async fn link_artifact(&self, path: &Path, sha256: &str, member: &Path) -> Result<(), String> {
            if self.refuse_link.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err("the agent connection was lost".into());
            }
            // As the agent does: a link replaces anything there but a directory.
            if self.directories.lock().expect("directories").contains(path) {
                return Err(format!("{} is a directory", path.display()));
            }
            self.links.lock().expect("links").insert(path.into());
            self.calls.lock().expect("calls").push(format!("link {} {sha256}/{}", path.display(), member.display()));
            Ok(())
        }
        // Its content cache is never cleared: a link it placed still resolves.
        async fn linked_artifact(&self, path: &Path) -> Result<Option<String>, String> {
            Ok(self.links.lock().expect("links").contains(path).then(|| "linked".into()))
        }
    }

    /// A crew's home, and its skills tree in it, as the access layer declares them.
    fn skill_needs(digest: &str) -> Vec<Need> {
        vec![
            Need {
                name: NeedName::new(HOME),
                digest: ContentDigest::new("home"),
                item: Item::Home { key: RelativePath::new("agent-homes/coder").expect("key"), base: HomeBase::Durable },
            },
            Need {
                name: NeedName::new(SKILLS),
                digest: ContentDigest::new(digest),
                item: Item::Tree {
                    home: NeedName::new(HOME),
                    path: RelativePath::new("skills").expect("path"),
                    sources: TreeSources { controller: true, ..TreeSources::default() },
                },
            },
        ]
    }

    // Behaviour (#3663): skills reach a crew's home through the two content-cache calls
    // harness layers use: the archive is pushed by its digest and the home's `skills` is
    // linked to it. A repeat whose record already names the tree's digest pushes nothing;
    // a real `skills` directory left by an older delivery is moved aside once, before the
    // first link, and never again. A link that fails after the move leaves the access
    // undelivered (no harness starts), and the retry links without moving anything.
    #[tokio::test]
    async fn skills_are_pushed_by_digest_and_linked_and_a_legacy_directory_is_moved_aside_once() {
        let cache = tempfile::tempdir().expect("cache");
        let trees = Arc::new(SkillTrees::new(
            Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(Arc::new(ProcessCommandRunner))),
            cache.path().to_path_buf(),
        ));
        let tree = SkillTree::resolve(None, &[]).await.expect("an empty selection needs no supply");
        let archive = trees.crew_archive(&tree, None).await.expect("archive");
        let delivery: Arc<dyn TreeDelivery> = Arc::new(SkillsDelivery { trees, tokens: None, tree: tree.clone() });
        let trees = BTreeMap::from([(NeedName::new(SKILLS), delivery)]);
        let agent = Recording::default();
        let skills = Path::new("/home/crew/flotilla/agent-homes/coder/skills");
        agent.directories.lock().expect("directories").insert(skills.into());
        let run = |digest: String| {
            let (needs, trees, agent) = (skill_needs(&digest), &trees, &agent);
            async move { deliver(&needs, trees, &place_mechanisms(false), &CostTable::default(), agent).await }
        };
        let (push, link) = (format!("push {} tar.gz", archive.sha256), format!("link {} {}/", skills.display(), archive.sha256));

        agent.refuse_link.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = run(tree.digest()).await.expect_err("the link failed");
        assert!(!error.is_permanent(), "{error}");
        assert_eq!(agent.calls(), [push.clone(), format!("rename {}", skills.display())], "moved aside, not yet linked");
        run(tree.digest()).await.expect("retried");
        assert_eq!(agent.calls(), [push.clone(), link.clone()], "the retry links; nothing is left to move aside");
        run(tree.digest()).await.expect("kept");
        assert_eq!(agent.calls(), Vec::<String>::new(), "the record already names this digest");
        run(format!("{}-reselected", tree.digest())).await.expect("redelivered");
        assert_eq!(agent.calls(), [push, link], "a later delivery finds a link, not a directory");
    }

    // Behaviour (#3663): a refusal on positive evidence reaches the executor as permanent.
    // Here a private source's credential cannot be minted on a controller without a
    // credential store, so nothing is pushed and retrying cannot help.
    #[tokio::test]
    async fn a_refused_skill_tree_is_a_permanent_step_failure() {
        let root = tempfile::tempdir().expect("root");
        let supply = root.path().join("supply");
        std::fs::create_dir_all(&supply).expect("supply");
        let (repository, revision) = ("https://github.com/example/private-skills.git", "2".repeat(40));
        std::fs::write(
            supply.join(".flotilla-sources.json"),
            serde_json::json!({"schema_version": 5, "sources": [{"name": "private", "repository": repository, "revision": revision, "credential": "reader"}]})
                .to_string(),
        )
        .expect("manifest");
        let selected = flotilla_resources::SkillCatalogEntry {
            source: "private".into(),
            repository: repository.into(),
            revision,
            name: "alpha".into(),
            path: "skills/alpha".into(),
        };
        let tree = SkillTree::resolve(Some(&supply), &[selected]).await.expect("authorized by the supply");
        let trees = Arc::new(SkillTrees::new(
            Arc::new(flotilla_vcs::vcs::FlotillaVcs::object_cache(Arc::new(ProcessCommandRunner))),
            root.path().join("cache"),
        ));
        let digest = tree.digest();
        let delivery: Arc<dyn TreeDelivery> = Arc::new(SkillsDelivery { trees, tokens: None, tree });
        let agent = Recording::default();
        let error = deliver(
            &skill_needs(&digest),
            &BTreeMap::from([(NeedName::new(SKILLS), delivery)]),
            &place_mechanisms(false),
            &CostTable::default(),
            &agent,
        )
        .await
        .expect_err("refused");
        assert!(error.is_permanent() && error.to_string().contains("credential reader"), "{error}");
        assert_eq!(agent.calls(), Vec::<String>::new());
    }

    #[test]
    fn an_access_key_names_its_vessel_whatever_the_project_and_role_hold() {
        assert!(access_names_vessel("org/repo/convoy-1/implement/review/lead", "convoy-1", "implement"));
        assert!(!access_names_vessel("org/repo/convoy-1/implement-2/coder", "convoy-1", "implement"));
        assert!(!access_names_vessel("org/repo/convoy-2/implement/coder", "convoy-1", "implement"));
    }
}
