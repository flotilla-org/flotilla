//! The access layer's per-access status (#3964, #3956 Q4–Q8).
//!
//! An [`AccessDelivery`] is one access relation (#2914) delivered at one place: the
//! home the access layer delivered there, which launch reads, and, per need, the
//! mechanism the planner chose and the content digest that landed. `convoy explain`
//! shows the mechanism per need from it. A delivery that fails records its retry
//! state here, never on the Environment's status.
//!
//! The access is an opaque address: the crew role address
//! (`project:<p>/convoy:<c>/vessel:<v>/role:<r>`) until the #3108 cut-over.
//! Delivery never parses it.
//!
//! Like the account access kinds, the record derives serde, for the resource-store kind
//! production runs today, and `Stored`, for the generated tables the engine store takes
//! on at the cut-over.

use std::collections::BTreeMap;

use flotilla_schema::{Address, KindRegistry, RegistryError, Stored, Timestamp};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::NoStatusPatch, ReplicationClass};

/// The mechanism a need was delivered through, in the vessel delivery vocabulary
/// (#3956 Q8). Fetch by digest and provider volumes are further variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryMechanism {
    /// The guest reaches a host path read-only, as it changes.
    #[store(code = "hrl")]
    HostPathReadOnlyLive,
    /// The guest reaches a read-only copy of a host path taken once.
    #[store(code = "hrs")]
    HostPathReadOnlySnapshot,
    /// The guest reaches a host path read-write, as it changes.
    #[store(code = "hwl")]
    HostPathReadWriteLive,
    /// The guest reaches a read-write copy of a host path taken once.
    #[store(code = "hws")]
    HostPathReadWriteSnapshot,
    /// The vessel agent fetched the content by URL.
    #[store(code = "fu")]
    AgentFetchUrl,
    /// The controller pushed the content through the vessel agent.
    #[store(code = "pu")]
    AgentPush,
}

/// One need as it landed at the place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct DeliveredNeed {
    pub mechanism: DeliveryMechanism,
    /// The content digest that landed; never the content.
    pub digest: String,
    pub at: Timestamp,
}

/// The access layer's last failed attempt at the place, and when the next is due.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct DeliveryRetry {
    pub attempts: u32,
    pub error: String,
    pub next_at: Timestamp,
}

/// One access delivered at one place. One record per (access, place): a replaced
/// vessel's new place gets its own, and the old one is removed with its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
#[store(kind = "AccessDelivery", code = "ad", index(name = "by_access", eq(access)), index(name = "by_place", eq(place)))]
pub struct AccessDeliverySpec {
    /// The access relation's identity, opaque to delivery.
    pub access: Address,
    /// Where the access is delivered: the Environment its harness launches in.
    #[store(intern = "pc")]
    pub place: String,
    /// The harness adapter the access's home is for (`claude-code`, `codex`).
    #[store(intern = "it")]
    pub harness: String,
    /// The home the access layer delivered at the place. Launch uses exactly this.
    pub home: String,
    /// Each need delivered, by its name within the access (`home`, `skills`, `account`).
    #[store(child)]
    pub needs: BTreeMap<String, DeliveredNeed>,
    #[store(flatten)]
    pub retry: Option<DeliveryRetry>,
}

define_resource!(
    AccessDelivery,
    "accessdeliveries",
    AccessDeliverySpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    queryable = &["metadata.name", "spec.access", "spec.place"]
);

/// The generated-table kinds of the access layer's status.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    registry.register::<AccessDeliverySpec>()?;
    Ok(registry)
}
