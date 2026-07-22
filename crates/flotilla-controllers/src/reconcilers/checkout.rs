use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler},
    Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, Clone,
    ClonePhase, IntegrationCondition, ResourceBackend, ResourceError, ResourceObject, TypedResolver,
};
use tracing::warn;

const CHECKOUT_INTEGRATION_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemoval {
    Worktree { clone_path: String, branch: String, target_path: String },
    FreshClone { target_path: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchPreservationReason {
    CommitsPastBase,
    CheckedOutElsewhere,
    NotCreatedForConvoy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemovalOutcome {
    Removed,
    PreservedBranch { branch: String, reason: BranchPreservationReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCheckout {
    pub commit: Option<String>,
    pub branch_provenance: CheckoutBranchProvenance,
}

#[async_trait]
pub trait CheckoutRuntime: Send + Sync {
    async fn create_worktree(
        &self,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String>;
    async fn create_fresh_clone(
        &self,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String>;
    async fn inspect_integration(&self, checkout: &ResourceObject<Checkout>) -> Result<CheckoutIntegrationStatus, String>;
    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String>;
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
    Ready { prepared: PreparedCheckout },
    Integration { status: CheckoutIntegrationStatus },
    Waiting,
    Failed(String),
}

fn integration_observed_at(condition: &IntegrationCondition) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(condition.observed_at.as_deref()?).ok().map(|observed_at| observed_at.with_timezone(&Utc))
}

fn integration_is_fresh(status: &CheckoutStatus, now: DateTime<Utc>) -> bool {
    let observed_at = [
        integration_observed_at(&status.integration.clean),
        integration_observed_at(&status.integration.pushed),
        integration_observed_at(&status.integration.landed),
    ];
    let Some(oldest_observation) = observed_at.into_iter().collect::<Option<Vec<_>>>().and_then(|values| values.into_iter().min()) else {
        return false;
    };
    now.signed_duration_since(oldest_observation).to_std().is_ok_and(|age| age < CHECKOUT_INTEGRATION_REFRESH_AFTER)
}

impl<R> Reconciler for CheckoutReconciler<R>
where
    R: CheckoutRuntime + 'static,
{
    type Resource = Checkout;
    type Dependencies = CheckoutDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) != CheckoutPhase::Pending {
            if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Ready && !integration_is_fresh(status, Utc::now())) {
                return Ok(match self.runtime.inspect_integration(obj).await {
                    Ok(status) => CheckoutDeps::Integration { status },
                    Err(err) => CheckoutDeps::Failed(err),
                });
            }
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
                    Ok(prepared) => CheckoutDeps::Ready { prepared },
                    Err(err) => CheckoutDeps::Failed(err),
                })
            }
            CheckoutSpec::FreshClone(spec) => {
                Ok(match self.runtime.create_fresh_clone(&spec.url, &spec.r#ref, spec.base_ref.as_deref(), &spec.target_path).await {
                    Ok(prepared) => CheckoutDeps::Ready { prepared },
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
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) == CheckoutPhase::Pending {
            match deps {
                CheckoutDeps::Ready { prepared } => obj.spec.target_path().map(|path| CheckoutStatusPatch::MarkReady {
                    path: path.to_string(),
                    commit: prepared.commit.clone(),
                    branch_provenance: prepared.branch_provenance,
                }),
                CheckoutDeps::Integration { .. } => None,
                CheckoutDeps::Failed(message) => Some(CheckoutStatusPatch::MarkFailed { message: message.clone() }),
                CheckoutDeps::Waiting | CheckoutDeps::None => None,
            }
        } else if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Ready) {
            match deps {
                CheckoutDeps::Integration { status } => Some(CheckoutStatusPatch::UpdateIntegration { integration: status.clone() }),
                CheckoutDeps::Failed(message) => Some(CheckoutStatusPatch::UpdateIntegration {
                    integration: CheckoutIntegrationStatus {
                        clean: flotilla_resources::IntegrationCondition::builder()
                            .value(flotilla_resources::ConditionValue::Unknown)
                            .details(vec![message.clone()])
                            .observed_at(now.to_rfc3339())
                            .build(),
                        pushed: flotilla_resources::IntegrationCondition::builder()
                            .value(flotilla_resources::ConditionValue::Unknown)
                            .details(vec![message.clone()])
                            .observed_at(now.to_rfc3339())
                            .build(),
                        landed: flotilla_resources::IntegrationCondition::builder()
                            .value(flotilla_resources::ConditionValue::Unknown)
                            .details(vec![message.clone()])
                            .observed_at(now.to_rfc3339())
                            .build(),
                        landed_evidence: None,
                    },
                }),
                CheckoutDeps::None | CheckoutDeps::Ready { .. } | CheckoutDeps::Waiting => None,
            }
        } else {
            None
        };

        ReconcileOutcome::new(patch)
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let removal = match &obj.spec {
            CheckoutSpec::Worktree(spec) => {
                let clone = self.clones.get(&spec.clone_ref).await?;
                CheckoutRemoval::Worktree { clone_path: clone.spec.path, branch: spec.r#ref.clone(), target_path: spec.target_path.clone() }
            }
            CheckoutSpec::FreshClone(spec) => CheckoutRemoval::FreshClone { target_path: spec.target_path.clone() },
            CheckoutSpec::Observed(_) => return Ok(()),
        };
        let outcome = self.runtime.remove_checkout(&removal).await.map_err(ResourceError::other)?;
        if let CheckoutRemovalOutcome::PreservedBranch { branch, reason } = outcome {
            warn!(%branch, ?reason, "preserved branch during checkout cleanup");
        }
        Ok(())
    }

    fn finalizer_name(&self) -> Option<&'static str> {
        Some("flotilla.work/checkout-cleanup")
    }
}
