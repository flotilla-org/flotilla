use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    Checkout, CheckoutPhase, CheckoutSpec, CheckoutStatusPatch, Clone, ClonePhase, ResourceBackend, ResourceError, ResourceObject,
    TypedResolver,
};

#[async_trait]
pub trait CheckoutRuntime: Send + Sync {
    async fn create_worktree(
        &self,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<Option<String>, String>;
    async fn create_fresh_clone(
        &self,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<Option<String>, String>;
    async fn remove_checkout(&self, target_path: &str) -> Result<(), String>;
}

pub struct CheckoutReconciler<R> {
    runtime: Arc<R>,
    clones: TypedResolver<Clone>,
}

impl<R> CheckoutReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self { runtime, clones: backend.using::<Clone>(namespace) }
    }
}

pub enum CheckoutDeps {
    None,
    Ready { commit: Option<String> },
    Waiting,
    Failed(String),
}

impl<R> Reconciler for CheckoutReconciler<R>
where
    R: CheckoutRuntime + 'static,
{
    type Resource = Checkout;
    type Dependencies = CheckoutDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) != CheckoutPhase::Pending {
            return Ok(CheckoutDeps::None);
        }

        match &obj.spec {
            CheckoutSpec::Worktree(spec) => {
                let clone = match self.clones.get(&spec.clone_ref).await {
                    Ok(clone) => clone,
                    Err(ResourceError::NotFound { .. }) => return Ok(CheckoutDeps::Waiting),
                    Err(err) => return Err(err),
                };
                if clone.status.as_ref().map(|status| status.phase) != Some(ClonePhase::Ready) {
                    return Ok(CheckoutDeps::Waiting);
                }
                if clone.spec.env_ref != spec.env_ref {
                    return Ok(CheckoutDeps::Failed("worktree clone env_ref mismatch".to_string()));
                }
                Ok(match self.runtime.create_worktree(&clone.spec.path, &spec.r#ref, spec.base_ref.as_deref(), &spec.target_path).await {
                    Ok(commit) => CheckoutDeps::Ready { commit },
                    Err(err) => CheckoutDeps::Failed(err),
                })
            }
            CheckoutSpec::FreshClone(spec) => {
                Ok(match self.runtime.create_fresh_clone(&spec.url, &spec.r#ref, spec.base_ref.as_deref(), &spec.target_path).await {
                    Ok(commit) => CheckoutDeps::Ready { commit },
                    Err(err) => CheckoutDeps::Failed(err),
                })
            }
            // Observed checkouts are facts from the observed-resource backend.
            // The managed checkout reconciler must not actuate or patch them.
            CheckoutSpec::Observed(_) => Ok(CheckoutDeps::None),
        }
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) == CheckoutPhase::Pending {
            match deps {
                CheckoutDeps::Ready { commit } => {
                    obj.spec.target_path().map(|path| CheckoutStatusPatch::MarkReady { path: path.to_string(), commit: commit.clone() })
                }
                CheckoutDeps::Failed(message) => Some(CheckoutStatusPatch::MarkFailed { message: message.clone() }),
                CheckoutDeps::Waiting | CheckoutDeps::None => None,
            }
        } else {
            None
        };

        ReconcileOutcome::new(patch)
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let Some(target_path) = obj.spec.target_path() else {
            return Ok(());
        };
        self.runtime.remove_checkout(target_path).await.map_err(ResourceError::other)
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/checkout-cleanup")
    }
}
