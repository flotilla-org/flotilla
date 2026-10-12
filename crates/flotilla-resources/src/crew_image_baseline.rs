use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, NoStatusPatch, ReplicationClass};

// Image selection is fleet resolver configuration, not host runtime state.
// Compatibility-only in generation 2 (#2731): no fleet manifests author it.
// Keep registration for the previous stored generation; remove in generation 3
// after the cut-over fleet roll and validation of stored/operational references.
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
