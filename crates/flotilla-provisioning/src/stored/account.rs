//! Account access on the engine (#3824 layer 1): the AccountBinding machine and the
//! AccountProvision plain kind, declared beside the lifecycle kinds because the crates that
//! write them (admission, the daemon runtime) must not reach the engine.
//!
//! Their tables are the ones the owner reviewed on #3910 (`account_access.sql`): the binding's
//! spec, its state (`Open`, `Closed`, `Revoked`) and its epoch lineage; the provision's spec.
//! The serde shapes in `flotilla-resources` stay the old store's until the switch is deleted.
//!
//! A binding is chosen once and ends once (ADR 0064: ends are ends). While Open, two things
//! change on it, each through a self-edge rather than a spec edit: its recorded grants
//! (`regrant`, a withdrawn grant while another still authorises), and its epoch lineage
//! (`observe_epoch`, a replacement login for the same account). Like a Convoy's duties, both
//! are spec collections only edges write.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::account_access::{
    AccessName, CredentialName, Delivered, EpochEntry, GrantName, GrantScope, ProvisionPlan, Selection,
};
use flotilla_schema::{Address, Edge, EdgeError, KindRegistry, Records, RecordsFull, RecordsOf, Ref, RegistryError, Stored, Timestamp};
use flotilla_state_machines::{
    edges, states, Admission, EdgeDef, Ended, Machine, MachineDef, RecordWrite, Resource, StoredWrite, WrongState,
};
use serde::{Deserialize, Serialize};

/// A requester's binding of one requirement to one managed credential (#3882), as the engine
/// stores it: kind code `ab`, homed where the requester was admitted.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "AccountBinding",
    code = "ab",
    state = BindingState,
    index(name = "by_requester", eq(requester)),
    index(name = "open_by_credential", eq(credential), when(state in [Open]))
)]
pub struct BindingSpec {
    /// The requester: a crew, by its role address.
    pub requester: Address,
    /// The item that declared the requirement: the harness adapter, `claude-code`.
    #[store(intern = "it")]
    pub item: String,
    pub requirement: AccessName,
    /// The managed credential: its stable identity, never a token generation.
    pub credential: CredentialName,
    /// The account the credential belonged to at selection, as declared.
    pub account: Option<String>,
    /// Every grant authorising this credential for this requester, with its scope. Written
    /// only by `regrant`.
    #[store(child)]
    pub grants: BTreeMap<GrantName, GrantScope>,
    /// The selection record: every authorised candidate, the exclusions with their reasons.
    #[store(child)]
    pub candidates: BTreeSet<CredentialName>,
    #[store(child)]
    pub exclusions: BTreeMap<CredentialName, String>,
    pub selection: Selection,
    /// The epoch lineage, oldest first. Appended only by `observe_epoch`.
    #[store(records)]
    pub epochs: Records<EpochEntry>,
    pub opened_at: Timestamp,
}

states! {
    /// A binding's state; the time it entered the state is its `since`.
    pub enum BindingState => BindingLabel {
        /// In use.
        Open = "op",
        /// Ended by an explicit act: a rebind, or the requester's teardown.
        Closed { reason: String, delivered_until: Option<Timestamp> } = "cl",
        /// Ended because no grant authorises the credential any more.
        Revoked { reason: String, delivered_until: Option<Timestamp> } = "rv",
    }
}

/// Close the binding: a rebind or the requester's teardown. `delivered_until` is when
/// delivered material was last known present in a vessel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Close {
    pub reason: String,
    pub delivered_until: Option<Timestamp>,
}

/// Revoke the binding: no grant authorises the credential, or a replacement login names
/// another account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoke {
    pub reason: String,
    pub delivered_until: Option<Timestamp>,
}

/// Record the grants that now authorise the credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Regrant {
    pub grants: BTreeMap<GrantName, GrantScope>,
}

/// Record the credential's next authorisation epoch for the same account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveEpoch {
    pub epoch: String,
    pub observed_at: Timestamp,
}

edges! {
    pub enum BindingEdge => BindingEdgeKind {
        Close(Close) = "cl",
        Revoke(Revoke) = "rv",
        Regrant(Regrant) = "rg",
        ObserveEpoch(ObserveEpoch) = "ep",
    }
}

/// Why a binding write was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingRefusal {
    /// The binding has ended (Closed or Revoked).
    Ended,
    /// An end with no reason, or a regrant with no grant: the binding would record nothing.
    Empty,
    /// The epoch is not after the last one recorded.
    StaleEpoch,
    /// The grants are the ones already recorded.
    Unchanged,
}

impl From<Ended<BindingLabel>> for BindingRefusal {
    fn from(_: Ended<BindingLabel>) -> Self {
        BindingRefusal::Ended
    }
}

/// Unreachable: every edge leaves Open, and every other label is an end and refuses first.
impl From<WrongState<BindingLabel, BindingEdgeKind>> for BindingRefusal {
    fn from(_: WrongState<BindingLabel, BindingEdgeKind>) -> Self {
        BindingRefusal::Ended
    }
}

/// The spec writes the binding's edges make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingWrite {
    Grants { before: BTreeMap<GrantName, GrantScope>, after: BTreeMap<GrantName, GrantScope> },
    Epoch(EpochEntry),
}

const EPOCHS: RecordsOf<Resource<AccountBinding>, EpochEntry> = RecordsOf::new("epochs");

