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

use std::collections::BTreeMap;

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

/// The prefix of an inline charter's source, as charter delegation writes it.
const INLINE_CHARTER: &str = "inline charter ";

/// The repository a charter source names, by its key: `{repo}@{branch}:{path}`, as charter
/// delegation writes a repository pointer's. A branch holds no `:` (git refuses one), so the path
/// follows the last `:`. Only a remote URL names a key here (an ssh remote's needs its host
/// resolved, which a store read cannot do), and a remote URL's path holds no `@`, so the repository
/// ends at the first `@` after its authority: a branch may hold `@`, a repository path may not. An
/// inline charter's source names no repository; another source that names none is warned of, since
/// its change then holds the root's revision alone.
fn charter_repository(source: &str) -> Option<RepositoryKey> {
    if source.starts_with(INLINE_CHARTER) {
        return None;
    }
    let repository = source
        .rsplit_once(':')
        .and_then(|(pointer, _)| {
            // The path starts at the first `/` after the scheme's `://`; the authority before it
            // may hold a user's `@`.
            let authority = pointer.find("://").map_or(0, |scheme| scheme + 3);
            let path = authority + pointer[authority..].find('/')?;
            let separator = path + pointer[path..].find('@')?;
            Some(&pointer[..separator])
        })
        .and_then(|repo| RepositorySpec::remote(repo).ok())
        .map(|spec| spec.key());
    if repository.is_none() {
        tracing::warn!(%source, "a charter source names no repository; its change holds the root's revision alone");
    }
    repository
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

/// The roots a pass over records has read: each one's source and its documents' paths by kind and
/// name, so a rescan reads each root once, not once per record it declares.
#[derive(Default)]
pub(crate) struct Roots(BTreeMap<Identity, Option<Root>>);

/// A root's source, and the path of each document it declares, by kind and name.
struct Root {
    source: String,
    paths: BTreeMap<(String, String), String>,
}

impl Roots {
    fn get(&mut self, read: &mut Read<'_>, root: &Identity) -> Result<Option<&Root>, StoreError> {
        if !self.0.contains_key(root) {
            let held = load_root(read, root)?;
            self.0.insert(root.clone(), held);
        }
        Ok(self.0.get(root).and_then(Option::as_ref))
    }
}

/// The origin of record `name` of resource kind `kind`, rebuilt from its last change, with `project`
/// its own `project` column (a workflow template's). A record no change explains is an operator's.
pub(crate) fn origin_of(
    read: &mut Read<'_>,
    roots: &mut Roots,
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
            let (source, path) = match roots.get(read, &root.record)? {
                Some(held) => (Some(held.source.clone()), held.paths.get(&(kind.to_owned(), name.to_owned())).cloned()),
                None => (None, None),
            };
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

/// The source of `root`, and the path of each document it declares: the root's baselines list
/// every one. None while this store does not hold the root.
fn load_root(read: &mut Read<'_>, root: &Identity) -> Result<Option<Root>, StoreError> {
    let Some(id) = read.resolve(root.kind, &root.name, &root.home)? else { return Ok(None) };
    let Some(loaded) = read.load::<Resource<ManifestRoot>>(id)? else { return Ok(None) };
    let spec = loaded.value.spec();
    let paths = spec
        .baselines
        .keys()
        .filter_map(|document| serde_json::from_value::<DocumentKey>(serde_json::Value::String(document.0.clone())).ok())
        .map(|key| ((key.kind, key.name), key.path))
        .collect();
    Ok(Some(Root { source: spec.source.clone(), paths }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#4024): a charter's repository is named by its key when the charter is a
    // repository's, and an inline charter names none.
    #[test]
    fn a_charter_names_its_repository_by_key() {
        let key = |repo: &str| RepositorySpec::remote(repo).expect("spec").key();
        for (source, repo) in [
            ("https://example.test/app-ops@reviewed:charter", "https://example.test/app-ops"),
            ("https://example.test/app-ops@main:", "https://example.test/app-ops"),
            ("https://example.test/app-ops@release/2026@q4:charter", "https://example.test/app-ops"),
            ("https://bot@example.test/app-ops@main:charter", "https://bot@example.test/app-ops"),
        ] {
            assert_eq!(charter_repository(source), Some(key(repo)), "{source}");
        }
        assert_eq!(charter_repository("inline charter flotilla/p in ops.yaml"), None);
        assert_eq!(charter_repository("not a pointer"), None);
        // An ssh remote's key needs its host resolved, which a store read cannot do: no item.
        assert_eq!(charter_repository("git@github.com:acme/app-ops.git@main:charter/ops"), None);
        assert_eq!(scoped_project("flotilla/p"), "p");
    }
}
