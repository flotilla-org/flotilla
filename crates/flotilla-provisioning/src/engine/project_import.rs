//! Project's import onto the engine (#3824 layer 3b, ADR 0047).
//!
//! Project is a definition kind, so its spec imports as the other definition kinds' do
//! ([`super::definition_import`], whose rehearsal holds Project's kinds too): the merged spec, at each declaring home or else at the home that created
//! it, with a typed origin from that home's copy. Project adds two origins, a `project.yaml`
//! declaration and a whole-repository Project, read from the stamps project operations wrote.
//!
//! | Old copy | Imported |
//! |---|---|
//! | `spec` | the record's frozen definition, with its parent, charter and repositories as columns |
//! | A spec naming one repository twice | refused |
//! | `project-bootstrap-repository`, `-commit`, `project-declaration-file` | origin `ProjectDeclaration` |
//! | `managed-by=whole-repository-project` | origin `WholeRepository` |
//! | Retired `project-bootstrap-path` annotation, other labels and annotations | dropped |
//! | `status.declaration_refused`, `operational_entries` | a ProjectOps record at the copy's home |
//! | `status.dispatch_queue`, its error and attention | dropped: the dispatch reconciler recomputes it |
//!
//! A ProjectOps record imports only beside a Project of its name that imports, and, like every
//! definition, a name a home holds in several namespaces is refused (#3212: no namespaces).

use std::collections::BTreeSet;

use flotilla_engine_store::{ImportTarget, IMPORT_WRITER};
use flotilla_resources::{Project, ProjectSpec};
use flotilla_schema::{Kind, NewChange, Timestamp};

use super::{
    definition_import::{DefinitionCopy, DefinitionKind, Mapped},
    import::{ImportRefusal, ImportReport},
};
use crate::stored::project::{ProjectDefinition, ProjectOpsRecord};

impl DefinitionKind for ProjectDefinition {
    type Old = Project;
    type Stored = ProjectDefinition;
    fn map(_: Option<String>, spec: &ProjectSpec) -> Result<Mapped<ProjectDefinition>, String> {
        ProjectDefinition::new(spec).map(Mapped::Stored)
    }
}

/// One ProjectOps record the import makes: the project's name, the home and the record.
pub struct ImportedOps {
    pub name: String,
    pub home: String,
    pub record: ProjectOpsRecord,
    /// When its copy was created.
    pub created: Timestamp,
}

/// The ProjectOps records the copies' statuses import to, beside the Projects `imported` names by
/// `(namespace, name)`. Each status holding a dispatch queue counts one drop.
pub fn import_ops<'a>(
    copies: impl IntoIterator<Item = &'a DefinitionCopy<Project>>,
    imported: &BTreeSet<(String, String)>,
    report: &mut ImportReport,
) -> Vec<ImportedOps> {
    let kind = <ProjectOpsRecord as Kind>::KIND;
    let mut records = Vec::new();
    for copy in copies {
        let Some(status) = copy.object.status.as_ref() else { continue };
        if !status.dispatch_queue.is_empty() || status.dispatch_queue_error.is_some() || status.dispatch_queue_attention.is_some() {
            *report.dropped.entry((kind, "dispatch queue, recomputed")).or_default() += 1;
        }
        let Some(record) = ProjectOpsRecord::from_status(status) else { continue };
        let name = &copy.object.metadata.name;
        if copy.object.metadata.deletion_timestamp.is_some() || !imported.contains(&(copy.namespace.clone(), name.clone())) {
            *report.dropped.entry((kind, "its Project does not import")).or_default() += 1;
            continue;
        }
        let created = Timestamp::from_datetime(&copy.object.metadata.creation_timestamp);
        records.push(ImportedOps { name: name.clone(), home: copy.home.clone(), record, created });
    }
    records
}

/// Writes the ProjectOps records of the `copies` at homes `target` imports, beside the Projects
/// already written, `projects`, each at its copy's creation time.
pub(super) fn write_project_ops(
    copies: &[DefinitionCopy<Project>],
    projects: &BTreeSet<(String, String)>,
    target: &mut impl ImportTarget,
    report: &mut ImportReport,
) {
    let kind = <ProjectOpsRecord as Kind>::KIND;
    let ours: Vec<_> = copies.iter().filter(|copy| target.imports(&copy.home)).collect();
    for ops in import_ops(ours, projects, report) {
        let change = NewChange::new(IMPORT_WRITER, ops.created);
        match target.write(&ops.home, &ops.name, &change, &ops.record) {
            Ok(()) => *report.imported.entry((kind, "observed")).or_default() += 1,
            Err(reason) => report.refused.push(ImportRefusal { kind, record: ops.name, reason: format!("at `{}`: {reason}", ops.home) }),
        }
    }
}
