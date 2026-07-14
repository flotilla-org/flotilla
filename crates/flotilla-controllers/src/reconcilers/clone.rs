use std::sync::Arc;

use async_trait::async_trait;
use flotilla_resources::{
    clone_key,
    controller::{ReconcileOutcome, Reconciler},
    Clone, ClonePhase, CloneStatusPatch, Repository, RepositoryIdentity, ResourceError, ResourceObject, TypedResolver,
};

#[async_trait]
pub trait CloneRuntime: Send + Sync {
    async fn clone_and_inspect(&self, repo_url: &str, target_path: &str) -> Result<Option<String>, String>;
    async fn inspect_existing(&self, target_path: &str) -> Result<Option<String>, String>;
}

pub struct CloneReconciler<R> {
    runtime: Arc<R>,
    repositories: TypedResolver<Repository>,
}

impl<R> CloneReconciler<R> {
    pub fn new(runtime: Arc<R>, repositories: TypedResolver<Repository>) -> Self {
        Self { runtime, repositories }
    }
}

pub enum CloneDeps {
    None,
    Ready { default_branch: Option<String> },
    Failed(String),
}

impl<R> Reconciler for CloneReconciler<R>
where
    R: CloneRuntime + 'static,
{
    type Resource = Clone;
    type Dependencies = CloneDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        let repository = match self.repositories.get(&obj.spec.repo_ref.to_string()).await {
            Ok(repository) => repository,
            Err(ResourceError::NotFound { .. }) => return Ok(CloneDeps::Failed(format!("repository {} not found", obj.spec.repo_ref))),
            Err(error) => return Err(error),
        };
        if let Err(message) = repository.spec.verify_key(&obj.spec.repo_ref) {
            return Ok(CloneDeps::Failed(message));
        }
        let canonical_repo = match repository.spec.identity() {
            RepositoryIdentity::Remote { canonical_remote } => canonical_remote,
            RepositoryIdentity::Local { .. } => return Ok(CloneDeps::Failed("clone repository must have a transport remote".to_string())),
        };
        let expected_name = format!("clone-{}", clone_key(canonical_repo, &obj.spec.env_ref));
        if obj.metadata.name != expected_name {
            return Ok(CloneDeps::Failed(format!("clone name mismatch: expected {expected_name}")));
        }
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(ClonePhase::Pending) != ClonePhase::Pending {
            return Ok(CloneDeps::None);
        }

        let result = if obj.metadata.labels.get("flotilla.work/discovered").map(String::as_str) == Some("true") {
            self.runtime.inspect_existing(&obj.spec.path).await
        } else {
            self.runtime.clone_and_inspect(&obj.spec.url, &obj.spec.path).await
        };
        Ok(match result {
            Ok(default_branch) => CloneDeps::Ready { default_branch },
            Err(err) => CloneDeps::Failed(err),
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        deps: &Self::Dependencies,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = if obj.status.as_ref().map(|status| status.phase).unwrap_or(ClonePhase::Pending) == ClonePhase::Pending {
            match deps {
                CloneDeps::Ready { default_branch } => Some(CloneStatusPatch::MarkReady { default_branch: default_branch.clone() }),
                CloneDeps::Failed(message) => Some(CloneStatusPatch::MarkFailed { message: message.clone() }),
                CloneDeps::None => None,
            }
        } else {
            None
        };

        ReconcileOutcome::new(patch)
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/clone-cleanup")
    }
}
