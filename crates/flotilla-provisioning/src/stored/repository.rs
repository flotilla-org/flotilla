//! Repository on the engine (#3824 layer 3a): a plain record (a spec and no machine), one per
//! home that observes the repository, written by that home alone.
//!
//! The old store keeps Repository as a convergent fact: every host that observes a repository
//! writes the same record, named by its identity's key, and the Repository reconciler recomputes
//! its status. On the engine each observing home writes its own record, with what it observed
//! (the default branch) beside what it configured: the observation ruling's observer-written
//! records. The repository spec has a canonicalising decoder and private fields, so it is stored
//! as one frozen document; the columns are the ones a read selects by.
//!
//! The old status's `checkouts_by_host` and `diagnostics` have no production reader and are not
//! kept. Nothing reads or writes these tables yet: the import rehearsal restores into them, and
//! they join the daemon's decided store with Repository's writers and readers (3a-ii).

use flotilla_resources::{DefaultBranchObservation, RepositorySpec};
use flotilla_schema::{Frozen, KindRegistry, RegistryError, Stored};

/// The operational entry that declared a verification command: the old store's
/// `verification-command-provenance` annotation, typed. Its serde shape is the annotation's JSON.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationProvenance {
    pub source_repository: String,
    pub source_commit: String,
    pub entry_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_root: Option<String>,
}

/// A home's record of one repository: kind code `ry`, named by the repository's key.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "Repository", code = "ry", index(name = "by_verification_project", eq(verification_project)))]
pub struct RepositoryRecord {
    /// The repository as this home declares it: identity, remotes, forge and settings.
    #[store(frozen)]
    pub spec: Frozen<RepositorySpec>,
    /// The project whose operational entries declare the spec's verification commands; project
    /// operations find the repositories a project materialised by it.
    pub verification_project: Option<String>,
    /// Where each of those commands was declared, a document read whole.
    #[store(cbor)]
    pub verification_provenance: Vec<VerificationProvenance>,
    /// The record that supersedes this one after the forge-split sweep merged it there.
    pub superseded_by: Option<String>,
    /// The default branch this home's observations resolve to.
    pub default_branch: Option<String>,
    /// Each source's observation of the default branch, a document read whole.
    #[store(cbor)]
    pub default_branch_observations: Vec<DefaultBranchObservation>,
}

impl RepositoryRecord {
    /// A record of `spec` with no ops data or observations.
    pub fn new(spec: RepositorySpec) -> Result<Self, String> {
        Ok(Self {
            spec: Frozen::new(spec).map_err(|error| format!("freeze the repository: {error}"))?,
            verification_project: None,
            verification_provenance: Vec::new(),
            superseded_by: None,
            default_branch: None,
            default_branch_observations: Vec::new(),
        })
    }

    pub fn spec(&self) -> &RepositorySpec {
        self.spec.value()
    }
}

/// Registers Repository into `registry` under its code, `ry`: a plain kind.
pub fn register(registry: &mut KindRegistry) -> Result<(), RegistryError> {
    registry.register::<RepositoryRecord>()
}

/// Repository alone, for its DDL snapshot.
pub fn registry() -> Result<KindRegistry, RegistryError> {
    let mut registry = KindRegistry::new();
    register(&mut registry)?;
    Ok(registry)
}
