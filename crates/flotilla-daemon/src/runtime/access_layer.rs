//! The access layer of vessel delivery (#3964, #3956 Q4): per crew, before its harness
//! starts, the crew's harness home, skills and account material are delivered aboard its
//! vessel through the vessel agent, the same way for every environment kind.
//!
//! The launch path builds one request per crew: an opaque access key, the harness, the
//! home (its key under the target's writable base), the crew's resolved skill tree and its
//! account material. The planner lowers each need to a mechanism the place offers, and the
//! one executor runs the plan. Launch then points the harness at the home the delivery
//! returned; nothing derives the home again.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use async_trait::async_trait;
use flotilla_credentials::{AgentMaterialRegistry, HarnessHome, HarnessLogin, SkillTree};
use flotilla_environment::environment::VesselAgent;
use flotilla_resources::{
    account_access::{requester_home, AccessName},
    Convoy, Environment, ResourceError, Vessel,
};
use flotilla_vessel_delivery::{
    deliver, Access, AccessKey, ContentDigest, CostTable, DeliveryError, HostPathOffer, HostReach, Item, Liveness, Mechanism, Need,
    NeedName, Plan, ProviderMechanisms, Reachability, RelativePath, TreeDelivery, TreeSources,
};

use self::status::AccessStatus;

pub(super) mod status;

use super::account_access::{AccountLaunch, BoundAccount};
use super::credentials::agent_material_environment;
use super::state::ControllerRuntimeState;

const HOME: &str = "home";
const SKILLS: &str = "skills";
const ACCOUNT: &str = "account";
const LOGIN: &str = "login";
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
}

/// Deliver the crew's access layer aboard its vessel through `agent`, and return the
/// variables that point its harness at what landed: its home, and its account material.
/// `delivered` is the launch environment credential delivery resolved, which names the
/// vessel's delivered harness homes.
pub(super) async fn deliver_access(
    state: &Arc<ControllerRuntimeState>,
    launch: &AccessLaunch<'_>,
    agent: Arc<dyn VesselAgent>,
    delivered: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let material = state.agent_material.as_ref().ok_or("vessel delivery requires the agent material registry")?;
    let Some(harness) = material.harness_home(launch.adapter) else { return Ok(Vec::new()) };
    let crew = frozen_crew(state, &launch.account, launch.vessel_ref).await?;
    let status = AccessStatus::read(state, &launch.account, &crew.access, launch.adapter).await?;
    status.due(state)?;
    match deliver_crew(state, launch, material, &harness, crew, agent, delivered).await {
        Ok((plan, home, environment)) => {
            status.delivered(state, &plan, &home).await?;
            Ok(environment)
        }
        Err(Failed { need, error }) => {
            status.failed(state, need, &error).await?;
            Err(error)
        }
    }
}

/// A delivery that failed at one need: the need it was preparing or delivering.
struct Failed {
    need: String,
    error: String,
}

fn failed_at(need: &str) -> impl FnOnce(String) -> Failed + '_ {
    move |error| Failed { need: need.to_string(), error }
}

async fn deliver_crew(
    state: &Arc<ControllerRuntimeState>,
    launch: &AccessLaunch<'_>,
    material: &Arc<AgentMaterialRegistry>,
    harness: &HarnessHome,
    crew: FrozenCrew,
    agent: Arc<dyn VesselAgent>,
    delivered: &[(String, String)],
) -> Result<(Plan, String, Vec<(String, String)>), Failed> {
    let access = AccessKey::new(crew.access);
    let home_key = requester_home(Path::new(""), launch.account.convoy, launch.account.role, harness.name);
    let mut needs = vec![Need {
        name: NeedName::new(HOME),
        digest: ContentDigest::new(home_key.display().to_string()),
        item: Item::Home { key: RelativePath::new(home_key).map_err(failed_at(HOME))? },
    }];
    let mut trees: BTreeMap<NeedName, Arc<dyn TreeDelivery>> = BTreeMap::new();
    let mut tree =
        |needs: &mut Vec<Need>, name: &str, digest: String, path: &str, sources: TreeSources, delivery: Arc<dyn TreeDelivery>| {
            needs.push(Need {
                name: NeedName::new(name),
                digest: ContentDigest::new(digest),
                item: Item::Tree { home: NeedName::new(HOME), path: RelativePath::new(path).expect("a constant relative path"), sources },
            });
            trees.insert(NeedName::new(name), delivery);
        };
    let skills = material.skill_tree(&crew.skills).await.map_err(failed_at(SKILLS))?;
    tree(
        &mut needs,
        SKILLS,
        skills.digest(),
        "skills",
        TreeSources { url: true, ..TreeSources::default() },
        Arc::new(SkillsDelivery { state: state.clone(), material: material.clone(), tree: skills }),
    );
    let bound = match launch.access {
        Some(access) => Some(Arc::new(BoundAccount::resolve(state, &launch.account, access).await.map_err(failed_at(ACCOUNT))?)),
        None => None,
    };
    if let Some(bound) = &bound {
        tree(
            &mut needs,
            ACCOUNT,
            bound.digest(state, &launch.account).await.map_err(failed_at(ACCOUNT))?,
            flotilla_agent::account_access::CLAUDE_OAUTH_TOKEN_FILE,
            TreeSources { controller: true, ..TreeSources::default() },
            Arc::new(AccountDelivery { state: state.clone(), launch: OwnedAccountLaunch::of(&launch.account), bound: bound.clone() }),
        );
    }
    let base = agent.writable_base().await.map_err(failed_at(HOME))?;
    // A container's harness defaults (its contained Codex home) apply only inside it; an
    // SSH host's harness homes are what credential delivery put under its base.
    let resolved = if launch.account.agentless {
        delivered.to_vec()
    } else {
        let declared = declared_environment(state, &launch.account).await.map_err(failed_at(LOGIN))?;
        agent_material_environment(material, &[launch.adapter.to_string()].into(), &declared, delivered.iter().cloned())
            .map_err(failed_at(LOGIN))?
    };
    if let Some(login) = material.harness_login(launch.adapter, &resolved, &base) {
        tree(
            &mut needs,
            LOGIN,
            login.digest(),
            "auth.json",
            TreeSources { controller: true, ..TreeSources::default() },
            Arc::new(LoginDelivery(login)),
        );
    }
    let place = launch.account.env_ref;
    let delivered = deliver(&needs, &trees, &place_mechanisms(launch.account.agentless), &CostTable::default(), agent.as_ref())
        .await
        .map_err(|error| Failed {
            need: match &error {
                DeliveryError::Step { need, .. } | DeliveryError::NoDelivery(need) | DeliveryError::Unrecordable(need) => need.to_string(),
                DeliveryError::Plan(_) | DeliveryError::Agent(_) => PLAN.to_string(),
            },
            error: format!("deliver access `{access}` aboard `{place}`: {error}"),
        })?;
    for step in &delivered.plan.steps {
        tracing::debug!(%access, place, need = %step.need, mechanism = %step.mechanism, action = ?step.action, "vessel delivery");
    }
    let home = delivered
        .homes
        .get(&NeedName::new(HOME))
        .ok_or_else(|| Failed {
            need: HOME.to_string(),
            error: format!("deliver access `{access}` aboard `{place}`: no home was delivered"),
        })?
        .display()
        .to_string();
    let mut environment = vec![(harness.variable.to_string(), home.clone())];
    if let Some(bound) = bound {
        environment.retain(|(name, _)| !bound.environment(&home).iter().any(|(bound, _)| bound == name));
        environment.extend(bound.environment(&home));
    }
    Ok((delivered.plan, home, environment))
}

