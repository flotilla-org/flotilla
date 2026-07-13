use serde::{Deserialize, Serialize};

use crate::error::ResourceError;

pub const AUTHORITY_LABEL: &str = "flotilla.work/authority";
pub const CONVOY_LABEL: &str = "flotilla.work/convoy";
pub const VESSEL_LABEL: &str = "flotilla.work/vessel";
pub const VESSEL_REF_LABEL: &str = "flotilla.work/vessel_ref";
pub const ROLE_LABEL: &str = "flotilla.work/role";
pub const REPO_KEY_LABEL: &str = "flotilla.work/repo-key";
pub const VESSEL_ORDINAL_LABEL: &str = "flotilla.work/vessel_ordinal";
pub const CREW_ORDINAL_LABEL: &str = "flotilla.work/crew_ordinal";
pub const REPO_LABEL: &str = "flotilla.work/repo";
pub const RESERVED_PREFIX: &str = "flotilla.work/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LifecycleAuthority {
    #[serde(rename = "observed")]
    Observed,
    #[serde(rename = "adopted")]
    Adopted,
    #[serde(rename = "managed")]
    Managed,
}

impl LifecycleAuthority {
    pub fn as_label_value(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Adopted => "adopted",
            Self::Managed => "managed",
        }
    }

    pub fn from_label_value(value: &str) -> Result<Self, ResourceError> {
        match value {
            "observed" => Ok(Self::Observed),
            "adopted" => Ok(Self::Adopted),
            "managed" => Ok(Self::Managed),
            other => Err(ResourceError::invalid(format!("invalid lifecycle authority '{other}'"))),
        }
    }
}
