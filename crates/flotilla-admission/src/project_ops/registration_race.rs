// Engine boundary: interleave the startup Repository controller with registration's Repository
// spec edit, at the Repository port the engine serves the kind from.
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use flotilla_resources::{
    discovered_checkouts::DiscoveredCheckouts,
    engine_records::{KindCode, KindRecords, LogPosition, Record, Since},
    Repository, RepositorySpec, RepositoryStatus,
};
use flotilla_store::{apply_status_patch, controller::Reconciler, WeakResourceBackend};
use futures::stream::BoxStream;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Contention {
    /// The controller's status write lands once, between registration's version check and its
    /// spec write.
    Once,
    /// A writer moves the record on before every version check registration makes.
    Persistent,
}

/// The engine's Repository port, with the startup Repository controller racing registration.
#[derive(Debug)]
pub(crate) struct RacingRepositories {
    pub inner: Arc<dyn KindRecords<Repository>>,
    pub backend: WeakResourceBackend,
    pub namespace: String,
    pub armed: Arc<AtomicBool>,
    pub discovered: DiscoveredCheckouts,
    pub contention: Contention,
    /// Set while the controller's own pass reads and writes, which never races itself.
    pub racing: AtomicBool,
}

impl flotilla_store::EnginePort for RacingRepositories {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<Repository>(self);
    }
}

impl RacingRepositories {
    /// One pass of the startup Repository controller over `name`, written through the backend.
    async fn controller_pass(&self, name: &str) -> Result<(), String> {
        self.racing.store(true, Ordering::SeqCst);
        let pass = async {
            let backend = self.backend.upgrade().ok_or("the backend is gone")?;
            let repositories = backend.using::<Repository>(&self.namespace);
            let repository = repositories.get(name).await.map_err(|error| error.to_string())?;
            let reconciler = flotilla_provisioning::reconcilers::RepositoryReconciler::new(
                self.discovered.clone(),
                Arc::clone(crate::test_support::no_held_checkouts()),
            );
            let prepared = reconciler.prepare(&repository).await.map_err(|error| error.to_string())?;
            let outcome = reconciler.reconcile(&repository, &prepared, chrono::Utc::now());
            let patch = outcome.patch.expect("repository status patch");
            apply_status_patch(&repositories, name, &patch).await.map(drop).map_err(|error| error.to_string())
        }
        .await;
        self.racing.store(false, Ordering::SeqCst);
        pass
    }

    /// Persistent contention: a storage writer moves the record on before each read of it, so
    /// every version registration checks is stale.
    async fn race(&self, name: &str) -> Result<(), String> {
        if matches!(self.contention, Contention::Persistent) && !self.racing.load(Ordering::SeqCst) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.controller_pass(name).await?;
            } else if let Some(held) = self.inner.copies(name).await?.into_iter().find(|record| record.home == self.inner.home()) {
                // Only a spec edit moves an engine record's version: the writer counts its
                // writes in a verification command registration leaves alone.
                let mut commands = held.spec.verification_commands().clone();
                let writes = commands.get("storage-writes").and_then(|count| count.parse::<u64>().ok()).unwrap_or(0);
                commands.insert("storage-writes".to_owned(), (writes + 1).to_string());
                self.inner.apply(name, &held.labels, &held.annotations, &held.spec.clone().with_verification_commands(commands)).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl KindRecords<Repository> for RacingRepositories {
    fn codes(&self) -> Vec<KindCode> {
        self.inner.codes()
    }
    fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
        self.inner.ticks()
    }
    fn home(&self) -> String {
        self.inner.home()
    }
    async fn get(&self, name: &str) -> Result<Option<Record<Repository>>, String> {
        self.race(name).await?;
        self.inner.get(name).await
    }
    async fn list(&self) -> Result<Vec<Record<Repository>>, String> {
        self.inner.list().await
    }
    async fn sources(&self) -> Result<Vec<Record<Repository>>, String> {
        self.inner.sources().await
    }
    async fn positioned(&self) -> Result<(Vec<Record<Repository>>, LogPosition), String> {
        self.inner.positioned().await
    }
    async fn position(&self) -> Result<LogPosition, String> {
        self.inner.position().await
    }
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        self.inner.changed_since(position).await
    }
    async fn copies(&self, name: &str) -> Result<Vec<Record<Repository>>, String> {
        self.race(name).await?;
        self.inner.copies(name).await
    }
    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &RepositorySpec,
    ) -> Result<Record<Repository>, String> {
        if matches!(self.contention, Contention::Once) && !self.racing.load(Ordering::SeqCst) && self.armed.swap(false, Ordering::SeqCst) {
            self.controller_pass(name).await?;
        }
        self.inner.apply(name, labels, annotations, spec).await
    }
    async fn set_status(
        &self,
        name: &str,
        version: u64,
        status: &RepositoryStatus,
    ) -> Result<Record<Repository>, flotilla_resources::engine_records::StatusWriteError> {
        self.inner.set_status(name, version, status).await
    }
    async fn delete(&self, name: &str) -> Result<(), String> {
        self.inner.delete(name).await
    }
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.inner.changes().await
    }
}
