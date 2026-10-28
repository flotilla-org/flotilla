//! The import of the declared definition kinds onto the engine (#3824 layer 2b, ADR 0047).
//!
//! The old store keeps one copy of a definition per home that wrote it and merges the copies at
//! every read. The engine keeps one writer per record, so the import carries each definition's
//! merged value, which is what readers see today, to the home that declares it:
//!
//! | Old copies of one name | Imported |
//! |---|---|
//! | Under a built-in's name (layer 2a) | dropped: the built-in is served from the code |
//! | Deleted (the merge reads it as deleted) | dropped |
//! | A retired ADR 0053 image layer | dropped: inert for resolution |
//! | With a field still in merge conflict | the merged value readers see, noted with its sibling values |
//! | Declared by a manifest at one or more homes | one record at each declaring home |
//! | Declared by none | one record at the home that created it; the other copies fold into it |
//!
//! Project (layer 3b) imports alike, with its project operations' status as records of their own
//! ([`super::project_import`]).
//!
//! Each record's [`DefinitionOrigin`] comes from its own home's copy: the labels and annotations
//! its writer stamped. A copy that names an origin but lacks one of its fields is refused. The
//! record holds only its own fields (#4024); the origin goes with it as the import's change, and
//! a workflow template's materialised project becomes its `project` column. The
//! manifest's baseline and last-applied hashes are not carried: the ManifestRoot import (layer 2c)
//! takes the baselines from the root's status, which holds the same hashes.
//!
//! The engine has no namespaces (#3212), so one name held in two namespaces at one home is refused.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::Rehearsal;
use flotilla_protocol::NodeId;
use flotilla_resources::{
    builtins::is_builtin,
    definitions::{declares, DefinitionOrigin},
    CredentialGrant, CredentialSpec, CrewDefaults, FleetDesignation, Forge, ImageLayer, ImageLayerResourceSpec, ObjectMeta, Project,
    Resource, ResourceObject, WorkflowTemplate, WorkflowTemplateSpec,
};
use flotilla_schema::Kind;
use flotilla_store::{definition_is_live, merge_definition_copies};

use super::{
    import::{ImportRefusal, ImportReport},
    project_import::rehearse_project_ops,
};
use crate::stored::{
    definition::{
        self as stored, CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition,
        ForgeDefinition, ImageLayerDefinition, WorkflowTemplateDefinition,
    },
    project::{self, ProjectDefinition},
};

/// One old copy of a definition: its namespace, the object, and the home that wrote it.
pub struct DefinitionCopy<R: Resource> {
    pub namespace: String,
    pub object: ResourceObject<R>,
    pub home: String,
}

