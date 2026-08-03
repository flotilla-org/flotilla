use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::checkout_integration::{
    checkout_observation_lacks_convoy_association, convoy_change_request_id_for_checkout, LANDING_EVIDENCE_TTL,
};
use flotilla_resources::{
    controller::{ReconcileOutcome, Reconciler, ReplicaConvoyCheckoutWatch, SecondaryWatch},
    Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec, CheckoutStatus, CheckoutStatusPatch, Clock,
    Clone, ClonePhase, Convoy, ConvoyPhase, IntegrationCondition, ReplicaReadResolver, ResourceBackend, ResourceError, ResourceObject,
    ResourceProvenance, SystemClock, TypedResolver, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL,
};
use tracing::warn;

const CHECKOUT_INTEGRATION_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
const CHECKOUT_PROVISIONING_REQUEUE_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemoval {
    Worktree { clone_path: String, branch: String, target_path: String },
    OrphanedWorktree { target_path: String },
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
    async fn inspect_integration(
        &self,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String>;
    async fn remove_checkout(&self, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String>;
}

pub struct CheckoutReconciler<R> {
    runtime: Arc<R>,
    clones: TypedResolver<Clone>,
    convoys: TypedResolver<Convoy>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    clock: Arc<dyn Clock>,
}

impl<R> CheckoutReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self::with_clock(runtime, backend, namespace, Arc::new(SystemClock))
    }

    pub fn with_clock(runtime: Arc<R>, backend: ResourceBackend, namespace: &str, clock: Arc<dyn Clock>) -> Self {
        Self {
            runtime,
            clones: backend.clone().using::<Clone>(namespace),
            convoys: backend.using::<Convoy>(namespace),
            federated_convoys: None,
            clock,
        }
    }

    pub fn with_federated_convoys(mut self, backend: &ResourceBackend, namespace: &str) -> Self {
        self.federated_convoys = Some(backend.including_replicas::<Convoy>(namespace));
        self
    }

    pub fn federated_secondary_watches(backend: &ResourceBackend, namespace: &str) -> Vec<Box<dyn SecondaryWatch<Primary = Checkout>>> {
        vec![Box::new(ReplicaConvoyCheckoutWatch { resolver: backend.including_replicas::<Convoy>(namespace) })]
    }

    async fn owning_convoy(&self, checkout: &ResourceObject<Checkout>) -> Result<Option<ResourceObject<Convoy>>, ResourceError> {
        let convoy_ref = checkout.metadata.labels.get(CONVOY_LABEL);
        let Some(origin) = checkout.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION) else {
            if let Some(convoy_ref) = convoy_ref {
                match self.convoys.get(convoy_ref).await {
                    Ok(convoy) => return Ok(Some(convoy)),
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            if let Some(convoy) =
                self.convoys.list().await?.items.into_iter().find(|convoy| convoy_claims_checkout(convoy, &checkout.metadata.name))
            {
                return Ok(Some(convoy));
            }
            return self.any_federated_convoy(convoy_ref.map(String::as_str), &checkout.metadata.name).await;
        };
        let Some(federated) = self.federated_convoys.as_ref() else {
            return Ok(None);
        };
        Ok(federated.list().await?.items.into_iter().find_map(|source| {
            (convoy_ref.map_or_else(
                || convoy_claims_checkout(&source.object, &checkout.metadata.name),
                |convoy_ref| source.object.metadata.name == *convoy_ref,
            ) && matches!(&source.provenance, ResourceProvenance::Replica { origin_root, .. } if origin_root.as_str() == origin))
            .then_some(source.object)
        }))
    }

    async fn any_federated_convoy(
        &self,
        convoy_ref: Option<&str>,
        checkout_name: &str,
    ) -> Result<Option<ResourceObject<Convoy>>, ResourceError> {
        let Some(federated) = self.federated_convoys.as_ref() else {
            return Ok(None);
        };
        Ok(federated.list().await?.items.into_iter().find_map(|source| {
            convoy_ref
                .map_or_else(
                    || convoy_claims_checkout(&source.object, checkout_name),
                    |convoy_ref| source.object.metadata.name == convoy_ref,
                )
                .then_some(source.object)
        }))
    }
}

fn convoy_claims_checkout(convoy: &ResourceObject<Convoy>, checkout_name: &str) -> bool {
    flotilla_resources::expected_checkout_refs(convoy).is_ok_and(|expected| expected.contains(checkout_name))
}

pub enum CheckoutDeps {
    None,
    Ready { prepared: PreparedCheckout },
    Integration { status: Box<CheckoutIntegrationStatus> },
    Waiting,
    Failed(String),
}

fn integration_observed_at(condition: &IntegrationCondition) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(condition.observed_at.as_deref()?).ok().map(|observed_at| observed_at.with_timezone(&Utc))
}

