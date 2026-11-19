//! Demand as the previous generation stored it, read only by the import. Demand retired with #3108:
//! attentions are Messages on the engine and nothing writes a Demand. Remove one roll after the
//! kind retires.

use chrono::{DateTime, Utc};
use flotilla_protocol::{PrincipalRef, ResourceRef};
use flotilla_resources::{K8sObjectMeta, ObjectMeta};
use serde::Deserialize;
use serde_json::Value;

pub const KIND: &str = "Demand";
pub const PLURAL: &str = "demands";
const API_VERSION: &str = "flotilla.work/v1";

/// Why a held standing convoy, or a convoy whose reclaim is refused, needed a person.
pub const RECLAIM_REFUSAL_REASON_ANNOTATION: &str = "flotilla.work/reclaim-refusal-reason";
/// Why a standing convoy was held; stored under the same key as a refused reclaim.
pub const ENSURE_HOLD_REASON_ANNOTATION: &str = RECLAIM_REFUSAL_REASON_ANNOTATION;
/// Why a credential stopped refreshing for a vessel.
pub const CREDENTIAL_REFRESH_REASON_ANNOTATION: &str = "flotilla.work/credential-refresh-reason";
/// The convoy's name for the vessel whose credential stopped refreshing.
pub const CREDENTIAL_REFRESH_VESSEL_ANNOTATION: &str = "flotilla.work/credential-refresh-vessel";
/// The demand a standing role's configuration drift was raised as, by its ConvoyEnsure's name.
pub const ENSURE_DRIFT_ATTENTION_PREFIX: &str = "ensure-config-drift-";
/// Why a standing role's configuration drifted, and the command that rolls it.
pub const ENSURE_CONFIG_DRIFT_REASON_ANNOTATION: &str = "flotilla.work/ensure-config-drift-reason";
/// Names a held batch, why it is held and the command that clears it.
pub const DELIVERY_HOLD_REASON_ANNOTATION: &str = "flotilla.work/delivery-hold-reason";

/// One stored demand: its metadata, spec and status.
#[derive(Debug, Clone)]
pub struct RetiredDemand {
    pub metadata: ObjectMeta,
    pub spec: DemandSpec,
    pub status: Option<DemandStatus>,
}

impl RetiredDemand {
    /// The demand a stored document holds.
    pub fn decode(document: &Value) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(rename = "apiVersion")]
            api_version: String,
            kind: String,
            metadata: K8sObjectMeta,
            spec: DemandSpec,
            #[serde(default)]
            status: Option<DemandStatus>,
        }
        let stored = Stored::deserialize(document).map_err(|error| error.to_string())?;
        if stored.api_version != API_VERSION || stored.kind != KIND {
            return Err(format!("expected {API_VERSION} {KIND}, got {} {}", stored.api_version, stored.kind));
        }
        Ok(Self { metadata: stored.metadata.into(), spec: stored.spec, status: stored.status })
    }
}

/// The spec fields the import reads; the rest (its kind, expiry) are dropped.
#[derive(Debug, Clone, Deserialize)]
pub struct DemandSpec {
    pub originating_work_ref: ResourceRef,
    pub addressee: DemandAddressee,
    /// Retired response options, read so the import refuses a record that offers any.
    #[serde(default, rename = "response_options")]
    pub retired_response_options: Option<Value>,
    /// A retired human gate's context, read so the import refuses a record that carries one.
    #[serde(default, rename = "human_gate")]
    pub retired_human_gate: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DemandAddressee {
    Principal { principal_ref: PrincipalRef },
}

/// The status fields the import reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DemandStatus {
    #[serde(default)]
    pub state: DemandState,
    #[serde(default)]
    pub raised: Option<DemandTransition>,
    /// A retired verdict, read so the import refuses a record that carries one.
    #[serde(default, rename = "verdict")]
    pub retired_verdict: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DemandTransition {
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandState {
    #[default]
    Raised,
    Satisfied,
    Escalated,
    Acknowledged,
}
