//! Why a definition was written, as its change says it (#4024).
//!
//! A definition row holds only its own fields. Its writer's stamps become its change: the writer,
//! by what declared the definition, and the change's items, the records it was made from, each at
//! a revision when it has one. [`change_of`] maps an origin to that change; [`origin_of`] rebuilds
//! the origin from a record's last change, with what the change does not hold read from the
//! records it names: a manifest's source from its root, a document's path from the root's
//! baselines, and a template's project from its own `project` column.
//!
//! | Origin | Writer | Items |
//! |---|---|---|
//! | manifest document | `manifest-reconciler` | the ManifestRoot at the revision applied |
//! | charter document | `manifest-reconciler` | the above; the Project delegated to; the charter Repository at its commit when the charter is a repository's |
//! | project operations entry | `project-operations` | the Repository at the commit; the Project |
//! | project image layer | `project-image` | the Project at the commit |
//! | project declaration | `project-declaration` | the Repository at the commit |
//! | whole-repository Project | `whole-repository-project` | none |
//! | admission snapshot | `admission` | none |
//! | operator apply | `operator` | none |

use flotilla_protocol::RepositoryKey;
use flotilla_resources::{
    definitions::{CharterOrigin, DefinitionOrigin},
    DocumentKey, RepositorySpec,
};
use flotilla_schema::{ChangeHeader, ChangeItem, Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{Read, StoreError};
use flotilla_state_machines::Resource;

use crate::stored::{manifest_root::ManifestRoot, project::ProjectDefinition, repository::RepositoryRecord};

pub(crate) const MANIFEST: &str = "manifest-reconciler";
pub(crate) const PROJECT_OPERATIONS: &str = "project-operations";
pub(crate) const PROJECT_IMAGE: &str = "project-image";
pub(crate) const PROJECT_DECLARATION: &str = "project-declaration";
pub(crate) const WHOLE_REPOSITORY: &str = "whole-repository-project";
pub(crate) const ADMISSION: &str = "admission";
pub(crate) const OPERATOR: &str = "operator";

/// The namespace the decided store's records are served in; a charter's scope names it.
const NAMESPACE: &str = "flotilla";

const ROOT: KindCode = <Resource<ManifestRoot> as Kind>::CODE;
const REPOSITORY: KindCode = <RepositoryRecord as Kind>::CODE;
const PROJECT: KindCode = <ProjectDefinition as Kind>::CODE;

fn item(kind: KindCode, name: &str, home: &str, revision: Option<&str>) -> ChangeItem {
    ChangeItem { record: Identity { kind, name: name.to_owned(), home: home.to_owned() }, revision: revision.map(str::to_owned) }
}

/// The repository a charter source names, by its key: `{repo}@{branch}:{path}`, as charter
/// delegation writes a repository pointer's. An inline charter's source names no repository.
fn charter_repository(source: &str) -> Option<RepositoryKey> {
    let (repo, _) = source.rsplit_once('@')?;
    RepositorySpec::remote(repo).ok().map(|spec| spec.key())
}

/// The project a `namespace/project` scope names.
fn scoped_project(scope: &str) -> &str {
    scope.split_once('/').map_or(scope, |(_, project)| project)
}

/// The change a writer of `origin` at `home` makes, at `at`. The records its items name are this
/// home's: a definition is homed at the root that declares it, and each home holds its own record
/// of each repository it observes.
pub(crate) fn change_of(origin: &DefinitionOrigin, home: &str, at: Timestamp) -> NewChange {
    let (writer, items) = match origin {
        DefinitionOrigin::Operator => (OPERATOR, Vec::new()),
        DefinitionOrigin::Manifest { root, revision, charter, .. } => {
            let mut items = vec![item(ROOT, root, home, Some(revision))];
            if let Some(charter) = charter {
                items.push(item(PROJECT, scoped_project(&charter.scope), home, None));
                if let Some(repository) = charter.source.as_deref().and_then(charter_repository) {
                    items.push(item(REPOSITORY, &repository.0, home, Some(&charter.revision)));
                }
            }
            (MANIFEST, items)
        }
        DefinitionOrigin::ProjectOps { project, repository, commit, .. } => {
            (PROJECT_OPERATIONS, vec![item(REPOSITORY, repository, home, Some(commit)), item(PROJECT, project, home, None)])
        }
        DefinitionOrigin::ProjectImage { project, commit } => (PROJECT_IMAGE, vec![item(PROJECT, project, home, Some(commit))]),
        DefinitionOrigin::ProjectDeclaration { repository, commit } => {
            (PROJECT_DECLARATION, vec![item(REPOSITORY, repository, home, Some(commit))])
        }
        DefinitionOrigin::WholeRepository => (WHOLE_REPOSITORY, Vec::new()),
        DefinitionOrigin::Snapshot => (ADMISSION, Vec::new()),
    };
    NewChange::new(writer, at).with_items(items)
}

/// The project a workflow template written with `origin` is materialised for: its `project` column.
pub(crate) fn materialized_project(origin: &DefinitionOrigin) -> Option<String> {
    match origin {
        DefinitionOrigin::Manifest { materialized_project, .. } => materialized_project.clone(),
        DefinitionOrigin::ProjectOps { project, .. } => Some(project.clone()),
        _ => None,
    }
}

/// The origin of record `name` of resource kind `kind`, rebuilt from its last change, with `project`
/// its own `project` column (a workflow template's). A record no change explains is an operator's.
pub(crate) fn origin_of(
    read: &mut Read<'_>,
    kind: &str,
    name: &str,
    change: Option<&ChangeHeader>,
    project: Option<&str>,
) -> Result<DefinitionOrigin, StoreError> {
    let Some(change) = change else { return Ok(DefinitionOrigin::Operator) };
    let named = |code: KindCode| change.items.iter().find(|item| item.record.kind == code);
    let revision = |item: &ChangeItem| item.revision.clone().unwrap_or_default();
    Ok(match change.writer.as_str() {
        MANIFEST => {
            let Some(root) = named(ROOT) else { return Ok(DefinitionOrigin::Operator) };
            let (source, path) = root_document(read, &root.record, kind, name)?;
            let charter = named(PROJECT).map(|project| CharterOrigin {
                source: None,
                revision: named(REPOSITORY).map_or_else(|| revision(root), revision),
                scope: format!("{NAMESPACE}/{}", project.record.name),
            });
            DefinitionOrigin::Manifest {
                root: root.record.name.clone(),
                source,
                path,
                revision: revision(root),
                charter,
                materialized_project: project.map(str::to_owned),
            }
        }
        PROJECT_OPERATIONS => match (named(PROJECT), named(REPOSITORY)) {
            (Some(project), Some(repository)) => DefinitionOrigin::ProjectOps {
                project: project.record.name.clone(),
                repository: repository.record.name.clone(),
                commit: revision(repository),
                entry_path: None,
            },
            _ => DefinitionOrigin::Operator,
        },
        PROJECT_IMAGE => match named(PROJECT) {
            Some(project) => DefinitionOrigin::ProjectImage { project: project.record.name.clone(), commit: revision(project) },
            None => DefinitionOrigin::Operator,
        },
        PROJECT_DECLARATION => match named(REPOSITORY) {
            Some(repository) => {
                DefinitionOrigin::ProjectDeclaration { repository: repository.record.name.clone(), commit: revision(repository) }
            }
            None => DefinitionOrigin::Operator,
        },
        WHOLE_REPOSITORY => DefinitionOrigin::WholeRepository,
        ADMISSION => DefinitionOrigin::Snapshot,
        _ => DefinitionOrigin::Operator,
    })
}

/// The source of `root`, and the path of the document declaring `kind` `name` in it: the root's
/// baselines list every document it declares. Either is absent while the root does not hold it.
fn root_document(read: &mut Read<'_>, root: &Identity, kind: &str, name: &str) -> Result<(Option<String>, Option<String>), StoreError> {
    let Some(id) = read.resolve(root.kind, &root.name, &root.home)? else { return Ok((None, None)) };
    let Some(loaded) = read.load::<Resource<ManifestRoot>>(id)? else { return Ok((None, None)) };
    let spec = loaded.value.spec();
    let path = spec.baselines.keys().find_map(|document| {
        let key: DocumentKey = serde_json::from_value(serde_json::Value::String(document.0.clone())).ok()?;
        (key.kind == kind && key.name == name).then_some(key.path)
    });
    Ok((Some(spec.source.clone()), path))
}

/// The change of resource `id`'s newest entry.
pub(crate) fn last_change(read: &mut Read<'_>, id: flotilla_schema_sqlite::ResourceId) -> Result<Option<ChangeHeader>, StoreError> {
    read.last_change(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#4024): a charter's repository is named by its key when the charter is a
    // repository's, and an inline charter names none.
    #[test]
    fn a_charter_names_its_repository_by_key() {
        let key = charter_repository("https://example.test/app-ops@reviewed:charter").expect("a repository pointer");
        assert_eq!(key, RepositorySpec::remote("https://example.test/app-ops").expect("spec").key());
        assert_eq!(charter_repository("inline charter flotilla/p in ops.yaml"), None);
        assert_eq!(scoped_project("flotilla/p"), "p");
    }
}
