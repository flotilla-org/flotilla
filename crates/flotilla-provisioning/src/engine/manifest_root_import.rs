//! The import of old ManifestRoots onto the engine (#3824 layer 2c, ADR 0047).
//!
//! | Old | Engine |
//! |---|---|
//! | `spec.host`, `path`, `source` | the request, fixed |
//! | `spec.binding` | the state's `binding` |
//! | `status.applied_revision`, `source_error` | the state's `applied`, `source_error` |
//! | `status.documents[*].baseline_hash` | `baselines` |
//! | a Refused document's reason, since it was observed (the old status keeps `observed_at` while a document's state is unchanged) | `refusals` |
//! | `spec.suspended` | `suspended` |
//! | `spec.resolutions`, with the status's outcome for its token | `resolutions`: Requested until claimed |
//! | each document's phase, live and desired hashes, `stalled` | dropped: judgements |
//!
//! A document is named by the old `DocumentKey` encoding (a JSON tuple of path, kind, namespace and
//! name), so the reconciler finds its records under the same key.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{DocumentKey, DocumentPhase, ManifestRoot as OldRoot, ResolutionAction, ResolutionOutcome, ResourceObject};
use flotilla_schema::{State, Timestamp};
use flotilla_state_machines::{Resource, Version};

use super::import::{ImportRefusal, ImportReport};
use crate::stored::manifest_root::{
    self as stored, Act, DocumentName, ManifestRoot, Outcome, RefusalRecord, ResolutionRecord, RootSpec, RootState,
};

/// The engine's name of an old document key: the old status's map-key encoding, under which the
/// reconciler looks its records up. A key with no such encoding is refused, never renamed.
pub fn document_name(key: &DocumentKey) -> Result<DocumentName, String> {
    match serde_json::to_value(key) {
        Ok(serde_json::Value::String(encoded)) => Ok(DocumentName(encoded)),
        Ok(other) => Err(format!("document {}: key encodes as {other}, not text", key.path)),
        Err(error) => Err(format!("document {}: key does not encode: {error}", key.path)),
    }
}

/// Maps old root `old` onto the engine's ManifestRoot. Stored outcomes and reasons are carried as
/// the previous generation recorded them (ADR 0047), even ones the gates would refuse as a new
/// write, such as a failure with an empty reason: an import restores history, it does not replay it.
pub fn import_root(old: &ResourceObject<OldRoot>) -> Result<Resource<ManifestRoot>, String> {
    let status = old.status.clone().unwrap_or_default();
    let mut baselines = BTreeMap::new();
    let mut refusals = BTreeMap::new();
    for (key, document) in &status.documents {
        let name = document_name(key)?;
        if let Some(hash) = &document.baseline_hash {
            baselines.insert(name.clone(), hash.clone());
        }
        if document.phase == DocumentPhase::Refused {
            let reason = document.reason.clone().unwrap_or_else(|| "manifest document refused".to_owned());
            refusals.insert(name, RefusalRecord { reason, since: Timestamp::from_datetime(&document.observed_at) });
        }
    }
    let resolutions = old
        .spec
        .resolutions
        .iter()
        .map(|(key, resolution)| -> Result<_, String> {
            let settled =
                status.documents.get(key).filter(|document| document.resolved_token.as_deref() == Some(resolution.token.as_str()));
            let outcome = match settled.and_then(|document| document.resolution_outcome.clone()) {
                None => Outcome::Requested,
                Some(ResolutionOutcome::Started) => Outcome::Started,
                Some(ResolutionOutcome::Synced) => Outcome::Synced,
                Some(ResolutionOutcome::Adopted) => Outcome::Adopted,
                Some(ResolutionOutcome::Failed(reason)) => Outcome::Failed { reason },
            };
            let act = match resolution.action {
                ResolutionAction::Sync => Act::Sync,
                ResolutionAction::Adopt => Act::Adopt,
            };
            Ok((
                document_name(key)?,
                ResolutionRecord { act, token: resolution.token.clone(), by: resolution.requested_by.clone(), outcome },
            ))
        })
        .collect::<Result<_, _>>()?;
    let spec = RootSpec {
        host: old.spec.host.clone(),
        path: old.spec.path.clone(),
        source: old.spec.source.clone(),
        baselines,
        refusals,
        suspended: old.spec.suspended.iter().map(document_name).collect::<Result<BTreeSet<_>, _>>()?,
        resolutions,
    };
    let state = RootState::Bound {
        binding: old.spec.binding.clone(),
        applied: status.applied_revision.clone(),
        source_error: status.source_error.clone(),
    };
    let since = Timestamp::from_datetime(&old.metadata.creation_timestamp);
    Ok(Resource::stored(old.metadata.name.clone(), spec, State::new(state, since), Version(1)))
}

/// Maps every old root in `roots` (each with its home) onto the engine and rehearses them into a
/// temporary store, counted as `bound`; refusals are named.
pub fn rehearse_roots(roots: &[(ResourceObject<OldRoot>, String)], report: &mut ImportReport) {
    let rehearsal = stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(flotilla_engine_store::Rehearsal::open);
    let mut rehearsal = match rehearsal {
        Ok(rehearsal) => rehearsal,
        Err(reason) => {
            report.refused.extend(roots.iter().map(|(root, _)| ImportRefusal {
                kind: "ManifestRoot",
                record: root.metadata.name.clone(),
                reason: reason.clone(),
            }));
            return;
        }
    };
    for (old, home) in roots {
        let refuse = |reason: String| ImportRefusal { kind: "ManifestRoot", record: old.metadata.name.clone(), reason };
        let resource = match import_root(old) {
            Ok(resource) => resource,
            Err(reason) => {
                report.refused.push(refuse(reason));
                continue;
            }
        };
        match rehearsal.restore(home, &old.metadata.name, 1, &resource) {
            Ok(()) => *report.imported.entry(("ManifestRoot", "bound")).or_default() += 1,
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
