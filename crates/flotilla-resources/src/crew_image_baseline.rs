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

impl CrewImageBaseline {
    /// Resolve image and layer selection from one merged revision, refusing
    /// deleted or conflicted definitions rather than choosing a merge winner.
    pub async fn resolve(name: &str, baselines: &crate::DefinitionResolver<Self>) -> Result<CrewImageBaselineSpec, String> {
        let baseline = baselines.get(name).await.map_err(|error| format!("image-baseline `{name}` missing/unresolved: {error}"))?;
        let unresolved = if baseline.metadata.deletion_timestamp.is_some() {
            Some("baseline is deleted")
        } else if baseline.metadata.merge.as_ref().is_some_and(|merge| !merge.conflicts.is_empty()) {
            Some("baseline has unresolved merge conflicts")
        } else if baseline.spec.image.trim().is_empty() {
            Some("baseline image is empty")
        } else {
            None
        };
        if let Some(reason) = unresolved {
            return Err(format!("image-baseline `{name}` missing/unresolved: {reason}"));
        }
        Ok(baseline.spec)
    }
}
