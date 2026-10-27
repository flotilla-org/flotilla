//! The declared definition kinds' records (#3824 layer 2b-ii): Forge, FleetDesignation,
//! CredentialSpec, CredentialGrant, CrewDefaults, ImageLayer and WorkflowTemplate.
//!
//! On the engine each is a plain record at the home that declared it, with a typed
//! [`DefinitionOrigin`] in place of the provenance labels and annotations the old store keeps
//! (#3212). [`DefinitionRecords`] is the engine's records as the old store's definition reads see
//! them; the store installs it behind `[primitives]`. [`origin`] reads an origin from a writer's
//! labels and annotations, the import's mapping and the write path's alike, and [`stamp`] is its
//! inverse, for readers that still look for the stamps (they move onto `origin` in 2b-ii-b).

use std::collections::BTreeMap;

use flotilla_schema::Stored;
use serde::{Deserialize, Serialize};

use crate::{
    definition_provenance::{
        CHARTER_REVISION_ANNOTATION, CHARTER_SCOPE_ANNOTATION, CHARTER_SOURCE_ANNOTATION, MANIFEST_MANAGED_BY_VALUE,
        MANIFEST_PATH_ANNOTATION, MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_REVISION_ANNOTATION, MANIFEST_SOURCE_ANNOTATION,
        MATERIALIZED_PROJECT_ANNOTATION, PROJECT_IMAGE_MANAGER, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION,
        SOURCE_REPOSITORY_ANNOTATION,
    },
    MANAGED_BY_LABEL, PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND,
};

/// The kinds the engine's definition records serve, by their resource kind names.
pub const DEFINITION_KINDS: [&str; 7] =
    ["Forge", "FleetDesignation", "CredentialSpec", "CredentialGrant", "CrewDefaults", "ImageLayer", "WorkflowTemplate"];

/// Whether `kind` is one of [`DEFINITION_KINDS`].
pub fn is_definition_kind(kind: &str) -> bool {
    DEFINITION_KINDS.contains(&kind)
}

/// Where a definition came from: the one writer that declared it, with what that writer needs to
/// find its own definitions again. It replaces the provenance labels and annotations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub enum DefinitionOrigin {
    /// Applied by an operator (`resource apply`, `workflow-template apply`), declared by no source.
    #[store(code = "op")]
    Operator,
    /// Declared by a document of a manifest tree, applied by its ManifestRoot's reconciler.
    #[store(code = "mf")]
    Manifest {
        /// The ManifestRoot whose reconciler applies it.
        root: String,
        /// The tree's source and the document's path and revision in it.
        source: String,
        path: String,
        revision: String,
        /// A charter document's source, revision and delegated scope.
        #[store(flatten)]
        charter: Option<CharterOrigin>,
        /// The project a charter operational entry materialises its workflow for.
        materialized_project: Option<String>,
    },
    /// Materialised by project operations from an operational entry in a project's repository.
    #[store(code = "po")]
    ProjectOps { project: String, repository: String, commit: String, entry_path: String },
    /// A project's image layer, read from its primary repository's `.flotilla/image/`.
    #[store(code = "pi")]
    ProjectImage { project: String, commit: String },
    /// A workflow snapshot admission prepared; its name is content-addressed.
    #[store(code = "sn")]
    Snapshot,
}

/// A charter document's provenance: the charter's source and revision, and the `namespace/project`
/// scope it was delegated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Stored)]
pub struct CharterOrigin {
    pub source: String,
    pub revision: String,
    pub scope: String,
}

/// Whether a writer's stamps say a manifest declares the definition. Project operations also stamp
/// the reconciler root on what they materialise from an ops root, but never a manifest source: a
/// definition with a source entry path and no manifest source is project ops'.
pub fn declares(labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>) -> bool {
    let project_ops = annotations.contains_key(SOURCE_ENTRY_PATH_ANNOTATION) && !annotations.contains_key(MANIFEST_SOURCE_ANNOTATION);
    labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == MANIFEST_MANAGED_BY_VALUE)
        || (annotations.contains_key(MANIFEST_RECONCILER_ROOT_ANNOTATION) && !project_ops)
}

