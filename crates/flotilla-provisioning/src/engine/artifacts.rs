//! Artifact on the engine, behind `[primitives]`: each artifact is written at its home, the
//! convoy's home for a crew's put and the builder's for a build log.

use flotilla_resources::{Artifact, ArtifactSpec};

use super::plain_records::{PlainKind, PlainRecords};
use crate::stored::artifact::ArtifactRecord;

/// The artifact records of the decided store at one home.
pub type EngineArtifacts = PlainRecords<ArtifactRecord>;

impl PlainKind for ArtifactRecord {
    type Resource = Artifact;

    const WRITER: &'static str = "artifact-records";

    fn spec(&self) -> Result<ArtifactSpec, String> {
        ArtifactRecord::spec(self)
    }

    fn from_spec(spec: ArtifactSpec) -> Result<Self, String> {
        ArtifactRecord::from_spec(&spec)
    }
}
