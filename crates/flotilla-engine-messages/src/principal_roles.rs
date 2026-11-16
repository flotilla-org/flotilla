//! The person roles' inboxes under each declared project (#3108), held per namespace and followed
//! from the Project records' changes: a read of them lists no Project, and a change re-reads only
//! the Projects it names, each by its name.
//!
//! Each namespace's watch is held by a task on the runtime that first read it, not by the Message
//! path: the watch holds the backend, which holds the path, so held there it would hold itself.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_address::{Address, Kind as AddressKind};
use flotilla_resources::{Project, ProjectSpec, ReadWatchEvent, ResourceError};
use flotilla_store::{ResourceBackend, StoreViewSource, ViewSource, WeakResourceBackend};
use futures::{stream::BoxStream, FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot};

use crate::roles;

type Changes = BoxStream<'static, Result<ReadWatchEvent<Project>, ResourceError>>;
type Answer = oneshot::Sender<Result<BTreeSet<Address>, ResourceError>>;

/// The person roles' inboxes under each namespace's declared projects.
#[derive(Default)]
pub(crate) struct PrincipalRoles {
    followers: std::sync::Mutex<BTreeMap<String, mpsc::UnboundedSender<Answer>>>,
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
        StoreViewSource::<Project>::new(backend.clone(), namespace).catch_up().await;
        // A follower whose runtime has ended is started again on this one.
        for _ in 0..2 {
            let follower = {
                let mut followers = self.followers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                followers
                    .entry(namespace.to_owned())
                    .or_insert_with(|| {
                        let (requests, received) = mpsc::unbounded_channel();
                        tokio::spawn(follow(backend.downgrade(), namespace.to_owned(), received));
                        requests
                    })
                    .clone()
            };
            let (answer, answered) = oneshot::channel();
            if follower.send(answer).is_ok() {
                if let Ok(inboxes) = answered.await {
                    return inboxes;
                }
            }
            self.followers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(namespace);
        }
        Err(ResourceError::other(format!("the Project follower of namespace {namespace} stopped")))
    }
}

/// Answers each request with `namespace`'s person inboxes, following the Projects between them.
async fn follow(backend: WeakResourceBackend, namespace: String, mut requests: mpsc::UnboundedReceiver<Answer>) {
    let mut followed = None;
    while let Some(answer) = requests.recv().await {
        let _ = answer.send(answer_from(&mut followed, &backend, &namespace).await);
    }
}

async fn answer_from(
    followed: &mut Option<Followed>,
    backend: &WeakResourceBackend,
    namespace: &str,
) -> Result<BTreeSet<Address>, ResourceError> {
    let backend = crate::ports::upgrade(backend).map_err(ResourceError::other)?;
    if let Some(held) = followed.as_mut() {
        match held.follow(&backend, namespace).await {
            Ok(()) => return Ok(held.inboxes(namespace)),
            // A watch that failed may have missed changes: list again.
            Err(error) => tracing::debug!(%namespace, %error, "the Project watch failed; the projects are listed again"),
        }
        *followed = None;
    }
    let held = followed.insert(Followed::start(&backend, namespace).await?);
    Ok(held.inboxes(namespace))
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