/// The origin a writer stamped as `labels` and `annotations`. Stamps naming an origin without all
/// of its fields are refused with the missing key named.
pub fn origin(labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>) -> Result<DefinitionOrigin, String> {
    let annotation = |key: &str| annotations.get(key).cloned().ok_or_else(|| format!("no `{key}` annotation"));
    let managed_by = labels.get(MANAGED_BY_LABEL).map(String::as_str);
    // Precedence: admission alone writes a snapshot, under a content-addressed name no manifest
    // declares, so the snapshot label is read first; then a project image, a manifest and project ops.
    if labels.get(PREPARED_SNAPSHOT_LABEL).is_some_and(|value| value == WORKFLOW_SNAPSHOT_KIND) {
        return Ok(DefinitionOrigin::Snapshot);
    }
    if managed_by == Some(PROJECT_IMAGE_MANAGER) {
        return Ok(DefinitionOrigin::ProjectImage {
            project: annotation(MATERIALIZED_PROJECT_ANNOTATION)?,
            commit: annotation(SOURCE_COMMIT_ANNOTATION)?,
        });
    }
    if declares(labels, annotations) {
        let charter = [CHARTER_SOURCE_ANNOTATION, CHARTER_REVISION_ANNOTATION, CHARTER_SCOPE_ANNOTATION]
            .iter()
            .any(|key| annotations.contains_key(*key))
            .then(|| {
                Ok::<_, String>(CharterOrigin {
                    source: annotation(CHARTER_SOURCE_ANNOTATION)?,
                    revision: annotation(CHARTER_REVISION_ANNOTATION)?,
                    scope: annotation(CHARTER_SCOPE_ANNOTATION)?,
                })
            })
            .transpose()?;
        return Ok(DefinitionOrigin::Manifest {
            root: annotation(MANIFEST_RECONCILER_ROOT_ANNOTATION)?,
            source: annotation(MANIFEST_SOURCE_ANNOTATION)?,
            path: annotation(MANIFEST_PATH_ANNOTATION)?,
            revision: annotation(MANIFEST_REVISION_ANNOTATION)?,
            charter,
            materialized_project: annotations.get(MATERIALIZED_PROJECT_ANNOTATION).cloned(),
        });
    }
    if annotations.contains_key(MATERIALIZED_PROJECT_ANNOTATION) {
        return Ok(DefinitionOrigin::ProjectOps {
            project: annotation(MATERIALIZED_PROJECT_ANNOTATION)?,
            repository: annotation(SOURCE_REPOSITORY_ANNOTATION)?,
            commit: annotation(SOURCE_COMMIT_ANNOTATION)?,
            entry_path: annotation(SOURCE_ENTRY_PATH_ANNOTATION)?,
        });
    }
    Ok(DefinitionOrigin::Operator)
}

/// The labels and annotations `origin`'s writer stamps, which [`origin`] reads back as `origin`.
pub fn stamp(origin: &DefinitionOrigin) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let pairs = |items: &[(&str, &str)]| items.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
    match origin {
        DefinitionOrigin::Operator => (BTreeMap::new(), BTreeMap::new()),
        DefinitionOrigin::Manifest { root, source, path, revision, charter, materialized_project } => {
            let mut annotations: BTreeMap<String, String> = pairs(&[
                (MANIFEST_RECONCILER_ROOT_ANNOTATION, root),
                (MANIFEST_SOURCE_ANNOTATION, source),
                (MANIFEST_PATH_ANNOTATION, path),
                (MANIFEST_REVISION_ANNOTATION, revision),
            ]);
            if let Some(charter) = charter {
                annotations.extend(pairs(&[
                    (CHARTER_SOURCE_ANNOTATION, &charter.source),
                    (CHARTER_REVISION_ANNOTATION, &charter.revision),
                    (CHARTER_SCOPE_ANNOTATION, &charter.scope),
                ]));
            }
            if let Some(project) = materialized_project {
                annotations.insert(MATERIALIZED_PROJECT_ANNOTATION.into(), project.clone());
            }
            (pairs(&[(MANAGED_BY_LABEL, MANIFEST_MANAGED_BY_VALUE)]), annotations)
        }
        DefinitionOrigin::ProjectOps { project, repository, commit, entry_path } => (
            BTreeMap::new(),
            pairs(&[
                (MATERIALIZED_PROJECT_ANNOTATION, project),
                (SOURCE_REPOSITORY_ANNOTATION, repository),
                (SOURCE_COMMIT_ANNOTATION, commit),
                (SOURCE_ENTRY_PATH_ANNOTATION, entry_path),
            ]),
        ),
        DefinitionOrigin::ProjectImage { project, commit } => (
            pairs(&[(MANAGED_BY_LABEL, PROJECT_IMAGE_MANAGER)]),
            pairs(&[(MATERIALIZED_PROJECT_ANNOTATION, project), (SOURCE_COMMIT_ANNOTATION, commit)]),
        ),
        DefinitionOrigin::Snapshot => (pairs(&[(PREPARED_SNAPSHOT_LABEL, WORKFLOW_SNAPSHOT_KIND)]), BTreeMap::new()),
    }
}