fn integration_is_fresh(status: &CheckoutStatus, now: DateTime<Utc>, max_age: Duration) -> bool {
    let observed_at = [
        integration_observed_at(&status.integration.clean),
        integration_observed_at(&status.integration.pushed),
        integration_observed_at(&status.integration.landed),
    ];
    let Some(oldest_observation) = observed_at.into_iter().collect::<Option<Vec<_>>>().and_then(|values| values.into_iter().min()) else {
        return false;
    };
    now.signed_duration_since(oldest_observation).to_std().is_ok_and(|age| age < max_age)
}

/// Terminal convoys keep the Landing TTL because teardown consumes the same
/// fresh evidence and no longer probes a checkout on demand.
fn convoy_needs_terminal_evidence(convoy: Option<&ResourceObject<Convoy>>) -> bool {
    convoy.and_then(|convoy| convoy.status.as_ref()).is_some_and(|status| {
        status.phase == ConvoyPhase::Landing || (status.phase.is_terminal() && status.phase != ConvoyPhase::Abandoned)
    })
}

impl<R> Reconciler for CheckoutReconciler<R>
where
    R: CheckoutRuntime + 'static,
{
    type Resource = Checkout;
    type Dependencies = CheckoutDeps;

    async fn fetch_dependencies(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Dependencies, ResourceError> {
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) != CheckoutPhase::Pending {
            let convoy = self.owning_convoy(obj).await?;
            let terminal_evidence = convoy_needs_terminal_evidence(convoy.as_ref());
            let refresh_after = if terminal_evidence { LANDING_EVIDENCE_TTL } else { CHECKOUT_INTEGRATION_REFRESH_AFTER };
            let expected_change_request_id = convoy.as_ref().and_then(|convoy| convoy_change_request_id_for_checkout(convoy, obj));
            if obj.status.as_ref().is_some_and(|status| {
                status.phase == CheckoutPhase::Ready
                    && (!integration_is_fresh(status, self.clock.now(), refresh_after)
                        || (terminal_evidence && checkout_observation_lacks_convoy_association(&status.integration))
                        || expected_change_request_id.as_ref().is_some_and(|expected| {
                            status.integration.change_request.as_ref().is_some_and(|observed| &observed.id != expected)
                        }))
            }) {
                return Ok(match self.runtime.inspect_integration(obj, convoy.as_ref()).await {
                    Ok(status) => CheckoutDeps::Integration { status: Box::new(status) },
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
                CheckoutDeps::Integration { status } => {
                    Some(CheckoutStatusPatch::UpdateIntegration { integration: Box::new(status.as_ref().clone()) })
                }
                CheckoutDeps::Failed(message) => Some(CheckoutStatusPatch::UpdateIntegration {
                    integration: Box::new(CheckoutIntegrationStatus {
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
                        change_request: None,
                    }),
                }),
                CheckoutDeps::None | CheckoutDeps::Ready { .. } | CheckoutDeps::Waiting => None,
            }
        } else {
            None
        };

        let mut outcome = ReconcileOutcome::new(patch);
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) == CheckoutPhase::Pending
            && !matches!(deps, CheckoutDeps::Failed(_))
        {
            outcome.requeue_after = Some(CHECKOUT_PROVISIONING_REQUEUE_AFTER);
        }
        outcome
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let removal = match &obj.spec {
            CheckoutSpec::Worktree(spec) => match self.clones.get(&spec.clone_ref).await {
                Ok(clone) => CheckoutRemoval::Worktree {
                    clone_path: clone.spec.path,
                    branch: spec.r#ref.clone(),
                    target_path: spec.target_path.clone(),
                },
                Err(ResourceError::NotFound { .. }) => CheckoutRemoval::OrphanedWorktree { target_path: spec.target_path.clone() },
                Err(err) => return Err(err),
            },
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

    fn finalizer_error_patch(&self, obj: &ResourceObject<Self::Resource>, error: &ResourceError) -> Option<CheckoutStatusPatch> {
        let message = format!("checkout teardown failed: {error}");
        if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Failed && status.message.as_deref() == Some(&message)) {
            return None;
        }
        Some(CheckoutStatusPatch::MarkFailed { message })
    }
}
