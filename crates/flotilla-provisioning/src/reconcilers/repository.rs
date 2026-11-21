use std::{collections::BTreeSet, sync::Arc};

use crate::engine::{checkouts::CheckoutRepositoryWake, clones::CloneRepositoryWake};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::checkout_records::latest_launches;
use flotilla_resources::{
    discovered_checkouts::DiscoveredCheckouts, lifecycle_reads::LifecycleReads, resolve_default_branch, CheckoutSpec, CredentialConsumer,
    CredentialSpec, DefaultBranchObservation, DefaultBranchProvenance, ForgeIdentity, ForgeSpec, LifecycleAuthority, ReadWatchEvent,
    Repository, RepositoryCheckoutRef, RepositoryKey, RepositoryStatus, RepositoryStatusPatch, ResourceError, ResourceObject,
};
use flotilla_resources::{forge_records::ForgeRecords, repository_records::RepositoryRecords};
use flotilla_schema::reads::carry;
use flotilla_store::{
    controller::{ReconcileOutcome, Reconciler, SecondaryWatch, WorkQueueSender},
    ResourceBackend,
};
use futures::StreamExt;
use tokio::sync::broadcast::error::RecvError;

#[async_trait]
pub trait ForgeDefaultBranchResolver: Send + Sync {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String>;
}

pub struct RepositoryReconciler {
    /// The checkouts the host's refresh discovered.
    discovered: DiscoveredCheckouts,
    /// The engine's environments, clones and checkouts, through its readings: a checkout's host is
    /// its environment's, and each clone launch found the remote's default branch.
    lifecycle_reads: Arc<dyn LifecycleReads>,
    forge_default_branch_resolver: Option<Arc<dyn ForgeDefaultBranchResolver>>,
}

impl RepositoryReconciler {
    pub fn new(discovered: DiscoveredCheckouts, lifecycle_reads: Arc<dyn LifecycleReads>) -> Self {
        Self { discovered, lifecycle_reads, forge_default_branch_resolver: None }
    }

    pub fn with_forge_default_branch_resolver(mut self, resolver: Arc<dyn ForgeDefaultBranchResolver>) -> Self {
        self.forge_default_branch_resolver = Some(resolver);
        self
    }

    /// What a repository follows: its discovered checkouts, and in the engine's `service` its
    /// checkouts and clones.
    pub fn secondary_watches(
        discovered: DiscoveredCheckouts,
        service: Arc<flotilla_schema_sqlite::stream::Service>,
        forges: Arc<dyn ForgeRecords>,
        repositories: Arc<dyn RepositoryRecords>,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Repository>>> {
        vec![
            Box::new(DiscoveredRepositoryWake(discovered)),
            Box::new(ForgeIdentityRepositoryWake { forges, repositories }),
            Box::new(CheckoutRepositoryWake::new(Arc::clone(&service))),
            Box::new(CloneRepositoryWake::new(service)),
        ]
    }

    async fn checkout_host(&self, checkout: &CheckoutSpec) -> Result<String, String> {
        match checkout {
            CheckoutSpec::Observed(spec) => Ok(spec.host_ref.clone()),
            CheckoutSpec::Worktree(spec) => self.environment_host(&spec.env_ref).await,
            CheckoutSpec::FreshClone(spec) => self.environment_host(&spec.env_ref).await,
        }
    }

    async fn environment_host(&self, environment_ref: &str) -> Result<String, String> {
        let environment = self
            .lifecycle_reads
            .environments()
            .and_then(|environments| environments.find(environment_ref))
            .and_then(|environment| environment.ok_or_else(|| "not found".to_owned()))
            .map_err(|error| format!("cannot resolve host for environment {environment_ref}: {error}"))?;
        environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.clone())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.clone()))
            .ok_or_else(|| format!("environment {environment_ref} has no host association"))
    }
}

/// Wakes a Repository's reconciler when the checkouts discovered for it change.
#[derive(Clone)]
struct DiscoveredRepositoryWake(DiscoveredCheckouts);

