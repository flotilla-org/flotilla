//! The pure planner: needs, the provider's declared mechanisms and what has
//! already landed in; a plan choosing one mechanism per need out.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use crate::vocabulary::{
    Access, ContentDigest, FetchBy, HostReach, Item, Liveness, Mechanism, Need, NeedName, ProviderMechanisms, Visibility,
};

/// The relative cost of each mechanism: a policy table, not code. The planner
/// picks the cheapest mechanism that meets a need's requirements; a tie goes to
/// the earlier candidate (host paths in declaration order, then fetch, then push).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostTable {
    pub host_path_live: u32,
    pub host_path_snapshot: u32,
    pub agent_fetch_url: u32,
    pub agent_fetch_digest: u32,
    pub agent_push: u32,
}

impl Default for CostTable {
    fn default() -> Self {
        Self { host_path_live: 0, host_path_snapshot: 1, agent_fetch_url: 2, agent_fetch_digest: 2, agent_push: 3 }
    }
}

impl CostTable {
    fn cost(&self, mechanism: &Mechanism) -> u32 {
        match mechanism {
            Mechanism::HostPath { liveness: Liveness::Live, .. } => self.host_path_live,
            Mechanism::HostPath { liveness: Liveness::Snapshot, .. } => self.host_path_snapshot,
            Mechanism::AgentFetch { by: FetchBy::Url } => self.agent_fetch_url,
            Mechanism::AgentFetch { by: FetchBy::Digest } => self.agent_fetch_digest,
            Mechanism::AgentPush => self.agent_push,
        }
    }
}

/// What landed for one need: its digest, the mechanism that delivered it, and for a
/// host-path tree where the guest saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    pub digest: ContentDigest,
    pub mechanism: Mechanism,
    pub guest_source: Option<PathBuf>,
}

/// Whether a step delivers or keeps what already landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Deliver,
    Keep,
}

/// One need lowered to one mechanism.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub need: NeedName,
    pub digest: ContentDigest,
    pub mechanism: Mechanism,
    pub action: Action,
    /// For a tree reached through a host path: where the guest sees it.
    pub guest_source: Option<PathBuf>,
}

/// Steps in execution order: every home before the trees placed in it, and within
/// homes and within trees in need-name order, whatever order the needs were given in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
}

impl Plan {
    /// What has landed once every step has run, for the access's status.
    pub fn landed(&self) -> BTreeMap<NeedName, Landed> {
        self.steps
            .iter()
            .map(|step| {
                let landed = Landed { digest: step.digest.clone(), mechanism: step.mechanism, guest_source: step.guest_source.clone() };
                (step.need.clone(), landed)
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    DuplicateNeed(NeedName),
    /// A tree names a need that is absent or is not a home.
    NoHome {
        tree: NeedName,
        home: NeedName,
    },
    /// No declared or mandatory mechanism can supply the need's content.
    NoMechanism(NeedName),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateNeed(need) => write!(f, "need `{need}` is declared twice"),
            Self::NoHome { tree, home } => write!(f, "tree `{tree}` is placed in `{home}`, which is not a home need"),
            Self::NoMechanism(need) => write!(f, "no mechanism can supply need `{need}`"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Lower every need to the cheapest mechanism that meets its requirements.
///
/// - A home is writable: a read-write host path over the target's bases when they
///   are host storage, otherwise agent push creates it in the vessel.
/// - A tree is read-only: a read-only host path whose directory contains the tree's
///   host copy, agent fetch when it has a URL, agent fetch by digest when its digest
///   sources are public and the vessel reaches public sources, agent push when the
///   controller holds it (#3956 ruling: a private or unreachable artifact is fetched
///   where its authority allows and pushed).
/// - A need whose digest already landed through the chosen mechanism, at the same
///   guest path, is kept, unless its home is delivered again. `delivered` holds only
///   what is still there: the executor drops a record whose content is missing
///   aboard (#4088).
///
/// Every tree's home is validated before any mechanism is chosen.
pub fn plan(
    needs: &[Need],
    mechanisms: &ProviderMechanisms,
    delivered: &BTreeMap<NeedName, Landed>,
    costs: &CostTable,
) -> Result<Plan, PlanError> {
    let mut by_name = BTreeMap::new();
    for need in needs {
        if by_name.insert(need.name.clone(), need).is_some() {
            return Err(PlanError::DuplicateNeed(need.name.clone()));
        }
    }
    for need in by_name.values() {
        if let Item::Tree { home, .. } = &need.item {
            if !matches!(by_name.get(home).map(|need| &need.item), Some(Item::Home { .. })) {
                return Err(PlanError::NoHome { tree: need.name.clone(), home: home.clone() });
            }
        }
    }
    let (homes, trees): (Vec<&Need>, Vec<&Need>) = by_name.values().partition(|need| matches!(need.item, Item::Home { .. }));
    let mut steps = Vec::with_capacity(needs.len());
    let mut delivered_homes = BTreeSet::new();
    for need in homes.into_iter().chain(trees) {
        let candidates = candidates(need, mechanisms);
        let (mechanism, guest_source) = candidates
            .into_iter()
            .enumerate()
            .min_by_key(|(index, (mechanism, _))| (costs.cost(mechanism), *index))
            .map(|(_, candidate)| candidate)
            .ok_or_else(|| PlanError::NoMechanism(need.name.clone()))?;
        let home_redelivered = match &need.item {
            Item::Home { .. } => false,
            Item::Tree { home, .. } => delivered_homes.contains(home),
        };
        let landed = delivered
            .get(&need.name)
            .is_some_and(|landed| landed.digest == need.digest && landed.mechanism == mechanism && landed.guest_source == guest_source);
        let action = if landed && !home_redelivered { Action::Keep } else { Action::Deliver };
        if action == Action::Deliver && matches!(need.item, Item::Home { .. }) {
            delivered_homes.insert(need.name.clone());
        }
        steps.push(Step { need: need.name.clone(), digest: need.digest.clone(), mechanism, action, guest_source });
    }
    Ok(Plan { steps })
}

/// Every mechanism that meets the need's requirements, in tie-break order.
fn candidates(need: &Need, mechanisms: &ProviderMechanisms) -> Vec<(Mechanism, Option<PathBuf>)> {
    let mut candidates = Vec::new();
    match &need.item {
        Item::Home { .. } => {
            for offer in &mechanisms.host_paths {
                if offer.reach == HostReach::WritableBase && offer.access == Access::ReadWrite {
                    candidates.push((Mechanism::HostPath { access: offer.access, liveness: offer.liveness }, None));
                }
            }
            candidates.push((Mechanism::AgentPush, None));
        }
        Item::Tree { sources, .. } => {
            if let Some(source) = &sources.host_path {
                for offer in &mechanisms.host_paths {
                    // A writable view would let the guest change the controller's copy.
                    if let (HostReach::Directory { host, guest }, Access::ReadOnly) = (&offer.reach, offer.access) {
                        if let Ok(rest) = source.strip_prefix(host) {
                            candidates
                                .push((Mechanism::HostPath { access: offer.access, liveness: offer.liveness }, Some(guest.join(rest))));
                        }
                    }
                }
            }
            if sources.url {
                candidates.push((Mechanism::AgentFetch { by: FetchBy::Url }, None));
            }
            if sources.digest == Some(Visibility::Public) && mechanisms.reach.vessel_public {
                candidates.push((Mechanism::AgentFetch { by: FetchBy::Digest }, None));
            }
            if sources.controller {
                candidates.push((Mechanism::AgentPush, None));
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests;
