//! The records the Message path reads about the convoys, crews and declarations its messages name,
//! each read by its key (#3108): a convoy's copies by name, a session, artifact, repository or
//! vessel by name, a declaration by name, a seat's sessions through its convoy's vessel, and the
//! newest launches of a standing role at each home. No read here lists a kind whole, save the
//! Forge catalog and this home's session launches, which have no key to read by.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};

use flotilla_resources::{
    lifecycle_reads::LifecycleReads, terminal_records::TerminalRecord, vessel_records::VesselRecord, Convoy, ForgeSpec, Resource,
    ResourceError, ResourceObject, ResourceProvenance, TerminalSession, CONVOY_LABEL, ROLE_LABEL,
};
use flotilla_schema::reads::carry;
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{ResourceBackend, WeakResourceBackend};

/// A convoy as this home holds it: this home's copy, else the first another home's replica lists.
#[derive(Debug, Clone)]
pub struct HeldConvoy {
    pub object: ResourceObject<Convoy>,
    /// The home whose copy `object` is.
    pub home: String,
    /// Whether any copy has not ended.
    pub live: bool,
}

/// Keyed reads of the records messages name, at one home.
pub struct KeyedReads {
    home: String,
    service: Arc<Service>,
    /// Weak: the backend holds the Message path, which holds these reads.
    backend: WeakResourceBackend,
    /// The engine's convoy, vessel and terminal session records; the composition root installs them.
    lifecycle: OnceLock<Arc<dyn LifecycleReads>>,
}

impl KeyedReads {
    pub fn new(home: &str, service: Arc<Service>, backend: WeakResourceBackend) -> Self {
        Self { home: home.to_owned(), service, backend, lifecycle: OnceLock::new() }
    }

    /// Installs the engine's lifecycle reads; the composition root owns them.
    pub fn install_lifecycle(&self, reads: Arc<dyn LifecycleReads>) -> Result<(), String> {
        self.lifecycle.set(reads).map_err(|_| "lifecycle reads are already installed".to_owned())
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub(crate) fn backend(&self) -> Result<ResourceBackend, String> {
        crate::ports::upgrade(&self.backend)
    }

    fn lifecycle(&self) -> Result<Arc<dyn LifecycleReads>, String> {
        self.lifecycle.get().cloned().ok_or_else(|| "the engine's lifecycle reads are not installed".to_owned())
    }

    /// `read` over the lifecycle reads, off the async workers: each is a store read.
    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let reads = self.lifecycle()?;
        tokio::task::spawn_blocking(carry(move || read(reads.as_ref()))).await.map_err(|error| error.to_string())?
    }

    /// Every home this store holds records of, this one among them.
    fn homes(&self) -> BTreeSet<String> {
        self.service.homes().into_iter().chain([self.home.clone()]).collect()
    }

