use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use flotilla_resources::{
    clone_key,
    controller::{ReconcileOutcome, Reconciler},
    Clone, ClonePhase, CloneStatusPatch, ObjectEvent, Repository, RepositoryIdentity, ResourceError, ResourceObject, TypedResolver,
};

const CLONE_RETRY_AFTER: Duration = Duration::from_secs(30);

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

pub enum ClonePrepared {
    None,
    Ready { default_branch: Option<String> },
    Retrying(String),
    Failed(String),
}

impl<R> Reconciler for CloneReconciler<R>
where
    R: CloneRuntime + 'static,
{
    type Resource = Clone;
    type Prepared = ClonePrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let repository = match self.repositories.get(&obj.spec.repo_ref.to_string()).await {
            Ok(repository) => repository,
            Err(ResourceError::NotFound { .. }) => return Ok(ClonePrepared::Failed(format!("repository {} not found", obj.spec.repo_ref))),
            Err(error) => return Err(error),
        };
        if let Err(message) = repository.spec.verify_key(&obj.spec.repo_ref) {
            return Ok(ClonePrepared::Failed(message));
        }
        let canonical_repo = match repository.spec.identity() {
            RepositoryIdentity::Remote { canonical_remote } => canonical_remote,
            RepositoryIdentity::Local { .. } => {
                return Ok(ClonePrepared::Failed("clone repository must have a transport remote".to_string()))
            }
        };
        let expected_name = format!("clone-{}", clone_key(canonical_repo, &obj.spec.env_ref));
        if obj.metadata.name != expected_name {
            return Ok(ClonePrepared::Failed(format!("clone name mismatch: expected {expected_name}")));
        }
        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(ClonePhase::Pending);
        if !matches!(phase, ClonePhase::Pending | ClonePhase::Cloning) {
            return Ok(ClonePrepared::None);
        }

        let result = if obj.metadata.labels.get("flotilla.work/discovered").map(String::as_str) == Some("true") {
            self.runtime.inspect_existing(&obj.spec.path).await
        } else {
            self.runtime.clone_and_inspect(&obj.spec.url, &obj.spec.path).await
        };
        Ok(match result {
            Ok(default_branch) => ClonePrepared::Ready { default_branch },
            Err(err) => ClonePrepared::Retrying(err),
        })
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let phase = obj.status.as_ref().map(|status| status.phase).unwrap_or(ClonePhase::Pending);
        let patch = if matches!(phase, ClonePhase::Pending | ClonePhase::Cloning) {
            match prepared {
                ClonePrepared::Ready { default_branch } => Some(CloneStatusPatch::MarkReady { default_branch: default_branch.clone() }),
                ClonePrepared::Retrying(message) => Some(CloneStatusPatch::MarkRetrying { message: message.clone() }),
                ClonePrepared::Failed(message) => Some(CloneStatusPatch::MarkFailed { message: message.clone(), failed_at: now }),
                ClonePrepared::None => None,
            }
        } else {
            None
        };

        let mut outcome = ReconcileOutcome::new(patch);
        if let ClonePrepared::Failed(message) = prepared {
            outcome.events.push(ObjectEvent::for_object(obj, "CloneFailed", message.clone()));
        }
        if matches!(prepared, ClonePrepared::Retrying(_)) {
            outcome.requeue_after = Some(CLONE_RETRY_AFTER);
        }
        outcome
    }

    async fn run_finalizer(&self, _obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/clone-cleanup")
    }
}
