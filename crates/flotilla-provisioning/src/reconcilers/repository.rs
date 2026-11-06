use std::{marker::PhantomData, sync::Arc};

use crate::engine::clones::CloneRepositoryWake;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    lifecycle_reads::{LifecycleReads, NoLifecycleReads},
    resolve_default_branch, Checkout, CheckoutSpec, DefaultBranchObservation, DefaultBranchProvenance, ForgeIdentity, LifecycleAuthority,
    Repository, RepositoryCheckoutRef, RepositoryKey, RepositoryStatus, RepositoryStatusPatch, ResourceError, ResourceObject,
    REPO_KEY_LABEL,
};
use flotilla_store::{
    controller::{LabelMappedWatch, ReconcileOutcome, Reconciler, ResolverLabelMappedWatch, SecondaryWatch},
    ResourceBackend, TypedResolver,
};

#[async_trait]
pub trait ForgeDefaultBranchResolver: Send + Sync {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String>;
}

pub struct RepositoryReconciler {
    checkout_sources: Vec<TypedResolver<Checkout>>,
    /// The engine's environments and clones, through its readings: a checkout's host is its
    /// environment's, and each clone launch found the remote's default branch.
    lifecycle_reads: Arc<dyn LifecycleReads>,
    forge_default_branch_resolver: Option<Arc<dyn ForgeDefaultBranchResolver>>,
}

impl RepositoryReconciler {
    pub fn new(durable: ResourceBackend, observed: ResourceBackend, namespace: &str) -> Self {
        Self {
            checkout_sources: vec![durable.using::<Checkout>(namespace), observed.using::<Checkout>(namespace)],
            lifecycle_reads: Arc::new(NoLifecycleReads),
            forge_default_branch_resolver: None,
        }
    }

    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }

    pub fn with_forge_default_branch_resolver(mut self, resolver: Arc<dyn ForgeDefaultBranchResolver>) -> Self {
        self.forge_default_branch_resolver = Some(resolver);
        self
    }

    /// The kinds a repository follows: its checkouts, and with the engine's `service` its clones.
    pub fn secondary_watches(
        observed: ResourceBackend,
        namespace: &str,
        service: Option<Arc<flotilla_schema_sqlite::stream::Service>>,
    ) -> Vec<Box<dyn SecondaryWatch<Primary = Repository>>> {
        let mut watches: Vec<Box<dyn SecondaryWatch<Primary = Repository>>> = vec![
            Box::new(LabelMappedWatch::<Checkout, Repository> { label_key: REPO_KEY_LABEL, _marker: PhantomData }),
            Box::new(ResolverLabelMappedWatch::<Checkout, Repository> {
                label_key: REPO_KEY_LABEL,
                resolver: observed.using::<Checkout>(namespace),
                _marker: PhantomData,
            }),
        ];
        if let Some(service) = service {
            watches.push(Box::new(CloneRepositoryWake::new(service)));
        }
        watches
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

        for source in &self.checkout_sources {
            for checkout in source.list().await?.items {
                if checkout.spec.repo_ref() != &repository_key {
                    continue;
                }
                let host_ref = match self.checkout_host(&checkout.spec).await {
                    Ok(host_ref) => host_ref,
                    Err(diagnostic) => {
                        status.diagnostics.push(diagnostic);
                        continue;
                    }
                };
                if let CheckoutSpec::Observed(spec) = &checkout.spec {
                    if spec.is_main && matches!(spec.r#ref.as_str(), "main" | "master" | "trunk") {
                        status
                            .default_branch_observations
                            .push(DefaultBranchObservation { branch: spec.r#ref.clone(), provenance: DefaultBranchProvenance::LocalTrunk });
                    }
                }
                let kind = checkout.spec.repository_checkout_kind();
                let authority = checkout.metadata.lifecycle_authority()?.unwrap_or(LifecycleAuthority::Managed);
                status.checkouts_by_host.entry(host_ref).or_default().push(RepositoryCheckoutRef {
                    checkout_ref: checkout.metadata.name,
                    kind,
                    authority,
                });
            }
        }

        // Each clone launch found the remote's default branch when it was realised. The engine's
        // reads take the store's lock, so they run off the async workers.
        let reads = Arc::clone(&self.lifecycle_reads);
        let key = repository_key.clone();
        let clones = tokio::task::spawn_blocking(move || reads.clones().and_then(|clones| clones.of_repository(&key)))
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