    /// Convoy `name`: this home's copy, else the first listed, and whether any copy is live.
    pub async fn convoy(&self, namespace: &str, name: &str) -> Result<Option<HeldConvoy>, ResourceError> {
        let copies = self.convoy_copies(namespace, name).await?;
        let live = copies.iter().any(|copy| !copy.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()));
        let held =
            copies.iter().position(|copy| matches!(copy.provenance, ResourceProvenance::Local)).or((!copies.is_empty()).then_some(0));
        Ok(held.map(|index| {
            let copy = copies[index].clone();
            let home = match &copy.provenance {
                ResourceProvenance::Local => self.home.clone(),
                ResourceProvenance::Replica { origin_root, .. } => origin_root.to_string(),
            };
            HeldConvoy { object: copy.object, home, live }
        }))
    }

    /// Every home's copy of convoy `name`, as replica reads.
    pub async fn convoy_copies(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<flotilla_resources::ReadResourceObject<Convoy>>, ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        Ok(found(backend.including_replicas::<Convoy>(namespace).get_all(name).await)?.map(|copies| copies.items).unwrap_or_default())
    }

    /// Record `name` of a replicated kind, this home's copy first.
    pub async fn record<K: Resource>(&self, namespace: &str, name: &str) -> Result<Option<ResourceObject<K>>, ResourceError> {
        Ok(self.read::<K>(namespace, name).await?.map(|read| read.object))
    }

    /// Record `name` of a replicated kind with where it is read from.
    pub async fn read<K: Resource>(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<K>>, ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        found(backend.including_replicas::<K>(namespace).get(name).await)
    }

    /// Declaration `name` of a definition kind (a project, a standing role's ensure).
    pub async fn declared<K: Resource>(&self, namespace: &str, name: &str) -> Result<Option<ResourceObject<K>>, ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        found(backend.definitions::<K>(namespace).get(name).await)
    }

    /// The Forge catalog: a repository's address is read from the forge its remote's host names,
    /// and no key reaches that forge.
    pub async fn forges(&self, namespace: &str) -> Result<Vec<ForgeSpec>, ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        Ok(backend.definitions::<flotilla_resources::Forge>(namespace).list().await?.into_iter().map(|forge| forge.spec).collect())
    }

    /// The sessions labelled as holding `role` in `convoy`, at every home.
    pub async fn labelled_sessions(
        &self,
        namespace: &str,
        convoy: &str,
        role: &str,
    ) -> Result<Vec<ResourceObject<TerminalSession>>, ResourceError> {
        let backend = self.backend().map_err(ResourceError::other)?;
        let labels = BTreeMap::from([(CONVOY_LABEL.to_owned(), convoy.to_owned()), (ROLE_LABEL.to_owned(), role.to_owned())]);
        Ok(backend
            .including_replicas::<TerminalSession>(namespace)
            .list_matching_labels(&labels)
            .await?
            .items
            .into_iter()
            .map(|read| read.object)
            .collect())
    }

    /// The sessions of the vessel launched for the work holding `role` in `convoy`, through the
    /// convoy's snapshot; see [`Self::work_sessions`]. `None` when the lifecycle reads are not
    /// installed or the snapshot names no work for the role.
    pub async fn seat_sessions(&self, convoy: &ResourceObject<Convoy>, convoy_home: &str, role: &str) -> Option<Vec<String>> {
        let work = snapshot_work(convoy, role)?;
        self.work_sessions(&convoy.metadata.name, convoy_home, &work, role).await.ok()
    }

    /// The sessions holding `role` in work `work` of `convoy`: those of the work's newest vessel
    /// launch at each home this store holds, since the vessel is homed where it was placed, one
    /// index read per home.
    pub async fn work_sessions(&self, convoy: &str, convoy_home: &str, work: &str, role: &str) -> Result<Vec<String>, String> {
        let (convoy, convoy_home, work, role) = (convoy.to_owned(), convoy_home.to_owned(), work.to_owned(), role.to_owned());
        let mut homes = self.homes();
        homes.insert(convoy_home.clone());
        self.blocking(move |reads| {
            let (vessels, terminals) = (reads.vessels()?, reads.terminals()?);
            let launch_of = flotilla_resources::vessel_resource_name(flotilla_resources::convoy::launch_of(&convoy), &work);
            let mut sessions = Vec::new();
            for home in &homes {
                let Some(vessel) = vessels.newest(&convoy, &convoy_home, &launch_of, Some(home))? else { continue };
                sessions.extend(
                    terminals
                        .of_vessel(&vessel.name)?
                        .into_iter()
                        .filter(|launch| launch.role == role)
                        .map(|launch| launch.session_name().to_owned()),
                );
            }
            Ok(sessions)
        })
        .await
        .inspect_err(|error| tracing::debug!(%error, "a convoy role's sessions could not be read"))
    }

    /// The newest vessel launch of work `work` of `convoy`: this home's, else the latest requested
    /// at another home.
    pub async fn vessel(&self, convoy: &str, convoy_home: &str, work: &str) -> Result<Option<VesselRecord>, String> {
        let (convoy, convoy_home, work, here) = (convoy.to_owned(), convoy_home.to_owned(), work.to_owned(), self.home.clone());
        let mut homes = self.homes();
        homes.insert(convoy_home.clone());
        self.blocking(move |reads| {
            let vessels = reads.vessels()?;
            let launch_of = flotilla_resources::vessel_resource_name(flotilla_resources::convoy::launch_of(&convoy), &work);
            let mut newest: Option<VesselRecord> = None;
            for home in &homes {
                let Some(vessel) = vessels.newest(&convoy, &convoy_home, &launch_of, Some(home))? else { continue };
                let replaces = newest
                    .as_ref()
                    .is_none_or(|held| held.home != here && (vessel.home == here || vessel.requested_at > held.requested_at));
                if replaces {
                    newest = Some(vessel);
                }
            }
            Ok(newest)
        })
        .await
    }

    /// The newest launch of `of` in `project` at each home this store holds, by name.
    pub async fn newest_launches(&self, of: &str, project: &str) -> Result<Vec<String>, String> {
        let (of, project) = (of.to_owned(), project.to_owned());
        let homes = self.homes();
        self.blocking(move |reads| {
            let convoys = reads.convoys()?;
            let mut launches = Vec::new();
            for home in &homes {
                launches.extend(convoys.newest(&of, &project, Some(home))?.map(|launch| launch.name));
            }
            Ok(launches)
        })
        .await
    }

    /// This home's running agent session launches: no key reaches them, so every launch held
    /// here is read.
    pub async fn running_here(&self) -> Result<Vec<TerminalRecord>, String> {
        let here = self.home.clone();
        self.blocking(move |reads| {
            Ok(reads.terminals()?.at(&here)?.into_iter().filter(|launch| launch.is_agent() && launch.is_running()).collect())
        })
        .await
    }
}

fn found<T>(read: Result<T, ResourceError>) -> Result<Option<T>, ResourceError> {
    match read {
        Ok(read) => Ok(Some(read)),
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The work holding `role` in `convoy`'s workflow snapshot.
pub fn snapshot_work(convoy: &ResourceObject<Convoy>, role: &str) -> Option<String> {
    let status = convoy.status.as_ref()?;
    status
        .workflow_snapshot
        .as_ref()?
        .vessels
        .iter()
        .find(|vessel| vessel.crew.iter().any(|crew| crew.role == role))
        .map(|vessel| vessel.name.clone())
}