impl RecordWrite<BindingSpec> for BindingWrite {
    fn field(&self) -> &'static str {
        match self {
            BindingWrite::Grants { .. } => "grants",
            BindingWrite::Epoch(_) => "epochs",
        }
    }

    fn apply(&self, spec: &mut BindingSpec, version: u64) -> Result<(), RecordsFull> {
        match self {
            BindingWrite::Grants { after, .. } => {
                spec.grants.clone_from(after);
                Ok(())
            }
            BindingWrite::Epoch(entry) => spec.epochs.append(version, entry.clone()).map(drop),
        }
    }
}

impl StoredWrite<Resource<AccountBinding>> for BindingWrite {
    fn store(&self, edge: Edge<Resource<AccountBinding>>) -> Result<Edge<Resource<AccountBinding>>, EdgeError> {
        match self {
            BindingWrite::Grants { before, after } => edge.replace_children("grants", before, after),
            BindingWrite::Epoch(entry) => edge.append(EPOCHS, entry),
        }
    }
}

/// What binding gates read: nothing beyond the binding itself.
pub trait BindingWorld {}
impl<T: ?Sized> BindingWorld for T {}

/// The AccountBinding machine.
pub struct AccountBinding;

impl Machine for AccountBinding {
    const NAME: &'static str = "AccountBinding";
    type Request = BindingSpec;
    type Spec = BindingSpec;
    type State = BindingState;
    type Edge = BindingEdge;
    type Write = BindingWrite;
    type Refusal = BindingRefusal;
    type World = dyn BindingWorld;

    fn definition() -> MachineDef<Self> {
        use BindingLabel::*;
        MachineDef::new(Open)
            .edge(
                EdgeDef::<Self, Close>::new()
                    .from([Open])
                    .to([Closed])
                    .changes(["reason", "delivered_until"])
                    .gate(
                        "a close names its reason",
                        |_, close: &Close| {
                            if close.reason.is_empty() {
                                Err(BindingRefusal::Empty)
                            } else {
                                Ok(())
                            }
                        },
                    )
                    .effect(|_, _, close: &Close| BindingState::Closed {
                        reason: close.reason.clone(),
                        delivered_until: close.delivered_until,
                    }),
            )
            .edge(
                EdgeDef::<Self, Revoke>::new()
                    .from([Open])
                    .to([Revoked])
                    .changes(["reason", "delivered_until"])
                    .gate(
                        "a revocation names its reason",
                        |_, revoke: &Revoke| {
                            if revoke.reason.is_empty() {
                                Err(BindingRefusal::Empty)
                            } else {
                                Ok(())
                            }
                        },
                    )
                    .effect(|_, _, revoke: &Revoke| BindingState::Revoked {
                        reason: revoke.reason.clone(),
                        delivered_until: revoke.delivered_until,
                    }),
            )
            .edge(
                EdgeDef::<Self, Regrant>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["grants"])
                    .gate("at least one grant still authorises the credential, and the set changed", |h, regrant: &Regrant| {
                        if regrant.grants.is_empty() {
                            Err(BindingRefusal::Empty)
                        } else if regrant.grants == h.spec().grants {
                            Err(BindingRefusal::Unchanged)
                        } else {
                            Ok(())
                        }
                    })
                    .effect(|h, state, regrant: &Regrant| {
                        let before = h.spec().grants.clone();
                        h.write(BindingWrite::Grants { before, after: regrant.grants.clone() });
                        state
                    }),
            )
            .edge(
                EdgeDef::<Self, ObserveEpoch>::new()
                    .from([Open])
                    .to([Open])
                    .writes(["epochs"])
                    .gate("the epoch is after the last one recorded", |h, observed: &ObserveEpoch| {
                        let last = h.spec().epochs.last().and_then(|row| row.record.epoch.parse::<u64>().ok());
                        match (last, observed.epoch.parse::<u64>().ok()) {
                            (Some(last), Some(next)) if next <= last => Err(BindingRefusal::StaleEpoch),
                            _ => Ok(()),
                        }
                    })
                    .effect(|h, state, observed: &ObserveEpoch| {
                        h.write(BindingWrite::Epoch(EpochEntry { epoch: observed.epoch.clone(), observed_at: observed.observed_at }));
                        state
                    }),
            )
            .end(Closed, "the binding was closed")
            .end(Revoked, "the binding was revoked")
    }

    /// Admission chose the credential before the create; the binding records the choice.
    fn admit(_: &mut Admission<'_, AccountBinding>, spec: BindingSpec) -> Result<(BindingSpec, BindingState), BindingRefusal> {
        Ok((spec, BindingState::Open))
    }
}

/// One binding provisioned at one place (#3882), as the engine stores it: kind code `ap`, a
/// plain kind homed where the agent loop runs. The provision runtime edits `delivered` whole.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "AccountProvision", code = "ap", index(name = "by_binding", eq(binding)), index(name = "by_place", eq(place)))]
pub struct ProvisionSpec {
    pub binding: Ref<Resource<AccountBinding>>,
    /// Where the agent loop runs: the Environment resource the harness launches in.
    #[store(intern = "pc")]
    pub place: String,
    /// The environment kind the plan was resolved for, as named by the environment provider.
    #[store(intern = "ek")]
    pub environment_kind: String,
    #[store(flatten)]
    pub plan: ProvisionPlan,
    /// The requester's home at the place, which the plan delivers into (`CLAUDE_CONFIG_DIR`).
    pub home: String,
    #[store(flatten)]
    pub delivered: Option<Delivered>,
}

/// Registers the account kinds into `registry` under the codes the owner reviewed: AccountBinding
/// `ab`, a machine, and AccountProvision `ap`, a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<Resource<AccountBinding>>()?;
    registry.register::<ProvisionSpec>()?;
    Ok(())
}

/// The account kinds alone, for their DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
