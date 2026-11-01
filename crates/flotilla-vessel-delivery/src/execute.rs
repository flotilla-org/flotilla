//! The one executor: runs a plan's steps through the vessel agent alone.
//!
//! Every step is keyed by content digest. What landed is written beside the
//! content, in each home's delivery record, as soon as a step completes, so a
//! re-run plans `Keep` for it and does nothing. A replaced vessel has no record
//! and receives everything again.
//!
//! A tree whose content is an entry of the vessel agent's content store
//! ([`Delivery::Stored`]) is linked by the executor itself, and its record names
//! the entry's digest. A home outlives the vessels it is delivered on, and the
//! store collects an entry once no live vessel links it (#4088), so such a record
//! is kept only while the store still holds that exact entry, and keeping it
//! places the link again, so the use is recorded under this vessel's agent.

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
    vocabulary::{ContentDigest, HomeBase, Item, Mechanism, Need, NeedName, ProviderMechanisms, RelativePath},
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
}

/// How a tree whose content lands in the vessel agent's content store gets there.
#[async_trait]
pub trait StoreDelivery: Send + Sync {
    /// Make the tree's content an entry of the agent's store through `mechanism`,
    /// and name it. `target` is the tree's place, which the executor then links to
    /// the entry: anything there a link cannot replace is moved aside first.
    async fn store(&self, mechanism: Mechanism, target: &Path, agent: &dyn VesselAgent) -> Result<StoreEntry, StepFailure>;
}

/// How a tree's content lands aboard, declared by its delivery (#4088).
#[derive(Clone)]
pub enum Delivery {
    /// Written at its place, beside its home's record, and gone with it.
    Placed(Arc<dyn TreeDelivery>),
    /// An entry of the vessel agent's content store, linked at its place by the
    /// executor. The store outlives one vessel, and its entries outlive no link.
    Stored(Arc<dyn StoreDelivery>),
}

/// One entry of the vessel agent's content store, and where a tree links it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreEntry {
    pub sha256: ContentDigest,
    /// Where under the tree's place the entry is linked; the place itself when `None`.
    pub link: Option<RelativePath>,
}

impl StoreEntry {
    fn link_at(&self, target: &Path) -> PathBuf {
        self.link.as_ref().map_or_else(|| target.to_path_buf(), |link| link.under(target))
    }
}

