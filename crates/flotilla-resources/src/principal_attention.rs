use chrono::{DateTime, Utc};
use flotilla_protocol::{PrincipalRef, ResourceRef};
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(Regard, "regards", RegardSpec, RegardStatus, RegardStatusPatch);
define_resource!(Demand, "demands", DemandSpec, DemandStatus, DemandStatusPatch);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DemandPoolRef(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct RegardSpec {
    pub principal_ref: PrincipalRef,
    pub target: ResourceRef,
    pub source: RegardSource,
    pub expiry: RegardExpiryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegardSource {
    Expressed,
    Implicit { policy: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegardExpiryPolicy {
    Decaying { expires_after_seconds: u64 },
    Pin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegardStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegardStatusPatch {
    Refresh { as_of: DateTime<Utc> },
}

impl StatusPatch<RegardStatus> for RegardStatusPatch {
    fn apply(&self, status: &mut RegardStatus) {
        match self {
            Self::Refresh { as_of } => {
                status.created_at.get_or_insert(*as_of);
                if status.refreshed_at.is_none_or(|current| *as_of > current) {
                    status.refreshed_at = Some(*as_of);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DemandSpec {
    pub originating_work_ref: ResourceRef,
    pub kind: DemandKind,
    pub addressee: DemandAddressee,
}

impl DemandSpec {
    pub fn for_dispatching_principal(originating_work_ref: ResourceRef, kind: DemandKind, dispatching_principal_ref: PrincipalRef) -> Self {
        Self { originating_work_ref, kind, addressee: DemandAddressee::Principal { principal_ref: dispatching_principal_ref } }
    }

    pub fn for_pool(originating_work_ref: ResourceRef, kind: DemandKind, pool_ref: DemandPoolRef) -> Self {
        Self { originating_work_ref, kind, addressee: DemandAddressee::Pool { pool_ref } }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandKind {
    Permission,
    HumanGate,
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DemandAddressee {
    Principal { principal_ref: PrincipalRef },
    Pool { pool_ref: DemandPoolRef },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DemandStatus {
    #[serde(default)]
    pub state: DemandState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raised: Option<DemandTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satisfied: Option<DemandTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged: Option<DemandTransition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct DemandTransition {
    pub as_of: DateTime<Utc>,
    pub authority: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemandState {
    #[default]
    Raised,
    Satisfied,
    Acknowledged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandStatusPatch {
    Raise { as_of: DateTime<Utc>, authority: String },
    Satisfy { as_of: DateTime<Utc>, authority: String },
    Acknowledge { as_of: DateTime<Utc>, authority: String },
}

impl StatusPatch<DemandStatus> for DemandStatusPatch {
    fn apply(&self, status: &mut DemandStatus) {
        match self {
            Self::Raise { as_of, authority } => {
                if status.raised.is_none() && status.satisfied.is_none() && status.acknowledged.is_none() {
                    status.state = DemandState::Raised;
                    status.raised = Some(DemandTransition { as_of: *as_of, authority: authority.clone() });
                }
            }
            Self::Satisfy { as_of, authority } => {
                if status.state != DemandState::Acknowledged {
                    status.state = DemandState::Satisfied;
                    status.raised.get_or_insert_with(|| DemandTransition { as_of: *as_of, authority: authority.clone() });
                    status.satisfied.get_or_insert_with(|| DemandTransition { as_of: *as_of, authority: authority.clone() });
                }
            }
            Self::Acknowledge { as_of, authority } => {
                status.state = DemandState::Acknowledged;
                status.raised.get_or_insert_with(|| DemandTransition { as_of: *as_of, authority: authority.clone() });
                status.acknowledged.get_or_insert_with(|| DemandTransition { as_of: *as_of, authority: authority.clone() });
            }
        }
    }
}
