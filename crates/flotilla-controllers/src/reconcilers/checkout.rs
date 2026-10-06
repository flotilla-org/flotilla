use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::{
    checkout_integration::{checkout_observation_lacks_convoy_association, convoy_change_request_id_for_checkout, LANDING_EVIDENCE_TTL},
    config::DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY,
    vcs::CheckoutMaterialisationError,
};
use flotilla_resources::{
    apply_status_patch,
    controller::{
        Actuation, ReconcileErrorExhaustion, ReconcileErrorPolicy, ReconcileOutcome, Reconciler, ReplicaConvoyCheckoutWatch, SecondaryWatch,
    },
    convoy_sanctions_checkout_reclaim, Checkout, CheckoutBranchProvenance, CheckoutIntegrationStatus, CheckoutPhase, CheckoutSpec,
    CheckoutStatus, CheckoutStatusPatch, Clock, Clone, CloneFailurePolicy, ClonePhase, Convoy, ConvoyPhase, EventRecorder, Forge,
    IntegrationCondition, LifecycleAuthority, ObjectEvent, ReplicaReadResolver, Resource, ResourceBackend, ResourceError, ResourceObject,
    ResourceProvenance, SystemClock, TypedResolver, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL, FORCE_TEARDOWN_ANNOTATION,
};
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, warn};

