//! The person roles' inboxes under each declared project (#3108), held per namespace and followed
//! from the Project records' changes: a read of them lists no Project, and a change re-reads only
//! the Projects it names, each by its name.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_resources::{Project, ProjectSpec, ReadWatchEvent, ResourceError};
use flotilla_store::{ResourceBackend, StoreViewSource, ViewSource};
use futures::{stream::BoxStream, FutureExt, StreamExt};

use crate::roles;

type Changes = BoxStream<'static, Result<ReadWatchEvent<Project>, ResourceError>>;

/// The person roles' inboxes under each namespace's declared projects.
#[derive(Default)]
pub(crate) struct PrincipalRoles {
    held: tokio::sync::Mutex<BTreeMap<String, Followed>>,
}

/// One namespace's projects' person inboxes, and the watch of their changes.
struct Followed {
    projects: BTreeMap<String, BTreeSet<Address>>,
    changes: Changes,
}

impl PrincipalRoles {
    /// The person roles' inboxes under every project declared in `namespace`, and under the
    /// namespace itself, as of every Project change committed before the call.
    pub(crate) async fn inboxes(&self, backend: &ResourceBackend, namespace: &str) -> Result<BTreeSet<Address>, ResourceError> {
        let mut held = self.held.lock().await;
        if let Some(followed) = held.get_mut(namespace) {
            StoreViewSource::<Project>::new(backend.clone(), namespace).catch_up().await;
            match followed.follow(backend, namespace).await {
                Ok(()) => return Ok(followed.inboxes(namespace)),
                // A watch that failed may have missed changes: list again.
                Err(error) => tracing::debug!(%namespace, %error, "the Project watch failed; the projects are listed again"),
            }
        }
        let followed = Followed::start(backend, namespace).await?;
        let inboxes = followed.inboxes(namespace);
        held.insert(namespace.to_owned(), followed);
        Ok(inboxes)
    }
}

impl Followed {
    /// Watches the Projects, then lists them: a change between the two is in the listing, the
    /// watch or both.
    async fn start(backend: &ResourceBackend, namespace: &str) -> Result<Self, ResourceError> {
        let changes = backend.including_replicas::<Project>(namespace).watch().await?;
        let projects = backend
            .definitions::<Project>(namespace)
            .list()
            .await?
            .into_iter()
            .map(|project| {
                let inboxes = project_inboxes(&project.metadata.name, Some(&project.spec));
                (project.metadata.name, inboxes)
            })
            .collect();
        Ok(Self { projects, changes })
    }

    /// Takes every change the watch holds, then reads each Project it names by its name.
    async fn follow(&mut self, backend: &ResourceBackend, namespace: &str) -> Result<(), ResourceError> {
        let mut changed = BTreeSet::new();
        loop {
            match self.changes.next().now_or_never() {
                None => break,
                Some(Some(event)) => {
                    changed.insert(changed_name(&event?));
                }
                Some(None) => return Err(ResourceError::other("the Project watch ended")),
            }
        }
        let definitions = backend.definitions::<Project>(namespace);
        for name in changed {
            match definitions.get(&name).await {
                Ok(project) => {
                    self.projects.insert(name.clone(), project_inboxes(&name, Some(&project.spec)));
                }
                Err(ResourceError::NotFound { .. }) => {
                    self.projects.remove(&name);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Every project's person inboxes; a namespace no Project declares counts as one, as a
    /// convoy with no project is addressed under its namespace.
    fn inboxes(&self, namespace: &str) -> BTreeSet<Address> {
        let mut inboxes: BTreeSet<Address> = self.projects.values().flatten().cloned().collect();
        if !self.projects.contains_key(namespace) {
            inboxes.extend(project_inboxes(namespace, None));
        }
        inboxes
    }
}

fn changed_name(event: &ReadWatchEvent<Project>) -> String {
    match event {
        ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => item.object.metadata.name.clone(),
        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name.clone(),
    }
}

/// The inboxes of each person role under `project`, and of each role its own charter declares a
/// principal holds. Declarations stay local: an inherited role shape names no holder in a child.
fn project_inboxes(project: &str, spec: Option<&ProjectSpec>) -> BTreeSet<Address> {
    let Ok(address) = Address::project(project) else {
        tracing::debug!(%project, "a project with no address has no person inboxes");
        return BTreeSet::new();
    };
    let declared = spec.into_iter().flat_map(|spec| {
        spec.role_definitions.iter().filter(|(_, definition)| definition.principal.is_some()).map(|(role, _)| role.as_str())
    });
    roles::PERSON_ROLES
        .iter()
        .map(|role| role.name)
        .chain(declared)
        .filter_map(|role| match address.child(AddressKind::Role, role) {
            Ok(inbox) => Some(inbox),
            Err(error) => {
                tracing::debug!(%project, %role, %error, "a person role with no address");
                None
            }
        })
        .collect()
}
