//! Resource lifecycle ownership shared by resource storage and query rows.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAuthority {
    Observed,
    Adopted,
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

    pub fn from_label_value(value: &str) -> Result<Self, String> {
        match value {
            "observed" => Ok(Self::Observed),
            "adopted" => Ok(Self::Adopted),
            "managed" => Ok(Self::Managed),
            other => Err(format!("invalid lifecycle authority '{other}'")),
        }
    }
}