/// One definition as its home holds it: the resource kind's serde spec, as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionRecord {
    pub name: String,
    /// The home that declared it, its only writer.
    pub home: String,
    pub origin: DefinitionOrigin,
    pub spec: serde_json::Value,
    /// The record's version at its home; it moves with each edit.
    pub version: u64,
}

/// The engine's definition records, one kind at a time by its resource kind name. A name several
/// homes declare (a host's manifest on each host) reads as this home's record, else the
/// lexically first home's. Writes are this home's alone.
#[async_trait::async_trait]
pub trait DefinitionRecords: Send + Sync + std::fmt::Debug {
    /// This home: the writer of the records it holds.
    fn home(&self) -> String;
    /// `kind`'s record `name`, as [`DefinitionRecords`] resolves several homes'.
    async fn get(&self, kind: &str, name: &str) -> Result<Option<DefinitionRecord>, String>;
    /// Every name of `kind`, each resolved as [`DefinitionRecords::get`].
    async fn list(&self, kind: &str) -> Result<Vec<DefinitionRecord>, String>;
    /// Writes this home's record `name` of `kind`: created, or its spec and origin replaced.
    async fn apply(&self, kind: &str, name: &str, origin: DefinitionOrigin, spec: serde_json::Value) -> Result<DefinitionRecord, String>;
    /// Deletes this home's record `name` of `kind`; absent is done. Another home's copy still reads.
    async fn delete(&self, kind: &str, name: &str) -> Result<(), String>;
    /// A tick for each pass that found any home's definition records changed since the pass
    /// before, from the moment it is called; watchers read the records again on each.
    async fn changes(&self) -> Result<futures::stream::BoxStream<'static, ()>, String>;
}

#[cfg(test)]
mod tests {
    use hegel::generators as gs;

    use super::*;

    fn text(tc: &hegel::TestCase) -> String {
        // Values are opaque to the mapping; the empty string is one too.
        tc.draw(gs::sampled_from(vec!["", "p", "ops-00aa", "a.yaml", "flotilla/p", "r1"])).to_owned()
    }

    // Behaviour (#3824 layer 2b-ii): every origin a writer can declare survives its stamps, so a
    // definition written with an origin reads back with it through the labels and annotations.
    #[hegel::test]
    fn every_origin_reads_back_from_its_stamps(tc: hegel::TestCase) {
        let origin = match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
            0 => DefinitionOrigin::Operator,
            1 => DefinitionOrigin::Manifest {
                root: text(&tc),
                source: text(&tc),
                path: text(&tc),
                revision: text(&tc),
                charter: tc.draw(gs::booleans()).then(|| CharterOrigin { source: text(&tc), revision: text(&tc), scope: text(&tc) }),
                materialized_project: tc.draw(gs::booleans()).then(|| text(&tc)),
            },
            2 => DefinitionOrigin::ProjectOps { project: text(&tc), repository: text(&tc), commit: text(&tc), entry_path: text(&tc) },
            3 => DefinitionOrigin::ProjectImage { project: text(&tc), commit: text(&tc) },
            _ => DefinitionOrigin::Snapshot,
        };
        let (labels, annotations) = stamp(&origin);
        assert_eq!(super::origin(&labels, &annotations), Ok(origin));
    }
}
