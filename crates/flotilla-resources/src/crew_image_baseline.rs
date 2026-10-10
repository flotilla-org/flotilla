use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, NoStatusPatch, ReplicationClass};

// Image selection is fleet resolver configuration, not host runtime state.
// Generation 1 keeps this kind authoritative alongside layers. Retire it only
// after generation 2 stops writing baselines plus the next fleet roll.
define_resource!(
    CrewImageBaseline,
    "crewimagebaselines",
    CrewImageBaselineSpec,
    (),
    NoStatusPatch,
    replication = ReplicationClass::Definitions
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewImageBaselineSpec {
    pub image: String,
    /// Generation 1 records layers alongside the authoritative literal image.
    /// ADR 0047: remove the default after the generation-1 fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layers: Option<crate::ImageLayerSelection>,
}
