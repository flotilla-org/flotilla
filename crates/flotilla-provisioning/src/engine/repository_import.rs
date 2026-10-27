//! Repository's import onto the engine (#3824 layer 3a, ADR 0047).
//!
//! The old store keeps one copy of a repository per home that observed it. Each home's live copy
//! imports to a record at that home:
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | Superseded by the forge-split sweep (`superseded-by`) | dropped: its record is the one it names; refused when that home holds no live copy of it (an interrupted sweep) |
//! | Named other than its identity's key | refused |
//! | `spec` | the record's spec, unchanged |
//! | `verification-command-project` and `-provenance` annotations | `verification_project`, `verification_provenance` |
//! | `status.default_branch` and `default_branch_observations` | carried as observed |
//! | `status.checkouts_by_host` and `diagnostics`, other labels and annotations | dropped: no production reader |
//!
//! The engine has no namespaces (#3212), so one key held in two namespaces at one home is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{
    definition_provenance::{SUPERSEDED_BY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION},
    Repository, RepositoryKey, ResourceObject,
};

use super::import::{ImportRefusal, ImportReport};
use crate::stored::repository::{self as stored, RepositoryRecord, VerificationProvenance};

const KIND: &str = "Repository";

/// One old copy of a repository: its namespace, the object, and the home that wrote it.
pub struct RepositoryCopy {
    pub namespace: String,
    pub object: ResourceObject<Repository>,
    pub home: String,
}

/// What an old copy imports to.
pub enum Imported {
    Record(Box<RepositoryRecord>),
    /// Not carried, for the reason named.
    Dropped(&'static str),
}

/// The record `old` imports to, or why it is dropped; refused with the reason.
pub fn import_repository(old: &ResourceObject<Repository>) -> Result<Imported, String> {
    if old.metadata.deletion_timestamp.is_some() {
        return Ok(Imported::Dropped("deleted"));
    }
    if old.metadata.annotations.contains_key(SUPERSEDED_BY_ANNOTATION) {
        return Ok(Imported::Dropped("superseded by the forge-split sweep"));
    }
    old.spec.verify_key(&RepositoryKey(old.metadata.name.clone()))?;
    let verification_provenance = match old.metadata.annotations.get(VERIFICATION_PROVENANCE_ANNOTATION) {
        Some(json) => serde_json::from_str::<Vec<VerificationProvenance>>(json)
            .map_err(|error| format!("its `{VERIFICATION_PROVENANCE_ANNOTATION}` annotation does not decode: {error}"))?,
        None => Vec::new(),
    };
    let mut record = RepositoryRecord::new(old.spec.clone())?;
    record.verification_project = old.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).cloned();
    record.verification_provenance = verification_provenance;
    if let Some(status) = &old.status {
        record.default_branch.clone_from(&status.default_branch);
        record.default_branch_observations.clone_from(&status.default_branch_observations);
    }
    Ok(Imported::Record(Box::new(record)))
}

/// Maps every copy onto the engine and rehearses the records into a temporary store, counted as
/// `observed`; drops are counted and refusals named in `report`.
pub fn rehearse_repositories(copies: &[RepositoryCopy], report: &mut ImportReport) {
    let rehearsal = stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(flotilla_engine_store::Rehearsal::open);
    let mut rehearsal = match rehearsal {
        Ok(rehearsal) => rehearsal,
        Err(reason) => {
            report.refused.push(ImportRefusal { kind: KIND, record: "<all>".to_owned(), reason });
            return;
        }
    };
    // The engine has no namespaces: a key a home holds in several is refused in all of them.
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for copy in copies {
        namespaces.entry((copy.object.metadata.name.as_str(), copy.home.as_str())).or_default().insert(copy.namespace.as_str());
    }
    // The live copies each home holds, which a superseded copy's target must be among.
    let live: BTreeSet<(&str, &str)> = copies
        .iter()
        .filter(|copy| {
            copy.object.metadata.deletion_timestamp.is_none() && !copy.object.metadata.annotations.contains_key(SUPERSEDED_BY_ANNOTATION)
        })
        .map(|copy| (copy.object.metadata.name.as_str(), copy.home.as_str()))
        .collect();
    for copy in copies {
        let name = &copy.object.metadata.name;
        let refuse = |reason: String| ImportRefusal { kind: KIND, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        let held = &namespaces[&(name.as_str(), copy.home.as_str())];
        if held.len() > 1 {
            let held: Vec<_> = held.iter().copied().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        if let Some(target) = copy.object.metadata.annotations.get(SUPERSEDED_BY_ANNOTATION) {
            if copy.object.metadata.deletion_timestamp.is_none() && !live.contains(&(target.as_str(), copy.home.as_str())) {
                report.refused.push(refuse(format!("superseded by `{target}`, which this home holds no live copy of")));
                continue;
            }
        }
        match import_repository(&copy.object) {
            // Version 1: each record starts at its home as a fresh write; its old versions are not carried.
            Ok(Imported::Record(record)) => match rehearsal.restore(&copy.home, name, 1, &*record) {
                Ok(()) => *report.imported.entry((KIND, "observed")).or_default() += 1,
                Err(reason) => report.refused.push(refuse(reason)),
            },
            Ok(Imported::Dropped(reason)) => *report.dropped.entry((KIND, reason)).or_default() += 1,
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
