//! The one executor: runs a plan's steps through the vessel agent alone.
//!
//! Every step is keyed by content digest. What landed is written beside the
//! content, in each home's delivery record, as soon as a step completes, so a
//! re-run plans `Keep` for it and does nothing. A replaced vessel has no record
//! and receives everything again.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_environment::environment::VesselAgent;

use crate::{
    plan::{plan, Action, CostTable, Landed, Plan, PlanError, Step},
    vocabulary::{ContentDigest, HomeBase, Item, Mechanism, Need, NeedName, ProviderMechanisms},
};

/// Each home's record of what landed in it, read back to plan a re-run.
pub const DELIVERY_RECORD: &str = ".flotilla-delivery";
const HOME_MODE: u32 = 0o700;
const RECORD_MODE: u32 = 0o600;
/// A record names a handful of needs; anything larger is not one.
const RECORD_LIMIT: u64 = 64 * 1024;

/// How one tree's content reaches its place, for whichever mechanism the plan chose.
#[async_trait]
pub trait TreeDelivery: Send + Sync {
    /// Place the tree at `target` through `mechanism`. `guest_source` is where the
    /// guest sees a host-path tree. Replaces what an earlier digest placed there.
    async fn deliver(
        &self,
        mechanism: Mechanism,
        guest_source: Option<&Path>,
        target: &Path,
        agent: &dyn VesselAgent,
    ) -> Result<(), StepFailure>;

    /// Whether the tree lands as links into the agent's content store. Such a
    /// tree is kept only while one of those links is still a use: a shared
    /// store collects an entry no live vessel uses (#4087), and a home that
    /// outlived its vessel then keeps a record of content that is gone.
    fn linked_from_store(&self) -> bool {
        false
    }
}

/// Why a tree's delivery failed, and whether trying the same content again can help.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepFailure {
    pub error: String,
    /// Only positive evidence, such as an authority refusal or a policy violation,
    /// makes a failure permanent; anything else is retried.
    pub permanent: bool,
}

impl StepFailure {
    pub fn permanent(error: impl Into<String>) -> Self {
        Self { error: error.into(), permanent: true }
    }
}

impl From<String> for StepFailure {
    fn from(error: String) -> Self {
        Self { error, permanent: false }
    }
}

impl From<&str> for StepFailure {
    fn from(error: &str) -> Self {
        error.to_string().into()
    }
}

/// What one access delivered: where each home is aboard this vessel, and the
/// plan that put everything there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub homes: BTreeMap<NeedName, PathBuf>,
    pub plan: Plan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    Plan(PlanError),
    /// A tree need has no delivery for its content.
    NoDelivery(NeedName),
    /// A need's name, digest or guest path holds a tab or line break, which its record
    /// cannot hold.
    Unrecordable(NeedName),
    /// A step, or reading or writing a delivery record, failed.
    Step {
        need: NeedName,
        error: String,
        /// Retrying the same content cannot help.
        permanent: bool,
    },
    Agent(String),
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan(error) => write!(f, "plan vessel delivery: {error}"),
            Self::NoDelivery(need) => write!(f, "need `{need}` has no delivery"),
            Self::Unrecordable(need) => write!(f, "need `{need}` has a name or digest its delivery record cannot hold"),
            Self::Step { need, error, .. } => write!(f, "deliver `{need}`: {error}"),
            Self::Agent(error) => write!(f, "vessel agent: {error}"),
        }
    }
}

impl std::error::Error for DeliveryError {}

impl DeliveryError {
    /// Whether retrying the same delivery cannot help.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Step { permanent: true, .. })
    }
}

