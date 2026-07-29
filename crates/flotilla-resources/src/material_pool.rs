use std::collections::BTreeMap;

use flotilla_protocol::ResourceRef;
use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, status_patch::StatusPatch};

define_resource!(MaterialPool, "materialpools", MaterialPoolSpec, MaterialPoolStatus, MaterialPoolStatusPatch);

/// A host-local pool of opaque directory payloads. The pool layer assigns
/// directories exclusively; consumers decide what the files mean and how to
/// deliver them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialPoolSpec {
    #[serde(default)]
    pub units: BTreeMap<String, MaterialPoolUnitSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialPoolUnitSpec {
    pub directory: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialPoolStatus {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub leases: BTreeMap<String, MaterialPoolLease>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialPoolLease {
    pub holder_ref: ResourceRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterialPoolStatusPatch {
    ReplaceLeases { leases: BTreeMap<String, MaterialPoolLease> },
}

impl StatusPatch<MaterialPoolStatus> for MaterialPoolStatusPatch {
    fn apply(&self, status: &mut MaterialPoolStatus) {
        match self {
            Self::ReplaceLeases { leases } => status.leases = leases.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReplicationClass, Resource};

    #[test]
    fn pool_is_host_local_and_serializes_only_unit_locations_and_lease_owners() {
        assert_eq!(MaterialPool::REPLICATION_CLASS, ReplicationClass::None);
        let encoded = serde_json::to_value(MaterialPoolSpec {
            units: BTreeMap::from([("unit-0".to_string(), MaterialPoolUnitSpec {
                directory: "/var/lib/flotilla/material/unit-0".to_string(),
            })]),
        })
        .expect("serialize material pool");

        assert_eq!(encoded["units"]["unit-0"]["directory"], "/var/lib/flotilla/material/unit-0");
        assert!(encoded.to_string().find("auth").is_none());
    }
}
