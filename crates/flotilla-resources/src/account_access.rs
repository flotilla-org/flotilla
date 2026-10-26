//! Account access bindings and their provision records (#3907).
//!
//! The decisions are #3879 (requirements resolve through grants), #3880 (managed credential,
//! authorization epoch and material generation), #3881 (provision plans per candidate environment
//! kind), #3882 (binding selection, persistence, recovery and revocation) and #3884 (the first
//! slice, which amends selection to a seeded uniform draw).
//!
//! - An [`AccountBinding`] belongs to one requester and one requirement. It is chosen once, at the
//!   requester's admission, and records no place. Only an explicit rebind or an authorization
//!   change moves it: refresh, harness restart, controller restart and vessel replacement do not.
//! - An [`AccountProvision`] is one binding at one place: the plan the registered implementation
//!   returned there, and what was last delivered there. A binding can be provisioned in several
//!   places; model access is provisioned only where the agent loop runs.
//!
//! Neither holds token bytes (account management keeps those) or proxy lease durations (#3312).
//! Core records plans and never interprets access: the family's access description, mechanism
//! and plan are the registered implementation's own text.
//!
//! The records derive both serde, for the resource-store kinds [`AccountBinding`] and
//! [`AccountProvision`] that production runs today (owner ruling 1 on #3910), and `Stored`, for the
//! generated tables the owner reviewed, which the engine store takes on at the cut-over.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Kind as AddressKind;
use flotilla_schema::{Address, KindRegistry, Records, Ref, RegistryError, State, Stored, Timestamp};

use crate::{resource::define_resource, status_patch::NoStatusPatch, ReplicationClass};
use serde::{Deserialize, Serialize};

/// A `CredentialSpec` name: the managed credential's stable identity, which grants and bindings
/// name. Interned in its own domain (`cr`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
#[serde(transparent)]
#[store(intern = "cr")]
pub struct CredentialName(pub String);

/// A `CredentialGrant` name. Interned in its own domain (`gr`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
#[serde(transparent)]
#[store(intern = "gr")]
pub struct GrantName(pub String);

/// The access a requirement names, in its provider family's vocabulary: `claude:subscription`
/// for the first slice. Core compares it for equality only. Interned in its own domain (`aq`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Stored)]
#[serde(transparent)]
#[store(intern = "aq")]
pub struct AccessName(pub String);

/// One authorizing grant, as the binding saw it: the access it permits for this requester's
/// work, in the family's own description (for a Claude subscription, `account`). Core passes it
/// through and never computes containment over it (#3881).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct GrantScope {
    pub scope: String,
}

/// How the binding's credential was chosen (#3882, amended by #3884).
///
/// The serde tags (`pin`, `draw`) are the resource store's JSON vocabulary; the store codes (`pn`,
/// `dr`) are the generated tables'. They are separate and need not match; the same holds for every
/// enum in this module.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(tag = "by", rename_all = "kebab-case")]
pub enum Selection {
    /// An explicit pin, authorized by at least one grant. `pinned_by` says who pinned it: the
    /// operator's `convoy start` option, or a rebind.
    #[store(code = "pn")]
    Pin { pinned_by: String },
    /// A uniform draw over the authorized, usable candidates, from the injected random source:
    /// `index` is into the candidates less the exclusions, in sorted order.
    #[store(code = "dr")]
    Draw { index: u32 },
}

/// Labels of a binding's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum BindingLabel {
    #[store(code = "op")]
    Open,
    #[store(code = "cl")]
    Closed,
    #[store(code = "rv")]
    Revoked,
}

/// A binding's state. The time it entered the state is the `State`'s `since`: when Closed or
/// Revoked, that is the closing or revocation time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(tag = "state", rename_all = "kebab-case")]
#[store(labels = BindingLabel)]
pub enum BindingState {
    /// In use. Opened at the binding's `opened_at`.
    #[store(code = "op")]
    Open,
    /// Ended by an explicit act: a rebind, or the requester's teardown. Kept for attribution.
    #[store(code = "cl")]
    Closed {
        reason: String,
        /// Directly delivered material was present in a vessel until this time. Flotilla cannot
        /// prove it was not copied (#3882 "Revocation limits").
        delivered_until: Option<Timestamp>,
    },
    /// Ended because no grant authorizes the credential any more: "binding revoked: no
    /// authorizing grant". The requester waits.
    #[store(code = "rv")]
    Revoked { reason: String, delivered_until: Option<Timestamp> },
}

impl BindingState {
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }

    /// The reason a closed or revoked binding ended.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Open => None,
            Self::Closed { reason, .. } | Self::Revoked { reason, .. } => Some(reason),
        }
    }
}

/// One authorization epoch of the bound credential, as the binding learned it: the first is the
/// epoch current at selection; a replacement login for the same account appends the next. The
/// binding does not move.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct EpochEntry {
    pub epoch: String,
    pub observed_at: Timestamp,
}