/// Plan the needs against what already landed aboard, then run the plan.
///
/// Each home belongs to one access, and a caller delivers one access at a time: a
/// crew's launch is serialised by its own session's reconciler. Two concurrent
/// deliveries into the same home could each lose the other's record entries.
pub async fn deliver(
    needs: &[Need],
    trees: &BTreeMap<NeedName, Arc<dyn TreeDelivery>>,
    mechanisms: &ProviderMechanisms,
    costs: &CostTable,
    agent: &dyn VesselAgent,
) -> Result<Delivered, DeliveryError> {
    if let Some(need) = needs.iter().find(|need| !recordable(need.name.as_str()) || !recordable(need.digest.as_str())) {
        return Err(DeliveryError::Unrecordable(need.name.clone()));
    }
    if let Some(need) = needs.iter().find(|need| matches!(need.item, Item::Tree { .. }) && !trees.contains_key(&need.name)) {
        return Err(DeliveryError::NoDelivery(need.name.clone()));
    }
    let mut bases = BTreeMap::new();
    let mut homes = BTreeMap::new();
    for need in needs {
        let Item::Home { key, base } = &need.item else { continue };
        let resolved = match bases.get(base) {
            Some(resolved) => resolved,
            None => {
                let resolved = match base {
                    HomeBase::Durable => agent.home_base().await,
                    HomeBase::Runtime => agent.writable_base().await,
                }
                .map_err(DeliveryError::Agent)?;
                bases.entry(*base).or_insert(resolved)
            }
        };
        homes.insert(need.name.clone(), key.under(resolved));
    }
    let mut read = BTreeMap::new();
    for (name, home) in &homes {
        let record = agent.read(&home.join(DELIVERY_RECORD), RECORD_LIMIT).await.map_err(|error| DeliveryError::Step {
            need: name.clone(),
            error,
            permanent: false,
        })?;
        read.insert(name.clone(), record.map(|bytes| decode_record(&String::from_utf8_lossy(&bytes))).unwrap_or_default());
    }
    let mut landed: BTreeMap<NeedName, Landed> = read.values().flatten().map(|(need, landed)| (need.clone(), landed.clone())).collect();
    forget_collected(needs, trees, &homes, &mut landed, agent).await?;
    // The planner refuses a tree whose home is not a home need, so every step's home is in `homes`.
    let plan = plan(needs, mechanisms, &landed, costs).map_err(DeliveryError::Plan)?;
    if let Some(step) =
        plan.steps.iter().find(|step| step.guest_source.as_ref().is_some_and(|source| !recordable(&source.to_string_lossy())))
    {
        return Err(DeliveryError::Unrecordable(step.need.clone()));
    }
    let by_name = needs.iter().map(|need| (&need.name, need)).collect::<BTreeMap<_, _>>();
    let home_of = |need: &Need| match &need.item {
        Item::Home { .. } => need.name.clone(),
        Item::Tree { home, .. } => home.clone(),
    };
    let landed_by =
        |step: &Step| Landed { digest: step.digest.clone(), mechanism: step.mechanism, guest_source: step.guest_source.clone() };
    // Each record starts from what is kept, and gains each delivery as it completes.
    let mut records = homes.keys().map(|home| (home.clone(), BTreeMap::new())).collect::<BTreeMap<NeedName, BTreeMap<NeedName, Landed>>>();
    for step in plan.steps.iter().filter(|step| step.action == Action::Keep) {
        records.get_mut(&home_of(by_name[&step.need])).expect("every home has a record").insert(step.need.clone(), landed_by(step));
    }
    let mut written = read;
    for step in plan.steps.iter().filter(|step| step.action == Action::Deliver) {
        let need = by_name[&step.need];
        let home = home_of(need);
        let failed = |error| DeliveryError::Step { need: need.name.clone(), error, permanent: false };
        match &need.item {
            Item::Home { .. } => agent.create_dir(&homes[&home], HOME_MODE).await.map_err(failed)?,
            Item::Tree { path, .. } => {
                let tree = &trees[&need.name];
                // Forget what landed before replacing it, so a replacement that fails
                // partway is never taken for the content the record named.
                if written.get(&home).is_some_and(|record| record.contains_key(&need.name)) {
                    let record = &records[&home];
                    agent
                        .write(&homes[&home].join(DELIVERY_RECORD), encode_record(record).into_bytes(), RECORD_MODE)
                        .await
                        .map_err(failed)?;
                    written.insert(home.clone(), record.clone());
                }
                tree.deliver(step.mechanism, step.guest_source.as_deref(), &path.under(&homes[&home]), agent)
                    .await
                    .map_err(|StepFailure { error, permanent }| DeliveryError::Step { need: need.name.clone(), error, permanent })?;
            }
        }
        let record = records.get_mut(&home).expect("every home has a record");
        record.insert(need.name.clone(), landed_by(step));
        agent.write(&homes[&home].join(DELIVERY_RECORD), encode_record(record).into_bytes(), RECORD_MODE).await.map_err(failed)?;
        written.insert(home, record.clone());
    }
    // A need no longer asked for leaves its home's record too, so the record and the plan agree.
    for (home, record) in &records {
        if written.get(home) != Some(record) {
            agent
                .write(&homes[home].join(DELIVERY_RECORD), encode_record(record).into_bytes(), RECORD_MODE)
                .await
                .map_err(|error| DeliveryError::Step { need: home.clone(), error, permanent: false })?;
        }
    }
    Ok(Delivered { homes, plan })
}

