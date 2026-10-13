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
#[serde(from = "CrewImageBaselineRecord")]
pub struct CrewImageBaselineSpec {
    pub image: String,
}

/// Generation 1 recorded a layer selection alongside the authoritative literal
/// image; it is accepted and dropped. ADR 0047: remove `layers` one roll after
/// ADR 0062 images land.
#[derive(Deserialize)]
struct CrewImageBaselineRecord {
    image: String,
    #[serde(default, rename = "layers")]
    _layers: Option<serde::de::IgnoredAny>,
}

impl From<CrewImageBaselineRecord> for CrewImageBaselineSpec {
    fn from(record: CrewImageBaselineRecord) -> Self {
        Self { image: record.image }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADR 0047: a generation-1 baseline with a layer selection decodes and
    // rewrites without it.
    #[test]
    fn retired_layer_selection_is_dropped() {
        let spec: CrewImageBaselineSpec = serde_json::from_value(serde_json::json!({
            "image": "registry.test/crew@sha256:abc",
            "layers": {"base": "base", "utilities": "utils", "harness": null, "project": null}
        }))
        .expect("old baseline decodes");
        assert_eq!(spec.image, "registry.test/crew@sha256:abc");
        assert_eq!(serde_json::to_value(&spec).expect("encode"), serde_json::json!({"image": "registry.test/crew@sha256:abc"}));
    }
}