/// A requester's binding of one requirement to one managed credential (#3882).
///
/// The resource name is derived from the requester and the requirement, so there is at most one
/// open binding per pair: a rebind closes the old one under a retired name and opens the new one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(
    kind = "AccountBinding",
    code = "ab",
    index(name = "by_requester", eq(requester)),
    index(name = "open_by_credential", eq(credential), when(state in [Open]))
)]
pub struct AccountBindingSpec {
    /// The requester: a crew, by its role address (`project:<p>/convoy:<c>/role:<r>`; agent
    /// roles are unique across a convoy). The eventual home is AgentSession (#2912).
    pub requester: Address,
    /// The item that declared the requirement: the harness adapter, `claude-code`.
    #[store(intern = "it")]
    pub item: String,
    /// The access the requirement names.
    pub requirement: AccessName,
    /// The managed credential: its stable identity, never a token generation.
    pub credential: CredentialName,
    /// The account the credential belonged to at selection, as declared (`account_email`).
    pub account: Option<String>,
    /// Every grant authorizing this credential for this requester, with its scope. Updated when a
    /// grant is withdrawn while another still authorizes the credential.
    #[store(child)]
    pub grants: BTreeMap<GrantName, GrantScope>,
    /// The selection record: every authorized candidate, the exclusions with their reasons, and
    /// how the result was chosen. Exclusions are a subset of the candidates (an authorized
    /// candidate known to be unusable); a draw is over the candidates less the exclusions.
    #[store(child)]
    pub candidates: BTreeSet<CredentialName>,
    #[store(child)]
    pub exclusions: BTreeMap<CredentialName, String>,
    pub selection: Selection,
    /// The epoch lineage, oldest first.
    #[store(records)]
    pub epochs: Records<EpochEntry>,
    pub opened_at: Timestamp,
    #[store(state)]
    pub state: State<BindingState>,
}

/// When a plan's credential gets a new material generation (#3881).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    /// Direct delivery writes the new generation into the place.
    #[store(code = "pu")]
    Push,
    /// The mechanism handles it: an App mint, a proxy, an ambient login's external refresher.
    #[store(code = "mh")]
    Mechanism,
    /// The harness must restart to take it.
    #[store(code = "rs")]
    Restart,
}

/// The plan a registered provision implementation returned for one candidate environment kind
/// (#3881). Every text field is the family's own; core records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct ProvisionPlan {
    /// The registered implementation that made the plan, e.g. `claude-direct`.
    #[store(intern = "pi")]
    pub implementation: String,
    /// The effective access supplied, in the family's description (`account`).
    pub effective_access: String,
    /// How the access is supplied and made the harness's: for the first slice, an OAuth token
    /// written by the vessel agent into the requester's explicit `CLAUDE_CONFIG_DIR` home.
    #[store(intern = "pm")]
    pub mechanism: String,
    pub update_mode: UpdateMode,
}

/// What was last delivered at the place: the epoch and material generation, never the material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct Delivered {
    pub epoch: String,
    pub generation: String,
    pub at: Timestamp,
}

/// One binding provisioned at one place (#3882 "Provision is recorded separately, per place").
///
/// Removed when its copies are deleted: on revocation, rebind, requester teardown, or when a
/// replaced vessel's old place is cleaned up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(kind = "AccountProvision", code = "ap", index(name = "by_binding", eq(binding)), index(name = "by_place", eq(place)))]
pub struct AccountProvisionSpec {
    pub binding: Ref<AccountBindingSpec>,
    /// Where the agent loop runs: the Environment resource the harness launches in.
    #[store(intern = "pc")]
    pub place: String,
    /// The environment kind the plan was resolved for (`docker`, `agentless-ssh`, …), as named
    /// by the environment provider.
    #[store(intern = "ek")]
    pub environment_kind: String,
    #[store(flatten)]
    pub plan: ProvisionPlan,
    /// The requester's home at the place, which the plan delivers into (`CLAUDE_CONFIG_DIR`).
    pub home: String,
    #[store(flatten)]
    pub delivered: Option<Delivered>,
}

define_resource!(
    AccountBinding,
    "accountbindings",
    AccountBindingSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &["metadata.name", "spec.requester", "spec.credential"]
);

define_resource!(
    AccountProvision,
    "accountprovisions",
    AccountProvisionSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &["metadata.name", "spec.binding.name", "spec.place"]
);

/// The two kinds, for their DDL snapshot and the engine store's registry at the cut-over.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<AccountBindingSpec>()?;
    registry.register::<AccountProvisionSpec>()?;
    Ok(registry)
}

/// The binding's resource name: one per (requester, requirement), so a lookup needs no query.
pub fn binding_name(convoy: &str, role: &str, access: &AccessName) -> String {
    format!("{convoy}.{role}.{}", access.0.replace(':', "-"))
}

/// The requester's role address (`project:<p>/convoy:<c>/role:<r>`).
pub fn requester_address(project: Option<&str>, convoy: &str, role: &str) -> Result<Address, String> {
    let convoy = match project {
        Some(project) => Address::project(project).and_then(|address| address.child(AddressKind::Convoy, convoy)),
        None => Address::convoy(convoy),
    };
    convoy.and_then(|address| address.child(AddressKind::Role, role)).map_err(|error| format!("requester address: {error:?}"))
}

/// The material generation a delivery records (#3910 owner ruling 3): a short digest of the
/// published material, so a refreshed file is told apart without storing or logging it.
pub fn material_generation(material: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"flotilla-account-material-generation-v1");
    hash.update(material.as_bytes());
    format!("{:x}", hash.finalize())[..16].to_string()
}

impl std::fmt::Display for CredentialName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for GrantName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for AccessName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