/// What a home's record holds for one need: what landed, and for a stored tree,
/// the store entry it is linked to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    landed: Landed,
    stored: Option<StoreEntry>,
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
    trees: &BTreeMap<NeedName, Delivery>,
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
    let by_name = needs.iter().map(|need| (&need.name, need)).collect::<BTreeMap<_, _>>();
    // A stored tree counts as landed only while the store holds its recorded entry. A
    // record that names no entry predates stored trees naming theirs.
    let mut landed = BTreeMap::new();
    let mut entries = BTreeMap::new();
    for (name, recorded) in read.values().flatten() {
        let asked = by_name.get(name).is_some_and(|need| need.digest == recorded.landed.digest);
        if asked && matches!(trees.get(name), Some(Delivery::Stored(_))) {
            let held = match &recorded.stored {
                Some(entry) => agent.has_artifact(entry.sha256.as_str()).await.map_err(|error| DeliveryError::Step {
                    need: name.clone(),
                    error,
                    permanent: false,
                })?,
                None => false,
            };
            if !held {
                tracing::info!(need = %name, digest = %recorded.landed.digest, "the store no longer holds a recorded entry; delivering it again");
                continue;
            }
            entries.insert(name.clone(), recorded.stored.clone().expect("a held entry is recorded"));
        }
        landed.insert(name.clone(), recorded.landed.clone());
    }
    // The planner refuses a tree whose home is not a home need, so every step's home is in `homes`.
    let plan = plan(needs, mechanisms, &landed, costs).map_err(DeliveryError::Plan)?;
    if let Some(step) =
        plan.steps.iter().find(|step| step.guest_source.as_ref().is_some_and(|source| !recordable(&source.to_string_lossy())))
    {
        return Err(DeliveryError::Unrecordable(step.need.clone()));
    }
    let home_of = |need: &Need| match &need.item {
        Item::Home { .. } => need.name.clone(),
        Item::Tree { home, .. } => home.clone(),
    };
    let landed_by =
        |step: &Step| Landed { digest: step.digest.clone(), mechanism: step.mechanism, guest_source: step.guest_source.clone() };
    // Each record starts from what is kept, and gains each delivery as it completes. A
    // kept stored tree is linked again, so this vessel's agent records the use: a
    // collection after the vessel that first linked it retires leaves the entry be.
    let mut records =
        homes.keys().map(|home| (home.clone(), BTreeMap::new())).collect::<BTreeMap<NeedName, BTreeMap<NeedName, Recorded>>>();
    for step in plan.steps.iter().filter(|step| step.action == Action::Keep) {
        let need = by_name[&step.need];
        let stored = entries.remove(&need.name);
        if let (Some(entry), Item::Tree { home, path, .. }) = (&stored, &need.item) {
            link(entry, &path.under(&homes[home]), agent).await.map_err(|error| DeliveryError::Step {
                need: need.name.clone(),
                error,
                permanent: false,
            })?;
        }
        let recorded = Recorded { landed: landed_by(step), stored };
        records.get_mut(&home_of(need)).expect("every home has a record").insert(step.need.clone(), recorded);
    }
    let mut written = read;
    for step in plan.steps.iter().filter(|step| step.action == Action::Deliver) {
        let need = by_name[&step.need];
        let home = home_of(need);
        let failed = |error| DeliveryError::Step { need: need.name.clone(), error, permanent: false };
        let refused = |StepFailure { error, permanent }| DeliveryError::Step { need: need.name.clone(), error, permanent };
        let mut stored = None;
        match &need.item {
            Item::Home { .. } => agent.create_dir(&homes[&home], HOME_MODE).await.map_err(failed)?,
            Item::Tree { path, .. } => {
                let target = path.under(&homes[&home]);
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
                match &trees[&need.name] {
                    Delivery::Placed(tree) => {
                        tree.deliver(step.mechanism, step.guest_source.as_deref(), &target, agent).await.map_err(refused)?
                    }
                    Delivery::Stored(tree) => {
                        let entry = tree.store(step.mechanism, &target, agent).await.map_err(refused)?;
                        let link_text = entry.link.as_ref().map(|link| link.as_path().to_string_lossy().into_owned()).unwrap_or_default();
                        if !recordable(entry.sha256.as_str()) || !recordable(&link_text) {
                            return Err(DeliveryError::Unrecordable(need.name.clone()));
                        }
                        link(&entry, &target, agent).await.map_err(failed)?;
                        stored = Some(entry);
                    }
                }
            }
        }
        let record = records.get_mut(&home).expect("every home has a record");
        record.insert(need.name.clone(), Recorded { landed: landed_by(step), stored });
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

/// Point the tree placed at `target` to its store entry, recording the use under this
/// vessel's agent.
async fn link(entry: &StoreEntry, target: &Path, agent: &dyn VesselAgent) -> Result<(), String> {
    agent.link_artifact(&entry.link_at(target), entry.sha256.as_str(), Path::new("")).await
}

/// A record field holds no tab or line break.
fn recordable(field: &str) -> bool {
    !field.contains(['\t', '\n', '\r'])
}

/// One line per need: name, digest, mechanism and, for a host-path tree, its guest
/// path, then for a stored tree its entry's digest and where under its place it is
/// linked (empty for the place itself), tab-separated.
fn encode_record(record: &BTreeMap<NeedName, Recorded>) -> String {
    record
        .iter()
        .map(|(need, Recorded { landed, stored })| {
            let source = landed.guest_source.as_ref().map(|source| source.display().to_string()).unwrap_or_default();
            let line = format!("{need}\t{}\t{}\t{source}", landed.digest, landed.mechanism);
            match stored {
                Some(entry) => {
                    let link = entry.link.as_ref().map(|link| link.as_path().display().to_string()).unwrap_or_default();
                    format!("{line}\t{}\t{link}\n", entry.sha256)
                }
                None => format!("{line}\n"),
            }
        })
        .collect()
}

/// A record line that does not parse is ignored, so its need is delivered again. A
/// line written before stored trees named their entry (#4088) reads with none, so a
/// stored tree it names is delivered again once.
fn decode_record(text: &str) -> BTreeMap<NeedName, Recorded> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let parsed = (|| {
                let (need, digest, mechanism) = (fields.next()?, fields.next()?, fields.next()?.parse().ok()?);
                let guest_source = fields.next().filter(|source| !source.is_empty()).map(PathBuf::from);
                let stored = match fields.next() {
                    Some(sha256) => {
                        let link = fields.next().filter(|link| !link.is_empty()).map(RelativePath::new).transpose().ok()?;
                        Some(StoreEntry { sha256: ContentDigest::new(sha256), link })
                    }
                    None => None,
                };
                let landed = Landed { digest: ContentDigest::new(digest), mechanism, guest_source };
                Some((NeedName::new(need), Recorded { landed, stored }))
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