/// What an old spec maps to on the engine.
pub enum Mapped<S> {
    Stored(S),
    /// Not carried, for the reason named.
    Dropped(&'static str),
}

/// A definition kind the import maps: its old resource and its engine kind.
pub trait DefinitionKind {
    type Old: Resource;
    type Stored: Kind + PartialEq;
    /// The engine record of `spec`, materialised for `project` when a template is.
    fn map(project: Option<String>, spec: &<Self::Old as Resource>::Spec) -> Result<Mapped<Self::Stored>, String>;
}

/// A kind whose every old spec maps onto one engine record.
macro_rules! whole_definition {
    ($($old:ty => $stored:ty),* $(,)?) => {$(
        impl DefinitionKind for $stored {
            type Old = $old;
            type Stored = $stored;
            fn map(_: Option<String>, spec: &<$old as Resource>::Spec) -> Result<Mapped<$stored>, String> {
                Ok(Mapped::Stored(<$stored>::new(spec)))
            }
        }
    )*};
}

whole_definition!(
    Forge => ForgeDefinition,
    FleetDesignation => FleetDesignationDefinition,
    CredentialSpec => CredentialSpecDefinition,
    CredentialGrant => CredentialGrantDefinition,
    CrewDefaults => CrewDefaultsDefinition,
);

impl DefinitionKind for ImageLayerDefinition {
    type Old = ImageLayer;
    type Stored = ImageLayerDefinition;
    fn map(_: Option<String>, spec: &ImageLayerResourceSpec) -> Result<Mapped<ImageLayerDefinition>, String> {
        match spec {
            ImageLayerResourceSpec::Current(layer) => ImageLayerDefinition::new(layer).map(Mapped::Stored),
            ImageLayerResourceSpec::Retired => Ok(Mapped::Dropped("retired ADR 0053 layer")),
        }
    }
}

impl DefinitionKind for WorkflowTemplateDefinition {
    type Old = WorkflowTemplate;
    type Stored = WorkflowTemplateDefinition;
    fn map(project: Option<String>, spec: &WorkflowTemplateSpec) -> Result<Mapped<WorkflowTemplateDefinition>, String> {
        WorkflowTemplateDefinition::new(project, spec).map(Mapped::Stored)
    }
}

/// The old copies of every definition kind the import carries.
#[derive(Default)]
pub struct DefinitionCopies {
    pub forges: Vec<DefinitionCopy<Forge>>,
    pub fleet_designations: Vec<DefinitionCopy<FleetDesignation>>,
    pub credential_specs: Vec<DefinitionCopy<CredentialSpec>>,
    pub credential_grants: Vec<DefinitionCopy<CredentialGrant>>,
    pub crew_defaults: Vec<DefinitionCopy<CrewDefaults>>,
    pub image_layers: Vec<DefinitionCopy<ImageLayer>>,
    pub workflow_templates: Vec<DefinitionCopy<WorkflowTemplate>>,
    /// Project (layer 3b), with its ProjectOps records from the copies' statuses.
    pub projects: Vec<DefinitionCopy<Project>>,
}

/// Maps every copy in `copies` onto the engine, restores each record into one temporary store
/// holding the definition kinds and Project's, reads it back, and reports counts by kind and
/// origin, the drops by kind and reason, and each refusal.
pub fn rehearse_definitions(copies: &DefinitionCopies) -> ImportReport {
    let mut report = ImportReport::default();
    let registry = stored::registry().and_then(|mut registry| project::register(&mut registry).map(|()| registry));
    let mut rehearsal = match registry.map_err(|error| format!("registry: {error:?}")).and_then(Rehearsal::open) {
        Ok(rehearsal) => rehearsal,
        Err(reason) => {
            report.refused.push(ImportRefusal { kind: "definitions", record: "<all>".to_owned(), reason });
            return report;
        }
    };
    rehearse_kind::<ForgeDefinition>(&copies.forges, &mut rehearsal, &mut report);
    rehearse_kind::<FleetDesignationDefinition>(&copies.fleet_designations, &mut rehearsal, &mut report);
    rehearse_kind::<CredentialSpecDefinition>(&copies.credential_specs, &mut rehearsal, &mut report);
    rehearse_kind::<CredentialGrantDefinition>(&copies.credential_grants, &mut rehearsal, &mut report);
    rehearse_kind::<CrewDefaultsDefinition>(&copies.crew_defaults, &mut rehearsal, &mut report);
    rehearse_kind::<ImageLayerDefinition>(&copies.image_layers, &mut rehearsal, &mut report);
    rehearse_kind::<WorkflowTemplateDefinition>(&copies.workflow_templates, &mut rehearsal, &mut report);
    let projects = rehearse_kind::<ProjectDefinition>(&copies.projects, &mut rehearsal, &mut report);
    rehearse_project_ops(&copies.projects, &projects, &mut rehearsal, &mut report);
    report
}

/// One engine record the import makes: its namespace, name, home and value.
pub struct ImportedDefinition<S> {
    pub namespace: String,
    pub name: String,
    pub home: String,
    pub record: S,
    /// Why its home's copy was written: the import's change for it.
    pub origin: DefinitionOrigin,
}

/// Maps the copies of `D`'s kind onto engine records, one group of copies per namespace and name.
/// Drops are counted and refusals listed in `report`.
pub fn import_kind<D: DefinitionKind>(copies: &[DefinitionCopy<D::Old>], report: &mut ImportReport) -> Vec<ImportedDefinition<D::Stored>>
where
    ResourceObject<D::Old>: Clone,
{
    let kind = <D::Stored as Kind>::KIND;
    let mut by_name = BTreeMap::<(&str, &str), Vec<&DefinitionCopy<D::Old>>>::new();
    for copy in copies {
        by_name.entry((copy.namespace.as_str(), copy.object.metadata.name.as_str())).or_default().push(copy);
    }
    let mut imported = Vec::new();
    for ((namespace, name), copies) in by_name {
        let refuse = |reason: String| ImportRefusal { kind, record: name.to_owned(), reason };
        if is_builtin(kind, name) {
            *report.dropped.entry((kind, "built-in, served from the code")).or_default() += copies.len();
            continue;
        }
        let merged = match merge_definition_copies(
            copies.iter().map(|copy| (copy.object.clone(), NodeId::new(copy.home.clone()))).collect::<Vec<_>>(),
        ) {
            Ok(merged) => merged,
            Err(error) => {
                report.refused.push(refuse(format!("its copies do not merge: {error}")));
                continue;
            }
        };
        if let Some(merge) = &merged.metadata.merge {
            let spec = serde_json::to_value(&merged.spec).unwrap_or_default();
            for (field, siblings) in &merge.conflicts {
                let chosen = field.strip_prefix("spec.").and_then(|field| spec.get(field)).map_or("absent".to_owned(), ToString::to_string);
                let siblings: Vec<_> =
                    siblings.iter().map(|sibling| format!("{} from {}", sibling.value, sibling.dot.author_root)).collect();
                report.notes.push(format!(
                    "{kind} `{name}`: `{field}` is in merge conflict; imported the merged value {chosen}, of the sibling values {}",
                    siblings.join(", ")
                ));
            }
        }
        if !definition_is_live(&merged) {
            *report.dropped.entry((kind, "deleted")).or_default() += copies.len();
            continue;
        }
        let live: Vec<_> = copies.iter().copied().filter(|copy| copy.object.metadata.deletion_timestamp.is_none()).collect();
        let declaring: Vec<_> =
            live.iter().copied().filter(|copy| declares(&copy.object.metadata.labels, &copy.object.metadata.annotations)).collect();
        let homes = if declaring.is_empty() {
            // An undeclared name stays with the home that created it.
            live.iter()
                .copied()
                .min_by(|a, b| (a.object.metadata.creation_timestamp, &a.home).cmp(&(b.object.metadata.creation_timestamp, &b.home)))
                .into_iter()
                .collect()
        } else {
            declaring
        };
        if homes.is_empty() {
            // The merge reads the name as live, yet every copy is deleted: nothing to carry.
            report.refused.push(refuse("the merge reads it as live, but every copy is deleted".to_owned()));
            continue;
        }
        let folded: BTreeSet<_> =
            copies.iter().map(|copy| copy.home.as_str()).filter(|home| !homes.iter().any(|kept| kept.home == *home)).collect();
        if !folded.is_empty() {
            let kept: Vec<_> = homes.iter().map(|copy| copy.home.as_str()).collect();
            report.notes.push(format!(
                "{kind} `{name}`: the copies at {} fold into the record at {}",
                folded.into_iter().collect::<Vec<_>>().join(", "),
                kept.join(", ")
            ));
        }
        for copy in homes {
            let origin = match origin(&copy.object.metadata) {
                Ok(origin) => origin,
                Err(reason) => {
                    report.refused.push(refuse(format!("at `{}`: {reason}", copy.home)));
                    continue;
                }
            };
            match D::map(super::provenance::materialized_project(&origin), &merged.spec) {
                Ok(Mapped::Stored(record)) => imported.push(ImportedDefinition {
                    namespace: namespace.to_owned(),
                    name: name.to_owned(),
                    home: copy.home.clone(),
                    record,
                    origin,
                }),
                Ok(Mapped::Dropped(reason)) => {
                    // The spec is the merged one, so every home's record drops alike: count the copies once.
                    *report.dropped.entry((kind, reason)).or_default() += copies.len();
                    break;
                }
                Err(reason) => report.refused.push(refuse(format!("at `{}`: {reason}", copy.home))),
            }
        }
    }
    // The engine has no namespaces: a name a home holds in several is refused in all of them,
    // so no caller imports one namespace's record in place of another's.
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for record in &imported {
        namespaces.entry((record.name.as_str(), record.home.as_str())).or_default().insert(record.namespace.as_str());
    }
    let clashes: BTreeMap<(String, String), String> = namespaces
        .into_iter()
        .filter(|(_, held)| held.len() > 1)
        .map(|((name, home), held)| ((name.to_owned(), home.to_owned()), held.into_iter().collect::<Vec<_>>().join("`, `")))
        .collect();
    imported.retain(|record| match clashes.get(&(record.name.clone(), record.home.clone())) {
        Some(held) => {
            report.refused.push(ImportRefusal {
                kind,
                record: record.name.clone(),
                reason: format!("at `{}`: held in namespaces `{held}`, and the engine has no namespaces", record.home),
            });
            false
        }
        None => true,
    });
    imported
}

/// Restores the records `copies` import to, and returns the `(namespace, name)` of each restored.
fn rehearse_kind<D: DefinitionKind>(
    copies: &[DefinitionCopy<D::Old>],
    rehearsal: &mut Rehearsal,
    report: &mut ImportReport,
) -> BTreeSet<(String, String)>
where
    ResourceObject<D::Old>: Clone,
{
    let kind = <D::Stored as Kind>::KIND;
    let mut restored = BTreeSet::new();
    for imported in import_kind::<D>(copies, report) {
        // Version 1: each record starts at its home as a fresh write, its old versions not carried.
        match rehearsal.restore(&imported.home, &imported.name, 1, &imported.record) {
            Ok(()) => {
                *report.imported.entry((kind, label(&imported.origin))).or_default() += 1;
                restored.insert((imported.namespace, imported.name));
            }
            Err(reason) => report.refused.push(ImportRefusal { kind, record: imported.name, reason }),
        }
    }
    restored
}

/// An origin's label, as the report counts it.
pub fn label(origin: &DefinitionOrigin) -> &'static str {
    match origin {
        DefinitionOrigin::Operator => "operator",
        DefinitionOrigin::Manifest { .. } => "manifest",
        DefinitionOrigin::ProjectOps { .. } => "project-ops",
        DefinitionOrigin::ProjectImage { .. } => "project-image",
        DefinitionOrigin::Snapshot => "snapshot",
        DefinitionOrigin::ProjectDeclaration { .. } => "project-declaration",
        DefinitionOrigin::WholeRepository => "whole-repository",
    }
}

/// The origin a copy's writer stamped on it: [`flotilla_resources::definitions::origin`], which
/// the write path reads too.
pub fn origin(meta: &ObjectMeta) -> Result<DefinitionOrigin, String> {
    flotilla_resources::definitions::origin(&meta.labels, &meta.annotations)
}