const CHECKOUT_INTEGRATION_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);
const CHECKOUT_PROVISIONING_REQUEUE_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutRemoval {
    Worktree { clone_path: String, branch: String, target_path: String },
    ForcedWorktree { clone_path: String, branch: String, target_path: String },
    LandedWorktree { clone_path: String, branch: String, target_path: String },
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
    ArchivedAndRemoved { archive_path: String },
    PreservedBranch { branch: String, reason: BranchPreservationReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCheckout {
    pub commit: Option<String>,
    pub branch_provenance: CheckoutBranchProvenance,
}

#[async_trait]
pub trait CheckoutRuntime: Send + Sync {
    /// Refuse a new branch that is already a forge change-request head.
    /// Existing targets are retries and must remain recoverable.
    /// Ok(Some) names a permanent conflict; Err is a retryable lookup failure.
    async fn validate_new_branch(&self, _checkout: &ResourceObject<Checkout>) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Restore registration protection through the checkout's owning environment.
    async fn protect_worktree_in(&self, _env_ref: &str, _clone_path: &str, _target: &str, _reason: &str) -> Result<(), String> {
        Ok(())
    }
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
    /// `Some(false)` is authoritative only when checked by the worktree's host.
    async fn checkout_path_exists_in(&self, _env_ref: &str, _path: &str) -> Result<Option<bool>, String> {
        Ok(None)
    }
    async fn create_worktree_in(
        &self,
        _env_ref: &str,
        clone_path: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
        _registration_reason: &str,
    ) -> Result<PreparedCheckout, CheckoutMaterialisationError> {
        self.create_worktree(clone_path, branch, base_ref, target_path).await.map_err(Into::into)
    }
    async fn create_fresh_clone_in(
        &self,
        _env_ref: &str,
        repo_url: &str,
        branch: &str,
        base_ref: Option<&str>,
        target_path: &str,
    ) -> Result<PreparedCheckout, String> {
        self.create_fresh_clone(repo_url, branch, base_ref, target_path).await
    }
    async fn inspect_integration_in(
        &self,
        _env_ref: &str,
        checkout: &ResourceObject<Checkout>,
        convoy: Option<&ResourceObject<Convoy>>,
    ) -> Result<CheckoutIntegrationStatus, String> {
        self.inspect_integration(checkout, convoy).await
    }
    async fn remove_checkout_in(&self, _env_ref: &str, removal: &CheckoutRemoval) -> Result<CheckoutRemovalOutcome, String> {
        self.remove_checkout(removal).await
    }
}

pub struct CheckoutReconciler<R> {
    runtime: Arc<R>,
    checkouts: TypedResolver<Checkout>,
    clones: TypedResolver<Clone>,
    convoys: TypedResolver<Convoy>,
    forges: flotilla_resources::DefinitionResolver<Forge>,
    federated_convoys: Option<ReplicaReadResolver<Convoy>>,
    local_root: Option<flotilla_protocol::NodeId>,
    clock: Arc<dyn Clock>,
    backend: ResourceBackend,
    finalizers: Mutex<BTreeMap<String, JoinHandle<Result<CheckoutRemovalOutcome, String>>>>,
    background_removal_limit: NonZeroUsize,
}

impl<R> CheckoutReconciler<R> {
    pub fn new(runtime: Arc<R>, backend: ResourceBackend, namespace: &str) -> Self {
        Self::with_clock(runtime, backend, namespace, Arc::new(SystemClock))
    }

    pub fn with_clock(runtime: Arc<R>, backend: ResourceBackend, namespace: &str, clock: Arc<dyn Clock>) -> Self {
        let local_root = match backend.local_root() {
            Ok(root) => Some(root),
            Err(error) => {
                warn!(%error, "checkout reconciler cannot resolve local authority root");
                None
            }
        };
        Self {
            runtime,
            checkouts: backend.clone().using::<Checkout>(namespace),
            clones: backend.clone().using::<Clone>(namespace),
            convoys: backend.clone().using::<Convoy>(namespace),
            forges: backend.definitions::<Forge>(namespace),
            federated_convoys: None,
            local_root,
            clock,
            backend: backend.clone(),
            finalizers: Mutex::new(BTreeMap::new()),
            background_removal_limit: DEFAULT_CHECKOUT_REMOVAL_CONCURRENCY,
        }
    }

    /// Bound background task admission as well as the runtime's active removals.
    pub fn with_background_removal_limit(mut self, limit: NonZeroUsize) -> Self {
        self.background_removal_limit = limit;
        self
    }

    async fn observe_clone_retry(
        &self,
        checkout: &ResourceObject<Checkout>,
        retry: Option<flotilla_resources::ControllerRetry>,
    ) -> Result<(), ResourceError> {
        if checkout.status.as_ref().and_then(|status| status.clone_retry.as_ref()) == retry.as_ref() {
            return Ok(());
        }
        apply_status_patch(&self.checkouts, &checkout.metadata.name, &CheckoutStatusPatch::ObserveCloneRetry { retry }).await?;
        Ok(())
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
            ) && match &source.provenance {
                ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str() == origin,
                ResourceProvenance::Local => self.local_root.as_ref().is_some_and(|root| root.as_str() == origin),
            })
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

pub enum CheckoutPrepared {
    None,
    Gone,
    Reappeared,
    OwnerTerminal,
    Ready { prepared: PreparedCheckout },
    Integration { status: Box<CheckoutIntegrationStatus> },
    RetryClone { clone_name: String, failed_at: DateTime<Utc> },
    Waiting,
    ValidationUnavailable(String),
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

/// Diagnostic identity shared by convoy and standalone registration callers.
pub fn managed_checkout_reason(owner: Option<&str>, checkout: &str) -> String {
    format!("flotilla-managed: {}/{}", owner.unwrap_or("standalone"), checkout)
}

fn checkout_registration_reason(checkout: &ResourceObject<Checkout>, convoy: Option<&ResourceObject<Convoy>>) -> String {
    let owner =
        convoy.map(|convoy| convoy.metadata.name.as_str()).or_else(|| checkout.metadata.labels.get(CONVOY_LABEL).map(String::as_str));
    managed_checkout_reason(owner, &checkout.metadata.name)
}

fn convoy_needs_delete_evidence(convoy: Option<&ResourceObject<Convoy>>) -> bool {
    convoy.is_some_and(|convoy| convoy.status.as_ref().is_none_or(|status| status.phase != ConvoyPhase::Abandoned))
}

impl<R> Reconciler for CheckoutReconciler<R>
where
    R: CheckoutRuntime + 'static,
{
    type Resource = Checkout;
    type Prepared = CheckoutPrepared;

    async fn prepare(&self, obj: &ResourceObject<Self::Resource>) -> Result<Self::Prepared, ResourceError> {
        let lifecycle_authority = obj.metadata.lifecycle_authority()?;
        let has_convoy_owner = obj.metadata.labels.contains_key(CONVOY_LABEL)
            || obj.metadata.owner_references.iter().any(|owner| owner.kind == Convoy::API_PATHS.kind);
        let convoy = self.owning_convoy(obj).await?;
        if lifecycle_authority == Some(LifecycleAuthority::Managed)
            && has_convoy_owner
            && convoy.as_ref().is_some_and(convoy_sanctions_checkout_reclaim)
        {
            return Ok(CheckoutPrepared::OwnerTerminal);
        }

        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) != CheckoutPhase::Pending {
            if obj.status.as_ref().is_some_and(|status| matches!(status.phase, CheckoutPhase::Ready | CheckoutPhase::Gone)) {
                if let CheckoutSpec::Worktree(worktree) = &obj.spec {
                    let path = obj.status.as_ref().and_then(|status| status.path.as_deref()).unwrap_or(&worktree.target_path);
                    match self.runtime.checkout_path_exists_in(&worktree.env_ref, path).await {
                        Ok(Some(false)) if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Ready) => {
                            return Ok(CheckoutPrepared::Gone)
                        }
                        Ok(Some(true)) if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Gone) => {
                            return Ok(CheckoutPrepared::Reappeared)
                        }
                        Ok(Some(_)) | Ok(None) => {}
                        Err(error) => return Ok(CheckoutPrepared::Failed(error)),
                    }
                }
            }
            if lifecycle_authority == Some(LifecycleAuthority::Managed)
                && obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Ready)
            {
                if let CheckoutSpec::Worktree(spec) = &obj.spec {
                    let source = match self.clones.get(&spec.clone_ref).await {
                        Ok(clone) => clone.spec.path,
                        Err(ResourceError::NotFound { .. }) => spec.target_path.clone(),
                        Err(error) => return Err(error),
                    };
                    let reason = checkout_registration_reason(obj, convoy.as_ref());
                    if let Err(error) = self.runtime.protect_worktree_in(&spec.env_ref, &source, &spec.target_path, &reason).await {
                        return Err(ResourceError::other(error));
                    }
                }
            }
            let delete_evidence = convoy_needs_delete_evidence(convoy.as_ref());
            let refresh_after = if delete_evidence { LANDING_EVIDENCE_TTL } else { CHECKOUT_INTEGRATION_REFRESH_AFTER };
            let forges = self.forges.list().await?.into_iter().map(|forge| forge.spec).collect::<Vec<_>>();
            let expected_change_request_id = convoy.as_ref().and_then(|convoy| convoy_change_request_id_for_checkout(convoy, obj, &forges));
            if obj.status.as_ref().is_some_and(|status| {
                status.phase == CheckoutPhase::Ready
                    && (!integration_is_fresh(status, self.clock.now(), refresh_after)
                        || (delete_evidence
                            && checkout_observation_lacks_convoy_association(&status.integration)
                            && convoy.as_ref().is_some_and(|convoy| {
                                convoy.status.as_ref().is_some_and(|status| status.branch_subject_scan_at.is_some())
                                    || expected_change_request_id.is_some()
                            }))
                        || expected_change_request_id.as_ref().is_some_and(|expected| {
                            status.integration.change_request.as_ref().is_some_and(|observed| &observed.id != expected)
                        }))
            }) {
                return Ok(match self.runtime.inspect_integration_in(obj.spec.env_ref().unwrap_or(""), obj, convoy.as_ref()).await {
                    Ok(status) => CheckoutPrepared::Integration { status: Box::new(status) },
                    Err(err) => CheckoutPrepared::Failed(err),
                });
            }
            return Ok(CheckoutPrepared::None);
        }

        if has_convoy_owner {
            let conflict = self.checkouts.list().await?.items.into_iter().find(|other| {
                other.metadata.name != obj.metadata.name
                    && other.metadata.deletion_timestamp.is_none()
                    && match other.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) {
                        CheckoutPhase::Ready | CheckoutPhase::Preparing | CheckoutPhase::Terminating => true,
                        // Pending siblings reserve in creation order; names break timestamp ties.
                        // A refused loser remains terminally Failed even if the winner later fails.
                        CheckoutPhase::Pending => {
                            (other.metadata.creation_timestamp, &other.metadata.name)
                                < (obj.metadata.creation_timestamp, &obj.metadata.name)
                        }
                        CheckoutPhase::Failed | CheckoutPhase::Gone => false,
                    }
                    && other.spec.repo_ref() == obj.spec.repo_ref()
                    && other.spec.env_ref() == obj.spec.env_ref()
                    && other.spec.branch() == obj.spec.branch()
            });
            if let Some(other) = conflict {
                return Ok(CheckoutPrepared::Failed(format!(
                    "checkout branch {} conflicts with Checkout {}; choose a fresh branch name",
                    obj.spec.branch(),
                    other.metadata.name
                )));
            }
            match self.runtime.validate_new_branch(obj).await {
                Ok(Some(conflict)) => return Ok(CheckoutPrepared::Failed(conflict)),
                Ok(None) => {}
                Err(error) => {
                    if obj.status.as_ref().and_then(|status| status.message.as_deref()) == Some(error.as_str()) {
                        debug!(checkout = %obj.metadata.name, %error, "checkout branch validation still unavailable; retrying");
                    } else {
                        warn!(checkout = %obj.metadata.name, %error, "checkout branch validation unavailable; retrying");
                    }
                    return Ok(CheckoutPrepared::ValidationUnavailable(error));
                }
            }
        }

        match &obj.spec {
            CheckoutSpec::Worktree(spec) => {
                let clone = match self.clones.get(&spec.clone_ref).await {
                    Ok(clone) => clone,
                    Err(ResourceError::NotFound { .. }) => {
                        self.observe_clone_retry(obj, None).await?;
                        return Ok(CheckoutPrepared::Waiting);
                    }
                    Err(err) => return Err(err),
                };
                self.observe_clone_retry(obj, clone.status.as_ref().and_then(|status| status.retry.clone())).await?;
                if clone.status.as_ref().map(|status| status.phase) == Some(ClonePhase::Failed) {
                    if clone.status.as_ref().and_then(|status| status.failure_policy) == Some(CloneFailurePolicy::Terminal) {
                        let message = clone
                            .status
                            .as_ref()
                            .and_then(|status| status.message.clone())
                            .unwrap_or_else(|| format!("clone {} failed validation", clone.metadata.name));
                        return Ok(CheckoutPrepared::Failed(message));
                    }
                    let failed_at = clone.status.as_ref().and_then(|status| status.failed_at).unwrap_or(clone.metadata.creation_timestamp);
                    return Ok(CheckoutPrepared::RetryClone { clone_name: clone.metadata.name, failed_at });
                }
                if clone.status.as_ref().map(|status| status.phase) != Some(ClonePhase::Ready) {
                    return Ok(CheckoutPrepared::Waiting);
                }
                if clone.spec.env_ref != spec.env_ref {
                    return Ok(CheckoutPrepared::Failed("worktree clone env_ref mismatch".to_string()));
                }
                Ok(
                    match self
                        .runtime
                        .create_worktree_in(
                            &spec.env_ref,
                            &clone.spec.path,
                            &spec.r#ref,
                            spec.base_ref.as_deref(),
                            &spec.target_path,
                            &checkout_registration_reason(obj, convoy.as_ref()),
                        )
                        .await
                    {
                        Ok(prepared) => CheckoutPrepared::Ready { prepared },
                        Err(CheckoutMaterialisationError::Protection(error)) => return Err(ResourceError::other(error)),
                        Err(CheckoutMaterialisationError::Creation(error)) => CheckoutPrepared::Failed(error),
                    },
                )
            }
            CheckoutSpec::FreshClone(spec) => Ok(
                match self
                    .runtime
                    .create_fresh_clone_in(&spec.env_ref, &spec.url, &spec.r#ref, spec.base_ref.as_deref(), &spec.target_path)
                    .await
                {
                    Ok(prepared) => CheckoutPrepared::Ready { prepared },
                    Err(err) => CheckoutPrepared::Failed(err),
                },
            ),
            // Observed checkouts are facts from the observed-resource backend.
            // The managed checkout reconciler must not actuate or patch them.
            CheckoutSpec::Observed(_) => Ok(CheckoutPrepared::None),
        }
    }

    fn reconcile(
        &self,
        obj: &ResourceObject<Self::Resource>,
        prepared: &Self::Prepared,
        now: chrono::DateTime<chrono::Utc>,
    ) -> ReconcileOutcome<Self::Resource> {
        let patch = if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) == CheckoutPhase::Pending {
            match prepared {
                CheckoutPrepared::Ready { prepared } => obj.spec.target_path().map(|path| CheckoutStatusPatch::MarkReady {
                    path: path.to_string(),
                    commit: prepared.commit.clone(),
                    branch_provenance: prepared.branch_provenance,
                }),
                CheckoutPrepared::Integration { .. }
                | CheckoutPrepared::OwnerTerminal
                | CheckoutPrepared::Gone
                | CheckoutPrepared::Reappeared => None,
                CheckoutPrepared::RetryClone { .. } => None,
                CheckoutPrepared::Failed(message) => Some(CheckoutStatusPatch::MarkFailed { message: message.clone() }),
                CheckoutPrepared::ValidationUnavailable(message) => (obj.status.as_ref().and_then(|status| status.message.as_ref())
                    != Some(message))
                .then(|| CheckoutStatusPatch::ObserveValidation { message: Some(message.clone()) }),
                CheckoutPrepared::Waiting | CheckoutPrepared::None => obj
                    .status
                    .as_ref()
                    .and_then(|status| status.message.as_ref())
                    .map(|_| CheckoutStatusPatch::ObserveValidation { message: None }),
            }
        } else if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Gone) {
            match prepared {
                CheckoutPrepared::Reappeared => {
                    obj.status.as_ref().and_then(|status| status.path.clone()).or_else(|| obj.spec.target_path().map(str::to_string)).map(
                        |path| CheckoutStatusPatch::MarkReady {
                            path,
                            commit: obj.status.as_ref().and_then(|status| status.commit.clone()),
                            branch_provenance: obj.status.as_ref().map(|status| status.branch_provenance).unwrap_or_default(),
                        },
                    )
                }
                _ => None,
            }
        } else if obj.status.as_ref().is_some_and(|status| status.phase == CheckoutPhase::Ready) {
            match prepared {
                CheckoutPrepared::Gone => Some(CheckoutStatusPatch::MarkGone),
                CheckoutPrepared::Integration { status } => {
                    Some(CheckoutStatusPatch::UpdateIntegration { integration: Box::new(status.as_ref().clone()) })
                }
                CheckoutPrepared::Failed(message) => Some(CheckoutStatusPatch::UpdateIntegration {
                    integration: Box::new(CheckoutIntegrationStatus {
                        head_revision: None,
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
                        remote_refs: Default::default(),
                    }),
                }),
                CheckoutPrepared::None
                | CheckoutPrepared::OwnerTerminal
                | CheckoutPrepared::Reappeared
                | CheckoutPrepared::Ready { .. }
                | CheckoutPrepared::RetryClone { .. }
                | CheckoutPrepared::Waiting
                | CheckoutPrepared::ValidationUnavailable(_) => None,
            }
        } else {
            None
        };

        let actuations = match prepared {
            CheckoutPrepared::OwnerTerminal => vec![Actuation::DeleteCheckout { name: obj.metadata.name.clone() }],
            CheckoutPrepared::RetryClone { clone_name, failed_at } => {
                vec![Actuation::RetryClone { name: clone_name.clone(), failed_at: *failed_at }]
            }
            _ => Vec::new(),
        };
        let mut outcome = ReconcileOutcome::with_actuations(patch, actuations);
        if obj.status.as_ref().map(|status| status.phase).unwrap_or(CheckoutPhase::Pending) == CheckoutPhase::Pending
            && !matches!(prepared, CheckoutPrepared::Failed(_))
        {
            outcome.requeue_after = Some(CHECKOUT_PROVISIONING_REQUEUE_AFTER);
        }
        outcome
    }

    // Protection and environment-inspection errors are transient. Keep the
    // current lifecycle phase and retry independently of the full resync.
    fn reconcile_error_policy(&self) -> Option<ReconcileErrorPolicy> {
        Some(ReconcileErrorPolicy {
            max_consecutive_failures: 5,
            initial_backoff: CHECKOUT_PROVISIONING_REQUEUE_AFTER,
            max_backoff: Duration::from_secs(30),
            exhaustion: ReconcileErrorExhaustion::Retry,
        })
    }

    async fn run_finalizer(&self, obj: &ResourceObject<Self::Resource>) -> Result<(), ResourceError> {
        let convoy = self.owning_convoy(obj).await?;
        let forced = convoy
            .as_ref()
            .is_some_and(|convoy| convoy.metadata.annotations.get(FORCE_TEARDOWN_ANNOTATION).map(String::as_str) == Some("true"));
        let landed = convoy.as_ref().is_some_and(|convoy| convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed));
        let removal = match &obj.spec {
            CheckoutSpec::Worktree(spec) => match self.clones.get(&spec.clone_ref).await {
                Ok(clone) => {
                    let clone_path = clone.spec.path;
                    let branch = spec.r#ref.clone();
                    let target_path = spec.target_path.clone();
                    if forced {
                        CheckoutRemoval::ForcedWorktree { clone_path, branch, target_path }
                    } else if landed {
                        CheckoutRemoval::LandedWorktree { clone_path, branch, target_path }
                    } else {
                        CheckoutRemoval::Worktree { clone_path, branch, target_path }
                    }
                }
                Err(ResourceError::NotFound { .. }) => CheckoutRemoval::OrphanedWorktree { target_path: spec.target_path.clone() },
                Err(err) => return Err(err),
            },
            CheckoutSpec::FreshClone(spec) => CheckoutRemoval::FreshClone { target_path: spec.target_path.clone() },
            CheckoutSpec::Observed(_) => return Ok(()),
        };
        let outcome = if matches!(removal, CheckoutRemoval::ForcedWorktree { .. } | CheckoutRemoval::LandedWorktree { .. }) {
            // A deleted Checkout can disappear before its completed task is collected.
            // Reclaim those slots without discarding results for live resources.
            let finished = {
                let finalizers = self.finalizers.lock().await;
                finalizers.iter().filter(|(_, handle)| handle.is_finished()).map(|(name, _)| name.clone()).collect::<Vec<_>>()
            };
            for finished_name in finished {
                if matches!(self.checkouts.get(&finished_name).await, Err(ResourceError::NotFound { .. })) {
                    self.finalizers.lock().await.remove(&finished_name);
                }
            }
            let mut finalizers = self.finalizers.lock().await;
            let name = obj.metadata.name.clone();
            match finalizers.get(&name) {
                Some(handle) if handle.is_finished() => {
                    let handle = finalizers.remove(&name).expect("finished finalizer present");
                    drop(finalizers);
                    handle
                        .await
                        .map_err(|error| ResourceError::other(format!("checkout finalizer task failed: {error}")))?
                        .map_err(ResourceError::other)?
                }
                Some(_) => return Err(ResourceError::FinalizerPending),
                None => {
                    if finalizers.len() >= self.background_removal_limit.get() {
                        return Err(ResourceError::FinalizerPending);
                    }
                    let runtime = Arc::clone(&self.runtime);
                    let env_ref = obj.spec.env_ref().unwrap_or("").to_string();
                    finalizers.insert(name, tokio::spawn(async move { runtime.remove_checkout_in(&env_ref, &removal).await }));
                    return Err(ResourceError::FinalizerPending);
                }
            }
        } else {
            self.runtime.remove_checkout_in(obj.spec.env_ref().unwrap_or(""), &removal).await.map_err(ResourceError::other)?
        };
        if let CheckoutRemovalOutcome::ArchivedAndRemoved { archive_path } = &outcome {
            if let Err(error) = EventRecorder::new(self.backend.clone())
                .record(
                    ObjectEvent::for_object(obj, "CheckoutArchived", format!("checkout archive saved at {archive_path}")),
                    self.clock.now(),
                )
                .await
            {
                warn!(%error, %archive_path, "could not record checkout archive location event");
            }
        }
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
