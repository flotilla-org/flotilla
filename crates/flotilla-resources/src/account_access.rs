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
//! These are the serde shapes of the old-store kinds [`AccountBinding`] and [`AccountProvision`]
//! that production runs with `[primitives]` off (owner ruling 1 on #3910); they go with the
//! switch. The engine stores the reviewed tables through `flotilla_provisioning::stored::account`
//! (#3824 layer 1); the parts both shapes share derive `Stored` here.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::Kind as AddressKind;
use flotilla_schema::{Address, Records, Ref, Stored};
/// The schema's values the account records carry, for the crates that read them without it.
pub use flotilla_schema::{State, Timestamp};

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

/// A binding's state. The time it entered the state is the `State`'s `since`: when Closed or
/// Revoked, that is the closing or revocation time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum BindingState {
    /// In use. Opened at the binding's `opened_at`.
    Open,
    /// Ended by an explicit act: a rebind, or the requester's teardown. Kept for attribution.
    Closed {
        reason: String,
        /// Directly delivered material was present in a vessel until this time. Flotilla cannot
        /// prove it was not copied (#3882 "Revocation limits").
        delivered_until: Option<Timestamp>,
    },
    /// Ended because no grant authorizes the credential any more: "binding revoked: no
    /// authorizing grant". The requester waits.
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBindingSpec {
    /// The requester: a crew, by its role address (`project:<p>/convoy:<c>/role:<r>`; agent
    /// roles are unique across a convoy). The eventual home is AgentSession (#2912).
    pub requester: Address,
    /// The item that declared the requirement: the harness adapter, `claude-code`.
    pub item: String,
    /// The access the requirement names.
    pub requirement: AccessName,
    /// The managed credential: its stable identity, never a token generation.
    pub credential: CredentialName,
    /// The account the credential belonged to at selection, as declared (`account_email`).
    pub account: Option<String>,
    /// Every grant authorizing this credential for this requester, with its scope. Updated when a
    /// grant is withdrawn while another still authorizes the credential.
    pub grants: BTreeMap<GrantName, GrantScope>,
    /// The selection record: every authorized candidate, the exclusions with their reasons, and
    /// how the result was chosen. Exclusions are a subset of the candidates (an authorized
    /// candidate known to be unusable); a draw is over the candidates less the exclusions.
    pub candidates: BTreeSet<CredentialName>,
    pub exclusions: BTreeMap<CredentialName, String>,
    pub selection: Selection,
    /// The epoch lineage, oldest first.
    pub epochs: Records<EpochEntry>,
    pub opened_at: Timestamp,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountProvisionSpec {
    pub binding: Ref<AccountBindingSpec>,
    /// Where the agent loop runs: the Environment resource the harness launches in.
    pub place: String,
    /// The environment kind the plan was resolved for (`docker`, `ssh-host`, …), as named
    /// by the environment provider.
    pub environment_kind: String,
    pub plan: ProvisionPlan,
    /// The requester's material directory at the place, which the plan delivers into. Before
    /// #4012 this was the requester's home itself; either way it is where the copy lies.
    pub home: String,
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

/// Labels a binding by its own name; its provisions carry it too, so they are found by it.
pub const ACCOUNT_BINDING_LABEL: &str = "flotilla.work/account-binding";

/// Labels a binding by its credential, so grant and declaration changes reach its convoy.
pub const ACCOUNT_CREDENTIAL_LABEL: &str = "flotilla.work/account-credential";

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

/// The convoy and role a requester address names.
pub fn requester_parts(requester: &Address) -> Option<(String, String)> {
    let mut convoy = None;
    let mut role = None;
    for segment in requester.segments() {
        match segment.kind() {
            AddressKind::Convoy => convoy = Some(segment.value().to_string()),
            AddressKind::Role => role = Some(segment.value().to_string()),
            _ => {}
        }
    }
    convoy.zip(role)
}

impl AccountBindingSpec {
    /// How the binding was chosen, for display: `pinned by <who>` or `drawn: <i> of <n> usable`.
    pub fn describe_selection(&self) -> String {
        match &self.selection {
            Selection::Pin { pinned_by } => format!("pinned by {pinned_by}"),
            Selection::Draw { index } => {
                let usable = self.candidates.iter().filter(|name| !self.exclusions.contains_key(*name)).count();
                format!("drawn: {} of {usable} usable", index + 1)
            }
        }
    }
}

/// A requester's home for one harness under a place's durable home base (#4012):
/// `<base>/agent-homes/requesters/<convoy>/<role>/<harness>` (#3910 ruling 4). Names are encoded
/// injectively: a name of ASCII alphanumerics, `-` and `_` is used as it is; any other has the
/// rest replaced by `-` and a digest of the whole name appended, so `a.b` and `a-b` never share.
pub fn requester_home(base: &std::path::Path, convoy: &str, role: &str, harness: &str) -> std::path::PathBuf {
    requester_path(base, REQUESTER_HOME_DIR, convoy, role, harness)
}

/// Where a place's requester material directories live under its runtime base.
pub const REQUESTER_MATERIAL_DIR: &str = "agent-material";

/// A requester's material directory for one harness under a place's runtime base (#4012):
/// `<base>/agent-material/requesters/<convoy>/<role>/<harness>`, encoded as [`requester_home`].
/// Secrets such as a token file live here rather than in the durable home, so a reboot that
/// clears the runtime base clears them and the next launch delivers them again.
pub fn requester_material(base: &std::path::Path, convoy: &str, role: &str, harness: &str) -> std::path::PathBuf {
    requester_path(base, REQUESTER_MATERIAL_DIR, convoy, role, harness)
}

/// The directory holding every requester of `convoy` under `tree` of `base`: the parent of
/// [`requester_home`]'s role directory, which the convoy's retirement removes whole.
pub fn requester_convoy_dir(base: &std::path::Path, tree: &str, convoy: &str) -> std::path::PathBuf {
    requester_path(base, tree, convoy, "role", "harness")
        .parent()
        .and_then(std::path::Path::parent)
        .expect("a requester path has a convoy directory")
        .to_path_buf()
}

/// Where a place's requester home directories live under its durable base.
pub const REQUESTER_HOME_DIR: &str = "agent-homes";

fn requester_path(base: &std::path::Path, tree: &str, convoy: &str, role: &str, harness: &str) -> std::path::PathBuf {
    fn component(name: &str) -> String {
        use sha2::{Digest, Sha256};
        let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if !name.is_empty() && name.chars().all(plain) {
            return name.to_string();
        }
        let sanitized = name.chars().map(|c| if plain(c) { c } else { '-' }).collect::<String>();
        format!("{sanitized}.{}", &format!("{:x}", Sha256::digest(name.as_bytes()))[..12])
    }
    base.join(tree).join("requesters").join(component(convoy)).join(component(role)).join(harness)
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

/// A binding as its readers see it, from whichever store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRecord {
    pub name: String,
    /// The home that wrote it, which alone writes it again.
    pub home: String,
    /// The store's version, compared only for equality.
    pub version: String,
    pub spec: AccountBindingSpec,
}

/// A provision as its readers see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionRecord {
    pub name: String,
    pub home: String,
    pub version: String,
    pub spec: AccountProvisionSpec,
}

/// A binding chosen for its requester, before it is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingBinding {
    /// The requester's binding name ([`binding_name`]).
    pub name: String,
    pub spec: AccountBindingSpec,
}