/// What a place offers vessel delivery beyond the mandatory agent fetch and push. An SSH
/// host's writable base is the host's own durable storage; a container's is not, so its
/// homes are created through the agent. Both kinds in use reach public sources: neither
/// declares an egress policy, and their skills are already fetched aboard. Step 4 of
/// #3956 moves this declaration onto each provider instance.
fn place_mechanisms(agentless: bool) -> ProviderMechanisms {
    let host_paths = if agentless {
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

/// The crew's skill tree, fetched by the vessel at each source's pinned revision.
struct SkillsDelivery {
    state: Arc<ControllerRuntimeState>,
    material: Arc<AgentMaterialRegistry>,
    tree: SkillTree,
}

#[async_trait]
impl TreeDelivery for SkillsDelivery {
    async fn deliver(&self, mechanism: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        if mechanism != (Mechanism::AgentFetch { by: flotilla_vessel_delivery::FetchBy::Url }) {
            return Err(format!("skills cannot be delivered by {mechanism}"));
        }
        let runner = agent.commands();
        let mut tokens = BTreeMap::new();
        let requests = self.tree.credentials();
        if !requests.is_empty() {
            let store = self.state.credential_store.as_ref().ok_or("skill sources need the host-local credential store")?;
            // `skill_tree` refused a credential over two repositories, so one token per
            // credential serves every source that names it.
            let mut minted: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();
            for request in requests {
                let token = match minted.get(&request.credential) {
                    Some(token) => token.clone(),
                    None => {
                        let token =
                            store.prepare_skill_source(&request.credential, &request.repository, &*runner).await.map_err(|error| {
                                format!("skill source {} credential {} mint failed: {error}", request.source, request.credential)
                            })?;
                        minted.insert(request.credential.clone(), token.clone());
                        token
                    }
                };
                tokens.insert(request.source, token);
            }
        }
        self.material.stage_skill_tree(&self.tree, target, &tokens, &*runner).await
    }
}

/// An owned copy of the launch's identity, for deliveries the executor holds.
struct OwnedAccountLaunch {
    namespace: String,
    convoy: String,
    role: String,
    env_ref: String,
    agentless: bool,
}

impl OwnedAccountLaunch {
    fn of(launch: &AccountLaunch<'_>) -> Self {
        Self {
            namespace: launch.namespace.into(),
            convoy: launch.convoy.into(),
            role: launch.role.into(),
            env_ref: launch.env_ref.into(),
            agentless: launch.agentless,
        }
    }

    fn borrow(&self) -> AccountLaunch<'_> {
        AccountLaunch {
            namespace: &self.namespace,
            convoy: &self.convoy,
            role: &self.role,
            env_ref: &self.env_ref,
            agentless: self.agentless,
        }
    }
}

/// The crew's bound account material (#3907), written into its home.
struct AccountDelivery {
    state: Arc<ControllerRuntimeState>,
    launch: OwnedAccountLaunch,
    bound: Arc<BoundAccount>,
}

#[async_trait]
impl TreeDelivery for AccountDelivery {
    async fn deliver(&self, _: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        let home = target.parent().ok_or("account material needs its home")?.display().to_string();
        self.bound.deliver(&self.state, &self.launch.borrow(), &home, &*agent.commands()).await
    }
}

/// The crew's harness login, linked into its home.
struct LoginDelivery(HarnessLogin);

#[async_trait]
impl TreeDelivery for LoginDelivery {
    async fn deliver(&self, _: Mechanism, _: Option<&Path>, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
        self.0.link(target.parent().ok_or("a login needs its home")?, agent).await
    }
}