impl SecondaryWatch for DiscoveredRepositoryWake {
    type Primary = Repository;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        _backend: ResourceBackend,
        _namespace: String,
        sender: WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        let mut changes = self.0.changes();
        Box::pin(async move {
            loop {
                match changes.recv().await {
                    Ok(repository) => {
                        if sender.send(repository.0).await.is_err() {
                            return Ok(());
                        }
                    }
                    Err(RecvError::Lagged(_)) => sender.send_every(),
                    Err(RecvError::Closed) => return Ok(()),
                }
            }
        })
    }
}

/// Wakes the Repositories on a forge when the forge, or a Forgejo credential on it, changes: a
/// repository's forge lookup runs as the daemon's identity there, which those declare.
#[derive(Clone)]
struct ForgeIdentityRepositoryWake {
    forges: Arc<dyn ForgeRecords>,
    repositories: Arc<dyn RepositoryRecords>,
}

impl ForgeIdentityRepositoryWake {
    /// The URL of the forge `event` changes the daemon's identity on, if any.
    async fn forge_url(&self, event: ForgeIdentityEvent) -> Result<Option<String>, ResourceError> {
        match event {
            ForgeIdentityEvent::Forge(spec) => Ok(Some(spec.https_url)),
            // An undeclared forge has no repositories to wake; its declaration wakes them.
            ForgeIdentityEvent::Credential(CredentialConsumer::Forgejo { forge_ref, .. }) => {
                Ok(self.forges.forge(&forge_ref).await.map_err(ResourceError::other)?.map(|forge| forge.https_url))
            }
            // Other consumers are no forge's daemon identity.
            ForgeIdentityEvent::Credential(_) => Ok(None),
        }
    }
}

/// What a Forge or CredentialSpec change says about the daemon's forge identities.
enum ForgeIdentityEvent {
    Forge(ForgeSpec),
    Credential(CredentialConsumer),
}

impl SecondaryWatch for ForgeIdentityRepositoryWake {
    type Primary = Repository;

    fn clone_box(&self) -> Box<dyn SecondaryWatch<Primary = Self::Primary>> {
        Box::new(self.clone())
    }