/// What changed, for the account controllers' wake-ups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountChange {
    /// A binding of `convoy`'s requesters, by its record name.
    Binding { name: String, convoy: Option<String> },
    /// A provision of binding `binding`, by its record name.
    Provision { name: String, binding: String, convoy: Option<String> },
}

/// How a version-checked provision write ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProvisionWrite {
    Written,
    /// Another write moved the record first, or removed it.
    Superseded,
}

/// The account kinds' read and write port (#3824 layer 1). With `[primitives]` off it is the old
/// store's records; with it on, the engine's: the binding home's AccountBinding machine and the
/// place's AccountProvision plain records, read from the decided store. Callers never see how
/// either names its records beyond the names it returns.
#[async_trait::async_trait]
pub trait AccountRecords: Send + Sync {
    /// The requester's current binding for `access`: its open one, or else its latest ended one.
    async fn requester_binding(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
        access: &AccessName,
    ) -> Result<Option<BindingRecord>, String>;
    /// Binding `name`, from whichever home holds it.
    async fn binding(&self, namespace: &str, name: &str) -> Result<Option<BindingRecord>, String>;
    /// Every binding of a convoy's requesters, open and ended: this home's alone with `local`.
    async fn convoy_bindings(&self, namespace: &str, convoy: &str, local: bool) -> Result<Vec<BindingRecord>, String>;
    /// The convoys whose bindings here use one of `credentials`, and every convoy with a binding
    /// here.
    async fn bound_convoys(&self, namespace: &str, credentials: &[String]) -> Result<(BTreeSet<String>, BTreeSet<String>), String>;
    /// Opens each binding; a requester that already holds an open one keeps it (#3882).
    async fn open(&self, namespace: &str, bindings: Vec<PendingBinding>) -> Result<(), String>;
    /// Ends an open binding as `state` (Closed or Revoked).
    async fn end(&self, namespace: &str, binding: &BindingRecord, state: BindingState, now: Timestamp) -> Result<(), String>;
    /// Records the grants that now authorise an open binding's credential.
    async fn regrant(&self, namespace: &str, binding: &BindingRecord, grants: BTreeMap<GrantName, GrantScope>) -> Result<(), String>;
    /// Appends the credential's next authorisation epoch to an open binding.
    async fn observe_epoch(&self, namespace: &str, binding: &BindingRecord, epoch: EpochEntry) -> Result<(), String>;
    /// An explicit rebind (#3882): the requester's open binding, if any, closes for `rebind` with
    /// `delivered_until`, kept for attribution, and `opened` opens in its place.
    async fn rebind(
        &self,
        namespace: &str,
        opened: PendingBinding,
        delivered_until: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<(), String>;
    /// This home's provision `name`.
    async fn provision(&self, namespace: &str, name: &str) -> Result<Option<ProvisionRecord>, String>;
    /// The provisions of `bindings`: this home's alone with `local`.
    async fn binding_provisions(&self, namespace: &str, bindings: &[String], local: bool) -> Result<Vec<ProvisionRecord>, String>;
    /// Every provision homed here.
    async fn local_provisions(&self, namespace: &str) -> Result<Vec<ProvisionRecord>, String>;
    /// Records provision `name` of `convoy`'s binding at this place, or with `replace` brings an
    /// existing one to `spec`; without it an existing record is kept.
    async fn record_provision(
        &self,
        namespace: &str,
        name: &str,
        convoy: &str,
        spec: &AccountProvisionSpec,
        replace: bool,
    ) -> Result<(), String>;
    /// Writes `spec` over `provision` as it was read.
    async fn update_provision(
        &self,
        namespace: &str,
        provision: &ProvisionRecord,
        spec: &AccountProvisionSpec,
    ) -> Result<ProvisionWrite, String>;
    /// Removes provision `name`; absent is done.
    async fn delete_provision(&self, namespace: &str, name: &str) -> Result<(), String>;
    /// Every later change of a binding or provision, from now on, for the controllers' wake-ups.
    async fn changes(&self, namespace: &str) -> Result<futures::stream::BoxStream<'static, Result<AccountChange, String>>, String>;
}