/// Drop from `landed` each tree linked from the content store that no live
/// link of the store reaches any more, so it is delivered again.
async fn forget_collected(
    needs: &[Need],
    trees: &BTreeMap<NeedName, Arc<dyn TreeDelivery>>,
    homes: &BTreeMap<NeedName, PathBuf>,
    landed: &mut BTreeMap<NeedName, Landed>,
    agent: &dyn VesselAgent,
) -> Result<(), DeliveryError> {
    let linked = needs
        .iter()
        .filter(|need| landed.contains_key(&need.name) && trees.get(&need.name).is_some_and(|tree| tree.linked_from_store()))
        .filter_map(|need| match &need.item {
            Item::Tree { home, path, .. } => homes.get(home).map(|home| (need.name.clone(), path.under(home))),
            Item::Home { .. } => None,
        })
        .collect::<Vec<_>>();
    let Some(first) = linked.first().map(|(need, _)| need.clone()) else { return Ok(()) };
    let links = agent.artifact_links().await.map_err(|error| DeliveryError::Step { need: first, error, permanent: false })?;
    for (need, target) in linked {
        if !links.iter().any(|link| link.starts_with(&target)) {
            landed.remove(&need);
        }
    }
    Ok(())
}

/// A record field holds no tab or line break.
fn recordable(field: &str) -> bool {
    !field.contains(['\t', '\n', '\r'])
}

/// One line per need: name, digest, mechanism and, for a host-path tree, its guest
/// path, tab-separated.
fn encode_record(record: &BTreeMap<NeedName, Landed>) -> String {
    record
        .iter()
        .map(|(need, landed)| {
            let source = landed.guest_source.as_ref().map(|source| source.display().to_string()).unwrap_or_default();
            format!("{need}\t{}\t{}\t{source}\n", landed.digest, landed.mechanism)
        })
        .collect()
}

/// A record line that does not parse is ignored, so its need is delivered again.
fn decode_record(text: &str) -> BTreeMap<NeedName, Landed> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let parsed = (|| {
                let (need, digest, mechanism) = (fields.next()?, fields.next()?, fields.next()?.parse().ok()?);
                let guest_source = fields.next().filter(|source| !source.is_empty()).map(PathBuf::from);
                Some((NeedName::new(need), Landed { digest: ContentDigest::new(digest), mechanism, guest_source }))
            })();
            if parsed.is_none() {
                tracing::warn!(line, "ignoring an unreadable vessel delivery record line; its need is delivered again");
            }
            parsed
        })
        .collect()
}

#[cfg(all(test, unix))]
mod tests;