    fn spawn(
        self: Box<Self>,
        backend: ResourceBackend,
        namespace: String,
        sender: WorkQueueSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ResourceError>> + Send>> {
        // A withdrawn forge arrives with the declaration it had, so it wakes the repositories it
        // served. A credential's deletion by name carries no spec, so no forge, and wakes nothing;
        // it comes only from resuming a watch at an earlier position, which this never does.
        Box::pin(async move {
            let forges = self.forges.changes().await.map_err(ResourceError::other)?.map(|spec| Ok(ForgeIdentityEvent::Forge(spec)));
            let credentials = backend.including_replicas::<CredentialSpec>(&namespace).watch().await?.filter_map(|event| {
                futures::future::ready(match event {
                    Ok(ReadWatchEvent::Added(spec) | ReadWatchEvent::Modified(spec) | ReadWatchEvent::Deleted(spec)) => {
                        Some(Ok(ForgeIdentityEvent::Credential(spec.object.spec.consumer)))
                    }
                    Ok(ReadWatchEvent::DeletedByName { .. }) => None,
                    Err(error) => Some(Err(error)),
                })
            });
            let mut events = std::pin::pin!(futures::stream::select(forges, credentials));
            while let Some(event) = events.next().await {
                let Some(url) = self.forge_url(event?).await? else { continue };
                // The Repositories on that forge, through the index of their forge's service URL;
                // every home's copy, since each names the one Repository this controller reconciles.
                let on_forge: BTreeSet<String> =
                    self.repositories.on_forge(&url).await.map_err(ResourceError::other)?.into_iter().map(|record| record.name).collect();
                for name in on_forge {
                    if sender.send(name).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        })
    }
}

impl Reconciler for RepositoryReconciler {
    type Resource = Repository;
    type Prepared = RepositoryStatus;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let repository_key = RepositoryKey(obj.metadata.name.clone());
        obj.spec.verify_key(&repository_key).map_err(ResourceError::invalid)?;
        let mut status = RepositoryStatus::default();

        if let (Some(forge), Some(resolver)) = (obj.spec.forge(), &self.forge_default_branch_resolver) {
            match resolver.default_branch(forge).await {
                Ok(Some(branch)) => {
                    status.default_branch_observations.push(DefaultBranchObservation { branch, provenance: DefaultBranchProvenance::Forge })
                }
                Ok(None) => {}
                Err(diagnostic) => status.diagnostics.push(format!("forge default branch observation failed: {diagnostic}")),
            }
        }

        // The engine's checkouts of this repository that have not ended: an adopted one is its
        // finder's, a made one its holder's. The discovered ones are as the refresh found them.
        let reads = Arc::clone(&self.lifecycle_reads);
        let key = repository_key.clone();
        let engine = tokio::task::spawn_blocking(carry(move || reads.checkouts().and_then(|checkouts| checkouts.of_repository(&key))))
            .await
            .map_err(|error| ResourceError::other(format!("engine read: {error}")))?
            .map_err(ResourceError::other)?;
        let engine = latest_launches(engine).into_values().filter(|record| !record.ended()).map(|record| {
            let authority =
                if matches!(record.spec, CheckoutSpec::Observed(_)) { LifecycleAuthority::Adopted } else { LifecycleAuthority::Managed };
            (record.checkout_name().to_owned(), record.spec, authority)
        });
        let discovered = self
            .discovered
            .all()
            .into_iter()
            .map(|checkout| (checkout.name, CheckoutSpec::Observed(checkout.spec), LifecycleAuthority::Observed));
        for (checkout_ref, spec, authority) in engine.chain(discovered) {
            if spec.repo_ref() != &repository_key {
                continue;
            }
            let host_ref = match self.checkout_host(&spec).await {
                Ok(host_ref) => host_ref,
                Err(diagnostic) => {
                    status.diagnostics.push(diagnostic);
                    continue;
                }
            };
            if let CheckoutSpec::Observed(spec) = &spec {
                if spec.is_main && matches!(spec.r#ref.as_str(), "main" | "master" | "trunk") {
                    status
                        .default_branch_observations
                        .push(DefaultBranchObservation { branch: spec.r#ref.clone(), provenance: DefaultBranchProvenance::LocalTrunk });
                }
            }
            let kind = spec.repository_checkout_kind();
            status.checkouts_by_host.entry(host_ref).or_default().push(RepositoryCheckoutRef { checkout_ref, kind, authority });
        }

        // Each clone launch found the remote's default branch when it was realised. The engine's
        // reads take the store's lock, so they run off the async workers.
        let reads = Arc::clone(&self.lifecycle_reads);
        let key = repository_key.clone();
        let clones = tokio::task::spawn_blocking(carry(move || reads.clones().and_then(|clones| clones.of_repository(&key))))
            .await
            .map_err(|error| ResourceError::other(format!("engine read: {error}")))?
            .map_err(ResourceError::other)?;
        for branch in clones.into_iter().filter_map(|clone| clone.default_branch) {
            status
                .default_branch_observations
                .push(DefaultBranchObservation { branch, provenance: DefaultBranchProvenance::RemoteSymbolicHead });
        }

        for checkouts in status.checkouts_by_host.values_mut() {
            checkouts.sort_by(|left, right| left.checkout_ref.cmp(&right.checkout_ref));
            checkouts.dedup();
        }
        status.default_branch_observations.sort();
        status.default_branch_observations.dedup();
        let (default_branch, diagnostics) = resolve_default_branch(&status.default_branch_observations);
        status.default_branch = default_branch;
        status.diagnostics.extend(diagnostics);
        status.diagnostics.sort();
        status.diagnostics.dedup();
        Ok(status)
    }

    fn reconcile(
        &self,
        _obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        _now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        ReconcileOutcome::new(Some(RepositoryStatusPatch::Replace(prepared.clone())))
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}
