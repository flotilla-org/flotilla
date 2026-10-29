//! Artifact's import onto the engine (ADR 0047).
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | `spec` | the record's columns, times to the microsecond; the summary as one document |
//! | A summary value that is null, an array or an object | refused: a put accepts only strings, numbers and booleans |
//! | A `size` above `i64::MAX` | refused: stored integers are signed and never wrap |
//! | `status.ledger_comment_creations` | dropped: nothing asks for a reservation any more |
//! | Owner references, labels and annotations | dropped (#3212) |
//!
//! Each copy imports at the home that wrote it. The engine has no namespaces, so one name a home
//! holds in two namespaces is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{Artifact, ResourceObject};

use flotilla_engine_store::ImportTarget;

use super::import::{created, ImportRefusal, ImportReport};
use crate::stored::artifact::{self as stored, ArtifactRecord};

const KIND: &str = "Artifact";

/// One old copy of an artifact: its namespace, the object, and the home that wrote it.
pub struct ArtifactCopy {
    pub namespace: String,
    pub object: ResourceObject<Artifact>,
    pub home: String,
}

/// What an old copy imports to.
pub enum Imported {
    Record(Box<ArtifactRecord>),
    /// Not carried, for the reason named.
    Dropped(&'static str),
}

/// The record `old` imports to, or why it is dropped; refused with the reason.
pub fn import_artifact(old: &ResourceObject<Artifact>) -> Result<Imported, String> {
    if old.metadata.deletion_timestamp.is_some() {
        return Ok(Imported::Dropped("deleted"));
    }
    ArtifactRecord::from_spec(&old.spec).map(|record| Imported::Record(Box::new(record)))
}

/// [`import_artifacts`] into a temporary store of their own.
pub fn rehearse_artifacts(copies: &[ArtifactCopy], report: &mut ImportReport) {
    match stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(flotilla_engine_store::Rehearsal::open) {
        Ok(mut rehearsal) => import_artifacts(copies, &mut rehearsal, report),
        Err(reason) => report.refused.push(ImportRefusal { kind: KIND, record: "<all>".to_owned(), reason }),
    }
}

/// Maps every copy at a home `target` imports onto the engine and writes the records, counted as
/// `stored`, each at its copy's creation time; drops are counted and refusals named in `report`.
pub fn import_artifacts(copies: &[ArtifactCopy], target: &mut impl ImportTarget, report: &mut ImportReport) {
    let copies: Vec<_> = copies.iter().filter(|copy| target.imports(&copy.home)).collect();
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for copy in copies.iter().filter(|copy| copy.object.metadata.deletion_timestamp.is_none()) {
        namespaces.entry((copy.object.metadata.name.as_str(), copy.home.as_str())).or_default().insert(copy.namespace.as_str());
    }
    for copy in &copies {
        let name = &copy.object.metadata.name;
        let refuse = |reason: String| ImportRefusal { kind: KIND, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        let held = namespaces.get(&(name.as_str(), copy.home.as_str())).cloned().unwrap_or_default();
        if held.len() > 1 {
            let held: Vec<_> = held.iter().copied().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        match import_artifact(&copy.object) {
            // Each record starts at its home as a fresh write; its old versions are not carried.
            Ok(Imported::Record(record)) => match target.write(&copy.home, name, &created(&copy.object), &*record) {
                Ok(()) => {
                    *report.imported.entry((KIND, "stored")).or_default() += 1;
                    if copy.object.status.as_ref().is_some_and(|status| !status.ledger_comment_creations.is_empty()) {
                        *report.dropped.entry((KIND, "ledger comment reservations")).or_default() += 1;
                    }
                }
                Err(reason) => report.refused.push(refuse(reason)),
            },
            Ok(Imported::Dropped(reason)) => *report.dropped.entry((KIND, reason)).or_default() += 1,
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
