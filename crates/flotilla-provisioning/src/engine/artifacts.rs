//! Artifact on the engine, behind `[primitives]`: each artifact is written at its home, the
//! convoy's home for a crew's put and the builder's for a build log.

use std::collections::BTreeMap;

use flotilla_resources::{engine_records::EngineObject, ArtifactSpec};
use flotilla_schema::Kind;
use serde_json::Value;

use super::plain_records::{Held, PlainKind, PlainRecords};
use crate::stored::artifact::ArtifactRecord;

/// The artifact records of the decided store at one home.
pub type EngineArtifacts = PlainRecords<ArtifactRecord>;

impl PlainKind for ArtifactRecord {
    const WRITER: &'static str = "artifact-records";

    fn object(name: String, home: String, version: u64, record: &Self) -> Held {
        let kind = <Self as Kind>::KIND;
        let spec = record.spec().map_err(|error| format!("{kind} {name} at {home}: {error}"))?;
        Ok(EngineObject {
            spec: serde_json::to_value(spec).map_err(|error| format!("{kind} {name} at {home}: {error}"))?,
            status: None,
            name,
            home,
            version,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        })
    }

    fn from_spec(spec: Value) -> Result<Self, String> {
        let spec: ArtifactSpec = serde_json::from_value(spec).map_err(|error| format!("{} spec: {error}", <Self as Kind>::KIND))?;
        ArtifactRecord::from_spec(&spec)
    }
}
