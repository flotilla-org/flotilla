use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    controller::{LabelMappedWatch, ReconcileOutcome, Reconciler, ResolverLabelMappedWatch, SecondaryWatch},
    resolve_default_branch, Checkout, CheckoutSpec, Clone, DefaultBranchObservation, DefaultBranchProvenance, Environment, ForgeIdentity,
    LifecycleAuthority, Repository, RepositoryCheckoutRef, RepositoryKey, RepositoryStatus, RepositoryStatusPatch, ResourceBackend,
    ResourceError, ResourceObject, TypedResolver, REPO_KEY_LABEL,
};

#[async_trait]
pub trait ForgeDefaultBranchResolver: Send + Sync {
    async fn default_branch(&self, forge: &ForgeIdentity) -> Result<Option<String>, String>;
}

#[derive(bon::Builder)]
pub struct RepositoryReconciler {
    checkout_sources: Vec<TypedResolver<Checkout>>,
    clones: TypedResolver<Clone>,
    environments: TypedResolver<Environment>,
    forge_default_branch_resolver: Option<Arc<dyn ForgeDefaultBranchResolver>>,
}

impl RepositoryReconciler {
    pub fn new(durable: ResourceBackend, observed: ResourceBackend, namespace: &str) -> Self {
        Self {
            checkout_sources: vec![durable.clone().using::<Checkout>(namespace), observed.using::<Checkout>(namespace)],
            clones: durable.clone().using::<Clone>(namespace),
            environments: durable.using::<Environment>(namespace),
            forge_default_branch_resolver: None,
        }
    }

    pub fn with_forge_default_branch_resolver(mut self, resolver: Arc<dyn ForgeDefaultBranchResolver>) -> Self {
        self.forge_default_branch_resolver = Some(resolver);
        self
    }

    pub fn secondary_watches(observed: ResourceBackend, namespace: &str) -> Vec<Box<dyn SecondaryWatch<Primary = Repository>>> {
        vec![
            Box::new(LabelMappedWatch::<Checkout, Repository> { label_key: REPO_KEY_LABEL, _marker: PhantomData }),
            Box::new(LabelMappedWatch::<Clone, Repository> { label_key: REPO_KEY_LABEL, _marker: PhantomData }),
            Box::new(ResolverLabelMappedWatch::<Checkout, Repository> {
                label_key: REPO_KEY_LABEL,
                resolver: observed.using::<Checkout>(namespace),
                _marker: PhantomData,
            }),
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
            .environments
            .get(environment_ref)
            .await
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
    type Dependencies = RepositoryStatus;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
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

        for clone in self.clones.list().await?.items {
            if clone.spec.repo_ref != repository_key {
                continue;
            }
            if let Some(branch) = clone.status.and_then(|status| status.default_branch) {
                status
                    .default_branch_observations
                    .push(DefaultBranchObservation { branch, provenance: DefaultBranchProvenance::RemoteSymbolicHead });
            }
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
        deps: &Self::Dependencies,
        _now: DateTime<Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        ReconcileOutcome::new(Some(RepositoryStatusPatch::Replace(deps.clone())))
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        None
    }
}
