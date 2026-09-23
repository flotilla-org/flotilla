use serde::{Deserialize, Serialize};

use crate::{resource::define_resource, NoStatusPatch, ReplicationClass};

// Image selection is fleet resolver configuration, not host runtime state.
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
}
