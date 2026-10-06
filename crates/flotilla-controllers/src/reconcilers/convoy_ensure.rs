//! Standing-convoy reconciliation and its complete transaction guard.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use flotilla_core::{
    convoy_ensure::{ConvoyEnsureAdmission, ConvoyEnsureReconciler, StandingConvoyBackingInspector},
    host_resolution::canonical_placement_host_ref_from_sources,
    in_process::PreparedConvoyAdmission,
    ops_entry::{
        materialized_workflow_name, ENSURED_FROM_ANNOTATION, ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, ENSURE_DRIFT_ATTENTION_PREFIX,
        ENSURE_PROVENANCE_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION, PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION,
        SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
    },
};
use flotilla_protocol::{PlacementTargetHost, PrincipalRef, ResourceRef};
use flotilla_resources::{
    api_version, apply_status_patch as apply_resource_status_patch, Clock, ConditionValue, ControllerRetry, Convoy as ResourceConvoy,
    ConvoyEnsure, ConvoyEnsureCondition, ConvoyEnsureConfigDrift, ConvoyEnsureHoldReason, ConvoyEnsureSpec, ConvoyEnsureStatus,
    ConvoyEnsureStatusPatch, ConvoyPhase, Demand as ResourceDemand, DemandExpiry, DemandExpiryDisposition, DemandKind, DemandSpec,
    DemandState, EventRecorder, Host as ResourceHost, InputMeta, ObjectEvent, PlacementPolicy, Project, ReadResourceObject, Repository,
    Resource, ResourceBackend, ResourceError, ResourceObject, ResourceProvenance, RetryBackoff, WorkflowTemplate,
    DRIVER_ADMISSION_CONDITION_TYPE,
};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::debug;
const ENSURE_BACKOFF_RESET_AFTER: ChronoDuration = ChronoDuration::minutes(10);
const ENSURE_MAX_CONSECUTIVE_FAILURES: u32 = 3;
const ENSURE_ESCALATION_AFTER: ChronoDuration = ChronoDuration::minutes(15);
const ENSURE_HOLD_ATTENTION_PREFIX: &str = "ensure-attention-";
const RECLAIM_REFUSAL_REASON_ANNOTATION: &str = "flotilla.work/reclaim-refusal-reason";
#[derive(Debug, Clone)]
struct EnsureAdmissionRetry {
    config_hash: String,
    dependency_hash: String,
    retry: ControllerRetry,
}

fn ensure_retry_delay(restart_count: u32) -> ChronoDuration {
    ChronoDuration::from_std(
        RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(15 * 60) }.delay(restart_count.saturating_add(1)),
    )
    .expect("ensure retry delay fits chrono")
}

fn record_ensure_admission_retry(
    retries: &mut HashMap<(String, String), EnsureAdmissionRetry>,
    key: (String, String),
    config_hash: String,
    dependency_hash: String,
    now: DateTime<Utc>,
    persisted: Option<&ControllerRetry>,
) -> (u32, DateTime<Utc>) {
    let previous = retries.get(&key).map(|entry| &entry.retry).or(persisted);
    let retry =
        ControllerRetry::retryable(previous, now, RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) });
    let result = (retry.attempts, retry.next_attempt_at().expect("new admission retry is retryable"));
    retries.insert(key, EnsureAdmissionRetry { config_hash, dependency_hash, retry });
    result
}

#[derive(Clone, Copy)]
enum EnsureConvoyScope {
    Local,
    IncludingReplicas,
}

fn ensure_config_hash(spec: &ConvoyEnsureSpec) -> Result<String, String> {
    let encoded = serde_json::to_vec(spec).map_err(|error| format!("serialize ensure config: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Owns standing-convoy retries and serializes periodic, explicit and roll passes.
#[derive(bon::Builder)]
pub struct EnsureReconciler {
    resource_backend: ResourceBackend,
    clock: Arc<dyn Clock>,
    #[builder(skip)]
    ensure_admission_retries: Mutex<HashMap<(String, String), EnsureAdmissionRetry>>,
    /// Keep periodic and explicit ensure passes in one transaction, including
    /// status reads, backing inspection, admission, and status publication.
    #[builder(skip)]
    ensure_reconciliation: Mutex<()>,
}

impl EnsureReconciler {
    fn pass<'a>(&'a self, admission: &'a dyn ConvoyEnsureAdmission) -> EnsurePass<'a> {
        EnsurePass::builder()
            .resource_backend(&self.resource_backend)
            .clock(&self.clock)
            .ensure_admission_retries(&self.ensure_admission_retries)
            .ensure_reconciliation(&self.ensure_reconciliation)
            .admission(admission)
            .build()
    }
}
impl EnsureReconciler {
    #[cfg(test)]
    pub(crate) async fn ensure_admission_dependency_hash(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String> {
        self.pass(admission).ensure_admission_dependency_hash(namespace, ensure).await
    }
    #[cfg(test)]
    pub(crate) async fn start_ensured_convoy(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String> {
        self.pass(admission).start_ensured_convoy(namespace, ensure).await
    }
}
#[derive(bon::Builder)]
struct EnsurePass<'a> {
    resource_backend: &'a ResourceBackend,
    clock: &'a Arc<dyn Clock>,
    ensure_admission_retries: &'a Mutex<HashMap<(String, String), EnsureAdmissionRetry>>,
    ensure_reconciliation: &'a Mutex<()>,
    admission: &'a dyn ConvoyEnsureAdmission,
}

#[async_trait]
impl ConvoyEnsureReconciler for EnsureReconciler {
    async fn active_ensured_convoys(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<ResourceConvoy>>, String> {
        self.pass(admission).active_ensured_convoys(namespace, name, EnsureConvoyScope::IncludingReplicas).await
    }
    async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        self.pass(admission).reconcile_convoy_ensures_once_with_backing_inspector(namespace, backing_inspector).await
    }
    async fn reconcile_convoy_ensure_now(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        name: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<String, String> {
        self.pass(admission).reconcile_convoy_ensure_now(namespace, name, backing_inspector).await
    }
    async fn roll_convoy_ensure(&self, admission: &dyn ConvoyEnsureAdmission, namespace: &str, name: &str) -> Result<String, String> {
        self.pass(admission).roll_convoy_ensure(namespace, name).await
    }
    async fn reap_ensured_convoy(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        ensure_name: &str,
        convoy_name: &str,
        force: bool,
    ) -> Result<(), String> {
        self.pass(admission).reap_ensured_convoy(namespace, ensure_name, convoy_name, force).await
    }
}

impl EnsurePass<'_> {
    pub async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).list().await.map_err(|e| e.to_string())?;
        let ensure_names = ensures.iter().map(|ensure| ensure.metadata.name.clone()).collect::<HashSet<_>>();
        self.ensure_admission_retries
            .lock()
            .await
            .retain(|(retry_namespace, retry_name), _| retry_namespace != namespace || ensure_names.contains(retry_name));
        let local_projects = self.resource_backend.clone().using::<Project>(namespace);
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        for ensure in ensures {
            if let Some(driver_ref) = &ensure.spec.driver_ref {
                if let Some(convoy) = self
                    .active_ensured_convoys(namespace, &ensure.metadata.name, EnsureConvoyScope::IncludingReplicas)
                    .await?
                    .into_iter()
                    .map(|source| source.object)
                    .max_by_key(|convoy| convoy.spec.generation)
                {
                    self.observe_ensure_config_drift(namespace, &ensure, &convoy).await?;
                }
                if self.resource_backend.clone().using::<ConvoyEnsure>(namespace).get(&ensure.metadata.name).await.is_ok()
                    && ensure.status.as_ref().is_some_and(|status| status.convoy_ref.is_some() || status.running_since.is_some())
                {
                    if let Err(error) =
                        self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverManaged).await
                    {
                        errors.push(format!(
                            "ConvoyEnsure/{}: could not clear legacy status for driver ownership: {error}",
                            ensure.metadata.name
                        ));
                    }
                }
                let target = match canonical_placement_host_ref(self.resource_backend, namespace, driver_ref).await {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        if let Err(error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "UnknownDriver",
                                format!("driver host `{driver_ref}` is unknown"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {error}", ensure.metadata.name));
                        }
                        continue;
                    }
                    Err(error) => {
                        if let Err(patch_error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "DriverUnreachable",
                                format!("driver host `{driver_ref}` could not be resolved: {error}"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {patch_error}", ensure.metadata.name));
                        }
                        continue;
                    }
                };
                if self.admission.local_host_id().as_ref() != Some(&target.reference) {
                    let hosts = match self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await {
                        Ok(hosts) => hosts,
                        Err(error) => {
                            errors.push(format!(
                                "ConvoyEnsure/{}: could not inspect driver host `{driver_ref}` reachability: {error}",
                                ensure.metadata.name
                            ));
                            continue;
                        }
                    };
                    let reachable = hosts
                        .items
                        .iter()
                        .find(|host| host.object.metadata.name == target.reference.as_str())
                        .is_some_and(|host| host.object.status.as_ref().is_some_and(|status| status.ready));
                    if reachable {
                        if let Err(error) = self.clear_ensure_driver_condition(namespace, &ensure).await {
                            errors.push(format!("ConvoyEnsure/{}: could not clear driver condition: {error}", ensure.metadata.name));
                        }
                    } else {
                        if let Err(error) = self
                            .set_ensure_driver_condition(
                                namespace,
                                &ensure,
                                "DriverUnreachable",
                                format!("driver host `{driver_ref}` is not reachable"),
                            )
                            .await
                        {
                            errors.push(format!("ConvoyEnsure/{}: could not record driver condition: {error}", ensure.metadata.name));
                        }
                    }
                    continue;
                }
                match self.reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector, false).await {
                    Ok(Some(change)) => changes.push(change),
                    Ok(None) => {}
                    Err(error) => errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name)),
                }
                continue;
            } else {
                match local_projects.get(&ensure.spec.project_ref).await {
                    Ok(project) if project.metadata.deletion_timestamp.is_none() => {}
                    Ok(_) | Err(ResourceError::NotFound { .. }) => {
                        match self.resource_backend.clone().definitions::<Project>(namespace).get(&ensure.spec.project_ref).await {
                            Ok(_) => {
                                debug!(
                                    ensure = %ensure.metadata.name,
                                    project = %ensure.spec.project_ref,
                                    "skipping standing convoy ensure away from its project home"
                                );
                                continue;
                            }
                            Err(ResourceError::NotFound { .. }) => {
                                errors.push(format!(
                                    "ConvoyEnsure/{}: parent Project/{} is absent",
                                    ensure.metadata.name, ensure.spec.project_ref
                                ));
                                continue;
                            }
                            Err(error) => {
                                errors.push(format!(
                                    "ConvoyEnsure/{}: could not resolve Project/{} authority: {error}",
                                    ensure.metadata.name, ensure.spec.project_ref
                                ));
                                continue;
                            }
                        }
                    }
                    Err(error) => {
                        errors.push(format!(
                            "ConvoyEnsure/{}: could not verify local Project/{} authority: {error}",
                            ensure.metadata.name, ensure.spec.project_ref
                        ));
                        continue;
                    }
                }
            }
            match self.reconcile_convoy_ensure(namespace, &ensure, backing_inspector, false).await {
                Ok(Some(change)) => changes.push(change),
                Ok(None) => {}
                Err(error) => errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name)),
            }
        }
        if errors.is_empty() {
            Ok(changes)
        } else if changes.is_empty() {
            Err(errors.join("; "))
        } else {
            Err(format!("{}; successful changes: {}", errors.join("; "), changes.join(", ")))
        }
    }

    /// Reset one ensure's retry budget and drive its admission synchronously.
    pub async fn reconcile_convoy_ensure_now(
        &self,
        namespace: &str,
        name: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<String, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace);
        let ensure = ensures.get(name).await.map_err(|error| error.to_string())?;
        if ensure.spec.driver_ref.is_some() {
            self.ensure_admission_retries.lock().await.remove(&(namespace.to_string(), name.to_string()));
            return self
                .reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector, true)
                .await
                .map(|change| change.unwrap_or_else(|| format!("ConvoyEnsure/{name} is already reconciled")));
        }
        self.reconcile_convoy_ensure(namespace, &ensure, backing_inspector, true)
            .await
            .map(|change| change.unwrap_or_else(|| format!("ConvoyEnsure/{name} is already reconciled")))
    }

    async fn active_ensured_convoys(
        &self,
        namespace: &str,
        name: &str,
        scope: EnsureConvoyScope,
    ) -> Result<Vec<ReadResourceObject<ResourceConvoy>>, String> {
        // Deliberately query current state for each ensure rather than caching
        // across admissions in the pass. This costs O(ensures * convoys); revisit
        // with a pass-local index if fleet size warrants it.
        let sources = match scope {
            EnsureConvoyScope::Local => self
                .resource_backend
                .using::<ResourceConvoy>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect(),
            EnsureConvoyScope::IncludingReplicas => {
                self.resource_backend.including_replicas::<ResourceConvoy>(namespace).list().await.map_err(|error| error.to_string())?.items
            }
        };
        Ok(sources
            .into_iter()
            .filter(|source| {
                source.object.metadata.annotations.get(ENSURED_FROM_ANNOTATION).map(String::as_str) == Some(name)
                    && source.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
            })
            .collect())
    }

    /// Explicitly retire a drifted running generation and admit its successor.
    /// Admission is prepared before abandonment so invalid declarations cannot
    /// interrupt the current crew. The ordinary lifecycle reclaims old backing.
    pub async fn roll_convoy_ensure(&self, namespace: &str, name: &str) -> Result<String, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensure =
            self.resource_backend.including_replicas::<ConvoyEnsure>(namespace).get(name).await.map_err(|error| error.to_string())?.object;
        if ensure.status.as_ref().is_some_and(|status| status.declaration_refused.is_some()) {
            return Err(format!("ConvoyEnsure/{name} has a refused declaration; refresh successfully before rolling"));
        }
        let convoy = self
            .active_ensured_convoys(namespace, name, EnsureConvoyScope::Local)
            .await?
            .into_iter()
            .map(|source| source.object)
            .max_by_key(|convoy| convoy.spec.generation)
            .ok_or_else(|| format!("ConvoyEnsure/{name} has no running convoy on this host"))?;
        self.observe_ensure_config_drift(namespace, &ensure, &convoy).await?;
        if convoy.status.as_ref().and_then(|status| status.ensure_admission.as_ref()) == Some(&ensure.spec) {
            return Ok(format!("ConvoyEnsure/{name} has no configuration drift"));
        }
        let (ensure, admission) = self.admission.prepare(namespace, &ensure).await?;
        if convoy.status.as_ref().and_then(|status| status.ensure_admission.as_ref()) == Some(&ensure.spec) {
            return Ok(format!("ConvoyEnsure/{name} has no configuration drift"));
        }
        let driver_target = match &ensure.spec.driver_ref {
            Some(driver) => Some(
                canonical_placement_host_ref(self.resource_backend, namespace, driver)
                    .await?
                    .ok_or_else(|| format!("unknown driver `{driver}`"))?,
            ),
            None => None,
        };
        self.admission
            .abandon(
                namespace,
                &convoy.metadata.name,
                "operator rolls changed ConvoyEnsure configuration",
                Some(&PrincipalRef::implicit_for_namespace(namespace)),
            )
            .await?;
        // Retirement and admission are separate durable operations, not an
        // atomic transaction. A crash or write failure below leaves a gap: the
        // ordinary ensure reconciliation loop retries admission (with backing
        // verification/backoff). A driver move is recovered by the new driver.
        self.patch_driver_ensure_status_if_local(namespace, name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
        if let Some(target) = driver_target {
            if self.admission.local_host_id().as_ref() != Some(&target.reference) {
                return Ok(format!("ConvoyEnsure/{name} retired its old generation; admission awaits driver {}", target.reference));
            }
        }
        let replacement = self.commit_ensured_convoy(namespace, &ensure, admission).await?;
        if ensure.spec.driver_ref.is_none() {
            self.patch_convoy_ensure(namespace, name, ConvoyEnsureStatusPatch::Running {
                convoy_ref: replacement.clone(),
                observed_at: self.clock.now(),
            })
            .await?;
        }
        Ok(format!("ConvoyEnsure/{name} rolled to {replacement}"))
    }

    /// Fingerprint the named resources consulted by standing-convoy admission.
    /// A changed fingerprint invalidates a read-only refusal's deadline so the
    /// next reconciliation pass can retry immediately.
    async fn ensure_admission_dependency_hash(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let mut versions = BTreeMap::new();
        let projects = self.resource_backend.clone().including_replicas::<Project>(namespace);
        let project = match projects.get(&ensure.spec.project_ref).await {
            Ok(project) => {
                versions.insert(format!("Project/{}", ensure.spec.project_ref), project.object.metadata.resource_version.clone());
                Some(project.object)
            }
            Err(error) => {
                versions.insert(format!("Project/{}", ensure.spec.project_ref), format!("absent:{error}"));
                None
            }
        };
        if let Some(project) = project {
            let repositories = self.resource_backend.clone().including_replicas::<Repository>(namespace);
            let repository_sources = repositories.list_replica_sources().await.map_err(|error| error.to_string())?;
            for repository in &project.spec.repositories {
                let name = repository.repo.to_string();
                let mut found = false;
                for source in repository_sources.items.iter().filter(|source| source.object.metadata.name == name) {
                    let provenance = match &source.provenance {
                        ResourceProvenance::Local => "local".to_string(),
                        ResourceProvenance::Replica { origin_root, .. } => format!("replica:{origin_root}"),
                    };
                    versions.insert(format!("Repository/{name}/{provenance}"), source.object.metadata.resource_version.clone());
                    found = true;
                }
                if !found {
                    versions.insert(format!("Repository/{name}"), "absent".to_string());
                }
            }
        }
        let projects = self.resource_backend.definitions::<Project>(namespace);
        let cascade = match projects.get(&ensure.spec.project_ref).await {
            Ok(project) => {
                match flotilla_resources::ResolvedCascade::load(self.resource_backend, namespace, &ensure.spec.project_ref, &project.spec)
                    .await
                {
                    Ok(cascade) => {
                        versions.insert("RoleCascade".into(), serde_json::to_string(&cascade).map_err(|error| error.to_string())?);
                        Some(cascade)
                    }
                    Err(error) => {
                        versions.insert("RoleCascade".into(), format!("refused:{error}"));
                        None
                    }
                }
            }
            Err(_) => None,
        };
        let workflow_ref = if ensure.spec.workflow_ref.is_empty() {
            cascade.as_ref().map(|cascade| cascade.workflow(Some(&ensure.spec.role)).value.as_str()).unwrap_or("")
        } else {
            &ensure.spec.workflow_ref
        };
        let workflows = self.resource_backend.clone().definitions::<WorkflowTemplate>(namespace);
        let mut workflow_names =
            BTreeSet::from([materialized_workflow_name(&ensure.spec.project_ref, workflow_ref), workflow_ref.to_string()]);
        if let Some(cascade) = &cascade {
            workflow_names.extend(cascade.project_chain.iter().map(|owner| materialized_workflow_name(owner, workflow_ref)));
        }
        for name in workflow_names {
            let version = workflows
                .get(&name)
                .await
                .map(|workflow| workflow.metadata.resource_version)
                .unwrap_or_else(|error| format!("absent:{error}"));
            versions.insert(format!("WorkflowTemplate/{name}"), version);
        }
        if let Some(name) = &ensure.spec.placement_policy {
            let policies = self.resource_backend.including_replicas::<PlacementPolicy>(namespace);
            let version = policies
                .get(name)
                .await
                .map(|policy| policy.object.metadata.resource_version)
                .unwrap_or_else(|error| format!("absent:{error}"));
            versions.insert(format!("PlacementPolicy/{name}"), version);
        }
        let encoded = serde_json::to_vec(&versions).map_err(|error| format!("serialize ensure admission dependencies: {error}"))?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }

    /// Admit a declared-driver ensure from the generation history homed here.
    ///
    /// Driver admission keeps admission retry state on the driver because the
    /// ensure definition may be homed on another root. Failed generations
    /// remain the runtime strike budget; read-only admission refusals retry
    /// indefinitely on their own short, dependency-invalidated backoff.
    async fn reconcile_driver_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
        force_now: bool,
    ) -> Result<Option<String>, String> {
        let active = self
            .active_ensured_convoys(namespace, &ensure.metadata.name, EnsureConvoyScope::IncludingReplicas)
            .await?
            .into_iter()
            .map(|source| source.object)
            .max_by_key(|convoy| convoy.spec.generation);
        if let Some(convoy) = active {
            self.observe_ensure_config_drift(namespace, ensure, &convoy).await?;
            return Ok(None);
        }

        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut generations = convoys
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name))
            .collect::<Vec<_>>();
        generations.sort_by_key(|convoy| convoy.spec.generation);

        let consecutive_failures = generations
            .iter()
            .rev()
            .take_while(|convoy| convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Failed))
            .count() as u32;

        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        let demand_name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{}", ensure.metadata.name);
        let resolved_escalation = match demands.get(&demand_name).await {
            Ok(demand)
                if demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated)) =>
            {
                if consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES && !force_now {
                    return Ok(None);
                }
                demands.delete(&demand_name).await.map_err(|error| error.to_string())?;
                consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES
            }
            Ok(_) => {
                demands.delete(&demand_name).await.map_err(|error| error.to_string())?;
                true
            }
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error.to_string()),
        };

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let config_hash = ensure_config_hash(&ensure.spec)?;
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        {
            let mut retries = self.ensure_admission_retries.lock().await;
            if retries.get(&retry_key).is_some_and(|retry| retry.config_hash != config_hash || retry.dependency_hash != dependency_hash) {
                retries.remove(&retry_key);
            }
            if !resolved_escalation && !force_now {
                if let Some(retry) = retries.get(&retry_key) {
                    if retry.retry.next_attempt_at().is_some_and(|retry_at| retry_at > self.clock.now()) {
                        return Ok(None);
                    }
                }
            } else {
                retries.remove(&retry_key);
            }
        }

        let latest = generations.last();
        if !resolved_escalation && !force_now && consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES {
            let latest = latest.expect("a positive failure count requires a generation");
            let failure = format!("ensured convoy entered a terminal failure phase; {consecutive_failures} consecutive generations failed");
            self.raise_ensure_attention(ensure, latest, &failure, Some(self.clock.now() + ENSURE_ESCALATION_AFTER)).await?;
            return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
        }
        // `reconcile-now` is the operator's explicit acknowledgement that any
        // missing backing records were deliberately wiped. Automatic passes
        // still require positive death evidence before admitting a successor.
        if !resolved_escalation && !force_now && consecutive_failures > 0 {
            let latest = latest.expect("a positive failure count requires a generation");
            backing_inspector.verify_backing_dead(latest).await?;
            let retry_at = latest.metadata.creation_timestamp + ensure_retry_delay(consecutive_failures - 1);
            if !force_now && retry_at > self.clock.now() {
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackoffState {
                    strikes: consecutive_failures,
                    retry_at,
                    failure: "ensured convoy entered a terminal failure phase".to_string(),
                })
                .await?;
                return Ok(None);
            }
        }

        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(_) => {
                self.ensure_admission_retries.lock().await.remove(&retry_key);
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                let now = self.clock.now();
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(
                        &mut retries,
                        retry_key,
                        config_hash,
                        dependency_hash,
                        now,
                        ensure.status.as_ref().and_then(|status| status.retry.as_ref()),
                    )
                };
                self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackoffState {
                    strikes: consecutive_failures,
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("driver admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn set_ensure_driver_condition(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        reason: &str,
        message: String,
    ) -> Result<(), String> {
        let unchanged = ensure.status.as_ref().is_some_and(|status| {
            status.conditions.iter().any(|condition| {
                condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE && condition.reason == reason && condition.message == message
            })
        });
        if unchanged {
            return Ok(());
        }
        self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverAdmission {
            condition: Some(ConvoyEnsureCondition {
                condition_type: DRIVER_ADMISSION_CONDITION_TYPE.to_string(),
                value: ConditionValue::False,
                reason: reason.to_string(),
                message,
                observed_at: self.clock.now(),
            }),
        })
        .await
    }

    async fn clear_ensure_driver_condition(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<(), String> {
        if ensure
            .status
            .as_ref()
            .is_some_and(|status| status.conditions.iter().any(|condition| condition.condition_type == DRIVER_ADMISSION_CONDITION_TYPE))
        {
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::DriverAdmission { condition: None })
                .await?;
        }
        Ok(())
    }

    async fn reconcile_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
        force_now: bool,
    ) -> Result<Option<String>, String> {
        let now = self.clock.now();
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let mut status = ensure.status.clone().unwrap_or_default();
        let config_hash = ensure_config_hash(&ensure.spec)?;
        if status.observed_config_hash.as_deref() != Some(&config_hash) {
            let changed = status.observed_config_hash.is_some();
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ObserveConfig {
                config_hash: config_hash.clone(),
                changed,
            })
            .await?;
            status.observed_config_hash = Some(config_hash.clone());
            if changed {
                self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
        }
        let convoy = match status.convoy_ref.as_deref() {
            Some(convoy_ref) => match convoys.get(convoy_ref).await {
                Ok(convoy) if convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name) => Some(convoy),
                Ok(_) => return Err(format!("convoy {convoy_ref} exists without this ensure's provenance")),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            },
            None => self
                .active_ensured_convoys(namespace, &ensure.metadata.name, EnsureConvoyScope::Local)
                .await?
                .into_iter()
                .map(|source| source.object)
                .max_by_key(|convoy| convoy.spec.generation),
        };
        let terminal = convoy
            .as_ref()
            .and_then(|convoy| convoy.status.as_ref())
            .is_some_and(|status| matches!(status.phase, ConvoyPhase::Failed | ConvoyPhase::Cancelled | ConvoyPhase::Abandoned));

        if let Some(convoy) = convoy.as_ref().filter(|_| !terminal) {
            self.observe_ensure_config_drift(namespace, ensure, convoy).await?;
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            let convoy_ref = convoy.metadata.name.clone();
            if status.convoy_ref.as_deref() != Some(&convoy_ref) || status.retry_at.is_some() || status.last_failure.is_some() {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref,
                    observed_at: now,
                })
                .await?;
                return Ok(Some(format!("ConvoyEnsure/{} observed running", ensure.metadata.name)));
            }
            if status.restart_count > 0
                && status.running_since.is_some_and(|running_since| now - running_since >= ENSURE_BACKOFF_RESET_AFTER)
            {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                return Ok(Some(format!("ConvoyEnsure/{} reset restart backoff", ensure.metadata.name)));
            }
            return Ok(None);
        }

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        let dependency_changed = {
            let mut retries = self.ensure_admission_retries.lock().await;
            let changed =
                retries.get(&retry_key).is_some_and(|retry| retry.config_hash != config_hash || retry.dependency_hash != dependency_hash);
            if changed {
                retries.remove(&retry_key);
            }
            changed
        };

        if convoy.is_none() {
            if !force_now && !dependency_changed && status.retry_at.is_some_and(|retry_at| retry_at > now) {
                return Ok(None);
            }
            if force_now {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
                status.retry = None;
            }
            return self.restart_absent_ensured_convoy(namespace, ensure, &status, now).await;
        }

        let convoy = convoy.expect("terminal branch requires an existing convoy");
        if status.hold_reason == Some(ConvoyEnsureHoldReason::RestartLimit) {
            if !force_now && self.ensure_attention_is_active(namespace, &ensure.metadata.name).await? {
                return Ok(None);
            }
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            if !force_now {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
                return Ok(Some(format!("ConvoyEnsure/{} restart hold cleared", ensure.metadata.name)));
            }
            status.restart_count = 0;
            status.retry_at = None;
            status.last_failure = None;
            status.hold_reason = None;
            status.retry = None;
        }
        let operator_forced = convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
        // A forced pass is the recovery boundary for recordless teardown: the
        // operator has acknowledged that raw deletion destroyed the evidence.
        if !operator_forced && !force_now {
            if let Err(reason) = backing_inspector.verify_backing_dead(&convoy).await {
                let failure = format!("standing convoy teardown held: {reason}");
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(&convoy, "BackingEvidenceRefused", failure.clone()), now)
                    .await
                    .map_err(|error| format!("record backing-evidence event: {error}"))?;
                self.raise_ensure_attention(ensure, &convoy, &failure, None).await?;
                if status.retry_at.is_some() || status.last_failure.as_deref() != Some(&failure) {
                    self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Holding {
                        convoy_ref: convoy.metadata.name.clone(),
                        failure,
                    })
                    .await?;
                    return Ok(Some(format!("ConvoyEnsure/{} held for operator attention", ensure.metadata.name)));
                }
                return Ok(None);
            }
        }
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;

        if status.retry_at.is_none() && !force_now {
            let failure = "ensured convoy entered a terminal failure phase";
            if status.restart_count.saturating_add(1) >= ENSURE_MAX_CONSECUTIVE_FAILURES {
                let failure = format!("{failure}; {} consecutive generations failed", ENSURE_MAX_CONSECUTIVE_FAILURES);
                self.raise_ensure_attention(ensure, &convoy, &failure, Some(now + ENSURE_ESCALATION_AFTER)).await?;
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::RestartLimitReached {
                    convoy_ref: convoy.metadata.name.clone(),
                    failure,
                })
                .await?;
                return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
            }
            let retry_at = now + ensure_retry_delay(status.restart_count);
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::BackingOff {
                retry_at,
                failure: failure.to_string(),
            })
            .await?;
            return Ok(Some(format!("ConvoyEnsure/{} backing off until {retry_at}", ensure.metadata.name)));
        }
        if !force_now && !dependency_changed && status.retry_at.is_some_and(|retry_at| retry_at > now) {
            return Ok(None);
        }
        if force_now {
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
        }

        let restart = async {
            // The terminal generation remains as history. A successful
            // restart admits the next generation under a fresh record name.
            self.start_ensured_convoy(namespace, ensure).await
        }
        .await;
        match restart {
            Ok(convoy_ref) => {
                self.ensure_admission_retries.lock().await.remove(&retry_key);
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref: convoy_ref.clone(),
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(ensure, "EnsureAdmissionRefused", error.clone()), now)
                    .await
                    .map_err(|record_error| format!("record ensure admission event: {record_error}"))?;
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(&mut retries, retry_key, config_hash, dependency_hash, now, status.retry.as_ref())
                };
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn restart_absent_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        status: &ConvoyEnsureStatus,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, String> {
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(convoy_ref) => {
                self.ensure_admission_retries.lock().await.remove(&(namespace.to_string(), ensure.metadata.name.clone()));
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref,
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                EventRecorder::new(self.resource_backend.clone())
                    .record(ObjectEvent::for_object(ensure, "EnsureAdmissionRefused", error.clone()), now)
                    .await
                    .map_err(|record_error| format!("record ensure admission event: {record_error}"))?;
                let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
                let config_hash = ensure_config_hash(&ensure.spec)?;
                let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
                let (refusals, retry_at) = {
                    let mut retries = self.ensure_admission_retries.lock().await;
                    record_ensure_admission_retry(&mut retries, retry_key, config_hash, dependency_hash, now, status.retry.as_ref())
                };
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
            }
        }
    }

    async fn observe_ensure_config_drift(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &ResourceObject<ResourceConvoy>,
    ) -> Result<(), String> {
        let admitted = convoy.status.as_ref().and_then(|status| status.ensure_admission.clone());
        let observed_hash = ensure_config_hash(&ensure.spec)?;
        let admitted_hash = admitted.as_ref().map(ensure_config_hash).transpose()?;
        let mut changes = Vec::new();
        if let Some(old) = &admitted {
            for repo in ensure.spec.repositories.iter().filter(|repo| !old.repositories.contains(repo)) {
                changes.push(format!("repository added: {repo}"));
            }
            for repo in old.repositories.iter().filter(|repo| !ensure.spec.repositories.contains(repo)) {
                changes.push(format!("repository removed: {repo}"));
            }
            if old.repositories != ensure.spec.repositories && changes.is_empty() {
                changes.push("repository order changed".into());
            }
            if old.workflow_ref != ensure.spec.workflow_ref {
                changes.push(format!("workflow: {} -> {}", old.workflow_ref, ensure.spec.workflow_ref));
            }
            if old.placement_policy != ensure.spec.placement_policy {
                changes.push(format!("placement: {:?} -> {:?}", old.placement_policy, ensure.spec.placement_policy));
            }
            if old.agent_overrides != ensure.spec.agent_overrides {
                changes.push(format!("agents: {:?} -> {:?}", old.agent_overrides, ensure.spec.agent_overrides));
            }
            if old.driver_ref != ensure.spec.driver_ref {
                changes.push(format!("driver: {:?} -> {:?}", old.driver_ref, ensure.spec.driver_ref));
            }
            if old.presents_as != ensure.spec.presents_as {
                changes.push(format!("presentation: {:?} -> {:?}", old.presents_as, ensure.spec.presents_as));
            }
            if old.escalation_reason != ensure.spec.escalation_reason {
                changes.push("placement escalation reason changed".into());
            }
            if old.project_ref != ensure.spec.project_ref {
                changes.push(format!("project: {} -> {}", old.project_ref, ensure.spec.project_ref));
            }
            if old.role != ensure.spec.role {
                changes.push(format!("role: {} -> {}", old.role, ensure.spec.role));
            }
        } else {
            // Never infer a previous generation's admission baseline from the
            // latest observed declaration: it may already have silently drifted.
            changes.push("admission configuration unknown; explicitly roll to establish a baseline".into());
        }
        let drift = (!changes.is_empty()).then(|| ConvoyEnsureConfigDrift { changes: changes.clone(), observed_at: self.clock.now() });
        let unchanged = ensure.status.as_ref().is_some_and(|status| {
            status.admitted_config_hash == admitted_hash
                && status.observed_config_hash.as_ref() == Some(&observed_hash)
                && status.config_drift.as_ref().map(|old| &old.changes) == drift.as_ref().map(|new| &new.changes)
        });
        if !unchanged {
            let patch = ConvoyEnsureStatusPatch::ConfigDrift { admitted_hash, observed_hash, drift };
            self.patch_driver_ensure_status_if_local(namespace, &ensure.metadata.name, patch).await?;
        }
        // Only the convoy's authority host raises runtime attention. The
        // definition home observes the replica and records its typed status.
        match self.resource_backend.using::<ResourceConvoy>(namespace).get(&convoy.metadata.name).await {
            Ok(_) => {}
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        let demands = self.resource_backend.using::<ResourceDemand>(namespace);
        let name = format!("{ENSURE_DRIFT_ATTENTION_PREFIX}{}", ensure.metadata.name);
        if changes.is_empty() {
            match demands.delete(&name).await {
                Ok(()) | Err(ResourceError::NotFound { .. }) => return Ok(()),
                Err(error) => return Err(error.to_string()),
            }
        }
        let target =
            ResourceRef::new(api_version(ResourceConvoy::API_PATHS), ResourceConvoy::API_PATHS.kind, namespace, &convoy.metadata.name);
        let meta = InputMeta::builder()
            .name(name)
            .annotations(BTreeMap::from([(
                ENSURE_CONFIG_DRIFT_REASON_ANNOTATION.to_string(),
                format!("ConfigDrift: {}; run flotilla ensure roll {}", changes.join("; "), ensure.metadata.name),
            )]))
            .build();
        let spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
        match demands.create(&meta, &spec).await {
            Ok(_) => Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                let current = demands.get(&meta.name).await.map_err(|error| error.to_string())?;
                if current.metadata.annotations == meta.annotations && current.spec == spec {
                    return Ok(());
                }
                demands.update(&meta, &current.metadata.resource_version, &spec).await.map(|_| ()).map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn raise_ensure_attention(
        &self,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &ResourceObject<ResourceConvoy>,
        reason: &str,
        escalation_deadline: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let demands = self.resource_backend.clone().using::<ResourceDemand>(&convoy.metadata.namespace);
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{}", ensure.metadata.name);
        let target = ResourceRef::new(
            api_version(ResourceConvoy::API_PATHS),
            ResourceConvoy::API_PATHS.kind,
            &convoy.metadata.namespace,
            &convoy.metadata.name,
        );
        let meta = InputMeta::builder()
            .name(name)
            .annotations(BTreeMap::from([(RECLAIM_REFUSAL_REASON_ANNOTATION.to_string(), reason.to_string())]))
            .build();
        let mut spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
        spec.expiry = escalation_deadline.map(|deadline| DemandExpiry { deadline, disposition: DemandExpiryDisposition::Escalate });
        match demands.create(&meta, &spec).await {
            Ok(_) => Ok(()),
            Err(ResourceError::Conflict { .. }) => {
                let current = demands.get(&meta.name).await.map_err(|error| error.to_string())?;
                demands.update(&meta, &current.metadata.resource_version, &spec).await.map(|_| ()).map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn ensure_attention_is_active(&self, namespace: &str, ensure_name: &str) -> Result<bool, String> {
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{ensure_name}");
        match self.resource_backend.clone().using::<ResourceDemand>(namespace).get(&name).await {
            Ok(demand) => {
                Ok(demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated)))
            }
            Err(ResourceError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn clear_ensure_attention(&self, namespace: &str, ensure_name: &str) -> Result<(), String> {
        let name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{ensure_name}");
        match self.resource_backend.clone().using::<ResourceDemand>(namespace).delete(&name).await {
            Ok(()) | Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn patch_convoy_ensure(&self, namespace: &str, name: &str, patch: ConvoyEnsureStatusPatch) -> Result<(), String> {
        apply_resource_status_patch(&self.resource_backend.clone().using::<ConvoyEnsure>(namespace), name, &patch)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn patch_driver_ensure_status_if_local(&self, namespace: &str, name: &str, patch: ConvoyEnsureStatusPatch) -> Result<(), String> {
        match self.resource_backend.using::<ConvoyEnsure>(namespace).get(name).await {
            Ok(_) => self.patch_convoy_ensure(namespace, name, patch).await,
            Err(ResourceError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn start_ensured_convoy(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let (ensure, admission) = self.admission.prepare(namespace, ensure).await?;
        self.commit_ensured_convoy(namespace, &ensure, admission).await
    }

    async fn commit_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        admission: PreparedConvoyAdmission,
    ) -> Result<String, String> {
        let commit = ensure
            .metadata
            .annotations
            .get(SOURCE_COMMIT_ANNOTATION)
            .cloned()
            .ok_or_else(|| "materialized ensure has no source commit provenance".to_string())?;
        let provenance = format!("ensured from {} @ {commit}", ensure.metadata.name);
        let mut annotations = BTreeMap::from([
            (ENSURED_FROM_ANNOTATION.to_string(), ensure.metadata.name.clone()),
            (ENSURE_PROVENANCE_ANNOTATION.to_string(), provenance),
        ]);
        for key in [MATERIALIZED_PROJECT_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION] {
            if let Some(value) = ensure.metadata.annotations.get(key) {
                annotations.insert(key.to_string(), value.clone());
            }
        }
        if let Some(presents_as) = &ensure.spec.presents_as {
            annotations.insert(PRESENTS_AS_ANNOTATION.to_string(), presents_as.clone());
        }
        let name = self.admission.commit(namespace, ensure, admission, annotations).await?;
        let convoy = self.resource_backend.using::<ResourceConvoy>(namespace).get(&name).await.map_err(|error| error.to_string())?;
        self.observe_ensure_config_drift(namespace, ensure, &convoy).await?;
        Ok(name)
    }

    async fn reap_ensured_convoy(&self, namespace: &str, ensure_name: &str, convoy_name: &str, force: bool) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = match convoys.get(convoy_name).await {
            Ok(convoy) => convoy,
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        if convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).map(String::as_str) != Some(ensure_name) {
            return Err(format!("refusing to reap standing convoy: it is not owned by ConvoyEnsure/{ensure_name}"));
        }
        self.admission.reap(namespace, convoy_name, force).await
    }
}
async fn canonical_placement_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let hosts = backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
    canonical_placement_host_ref_from_sources(&hosts.items, host_ref)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use chrono::TimeZone;
    use flotilla_protocol::CanonicalHostId;
    use flotilla_resources::{ConvoySpec, ConvoyStatus, InMemoryBackend, ProjectSpec, VirtualClock, WorkflowTemplateSpec};

    use super::*;

    // Stands in for provider-backed admission and lifecycle operations. Resource
    // storage, status patches, the clock and the reconcile loop are real.
    #[derive(bon::Builder)]
    struct Admission {
        backend: ResourceBackend,
        #[builder(skip)]
        refused: AtomicBool,
        #[builder(skip)]
        prepares: AtomicU32,
        #[builder(skip)]
        commits: AtomicU32,
        #[builder(skip)]
        reaps: AtomicU32,
    }

    #[async_trait]
    impl ConvoyEnsureAdmission for Admission {
        fn local_host_id(&self) -> Option<CanonicalHostId> {
            None
        }
        async fn prepare(
            &self,
            _: &str,
            ensure: &ResourceObject<ConvoyEnsure>,
        ) -> Result<(ResourceObject<ConvoyEnsure>, PreparedConvoyAdmission), String> {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            if self.refused.load(Ordering::SeqCst) {
                return Err("admission unavailable".into());
            }
            Ok((
                ensure.clone(),
                PreparedConvoyAdmission::builder()
                    .name("unused".into())
                    .spec(ConvoySpec::builder().workflow_ref("standing".into()).build())
                    .workflow(WorkflowTemplateSpec::builder().build())
                    .build(),
            ))
        }
        async fn commit(
            &self,
            namespace: &str,
            ensure: &ResourceObject<ConvoyEnsure>,
            _: PreparedConvoyAdmission,
            annotations: BTreeMap<String, String>,
        ) -> Result<String, String> {
            let generation = self.commits.fetch_add(1, Ordering::SeqCst) + 1;
            let name = format!("generation-{generation}");
            let convoys = self.backend.using::<ResourceConvoy>(namespace);
            let convoy = convoys
                .create(
                    &InputMeta::builder().name(name.clone()).annotations(annotations).build(),
                    &ConvoySpec::builder()
                        .workflow_ref("standing".into())
                        .project_ref(ensure.spec.project_ref.clone())
                        .role(ensure.spec.role.clone())
                        .generation(u64::from(generation))
                        .build(),
                )
                .await
                .map_err(|e| e.to_string())?;
            convoys
                .update_status(&name, &convoy.metadata.resource_version, &ConvoyStatus {
                    ensure_admission: Some(ensure.spec.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())?;
            Ok(name)
        }
        async fn abandon(&self, _: &str, _: &str, _: &str, _: Option<&PrincipalRef>) -> Result<(), String> {
            unreachable!("not rolling in these contracts")
        }
        async fn reap(&self, namespace: &str, name: &str, _: bool) -> Result<(), String> {
            self.reaps.fetch_add(1, Ordering::SeqCst);
            self.backend.using::<ResourceConvoy>(namespace).delete(name).await.map_err(|e| e.to_string())
        }
    }

    enum Backing {
        Dead,
        Live,
    }
    #[async_trait]
    impl StandingConvoyBackingInspector for Backing {
        async fn verify_backing_dead(&self, _: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
            if matches!(self, Self::Dead) {
                Ok(())
            } else {
                Err("backing still live".into())
            }
        }
    }

    async fn fixture() -> (EnsureReconciler, Admission, Arc<VirtualClock>) {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let clock = Arc::new(VirtualClock::new(Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("fixture timestamp")));
        backend
            .using::<Project>("test")
            .create(
                &InputMeta::builder().name("project".into()).build(),
                &ProjectSpec::builder().display_name("project".into()).default_workflow_ref("standing".into()).build(),
            )
            .await
            .expect("fixture operation succeeds");
        backend
            .using::<ConvoyEnsure>("test")
            .create(
                &InputMeta::builder()
                    .name("standing".into())
                    .annotations(BTreeMap::from([(SOURCE_COMMIT_ANNOTATION.into(), "commit".into())]))
                    .build(),
                &ConvoyEnsureSpec::builder()
                    .project_ref("project".into())
                    .role("standing".into())
                    .workflow_ref("standing".into())
                    .repositories(Vec::new())
                    .build(),
            )
            .await
            .expect("fixture operation succeeds");
        let controller = EnsureReconciler::builder().resource_backend(backend.clone()).clock(clock.clone()).build();
        let admission = Admission::builder().backend(backend).build();
        (controller, admission, clock)
    }
    async fn fail_current(admission: &Admission) {
        let ensure = admission.backend.using::<ConvoyEnsure>("test").get("standing").await.expect("ensure fixture");
        let name = ensure.status.expect("resource status").convoy_ref.expect("admitted convoy reference");
        let convoys = admission.backend.using::<ResourceConvoy>("test");
        let convoy = convoys.get(&name).await.expect("admitted convoy");
        let mut status = convoy.status.expect("resource status");
        status.phase = ConvoyPhase::Failed;
        convoys.update_status(&name, &convoy.metadata.resource_version, &status).await.expect("fixture operation succeeds");
    }

    // #2220: read-only admission refusals obey their deadline, survive controller
    // reconstruction, and retry without consuming the runtime restart budget.
    #[tokio::test]
    async fn admission_backoff_survives_controller_restart() {
        let (controller, admission, clock) = fixture().await;
        admission.refused.store(true, Ordering::SeqCst);
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_err());
        let controller = EnsureReconciler::builder().resource_backend(admission.backend.clone()).clock(clock.clone()).build();
        clock.advance(ChronoDuration::seconds(29));
        assert!(controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
            .await
            .expect("fixture operation succeeds")
            .is_empty());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), 1);
        admission.refused.store(false, Ordering::SeqCst);
        clock.advance(ChronoDuration::seconds(1));
        assert_eq!(
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds")
                .len(),
            1
        );
        let status =
            admission.backend.using::<ConvoyEnsure>("test").get("standing").await.expect("ensure fixture").status.expect("resource status");
        assert_eq!(status.restart_count, 0);
        assert!(status.retry_at.is_none());
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
    }

    // #2220: live backing raises attention and holds admission; verified death
    // schedules a restart, and three failed generations exhaust the strike budget.
    #[tokio::test]
    async fn backing_attention_restart_and_strike_budget() {
        let (controller, admission, clock) = fixture().await;
        controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
            .await
            .expect("fixture operation succeeds");
        fail_current(&admission).await;
        controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Live)
            .await
            .expect("fixture operation succeeds");
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
        let demands = admission.backend.using::<ResourceDemand>("test");
        assert!(demands.get("ensure-attention-standing").await.is_ok());
        for expected_commits in 2..=3 {
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds");
            assert!(matches!(demands.get("ensure-attention-standing").await, Err(ResourceError::NotFound { .. })));
            clock.advance(ChronoDuration::minutes(3));
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds");
            assert_eq!(admission.commits.load(Ordering::SeqCst), expected_commits);
            fail_current(&admission).await;
        }
        controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
            .await
            .expect("fixture operation succeeds");
        clock.advance(ChronoDuration::hours(1));
        assert!(controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
            .await
            .expect("fixture operation succeeds")
            .is_empty());
        assert_eq!(admission.commits.load(Ordering::SeqCst), 3);
        assert!(demands.get("ensure-attention-standing").await.expect("ensure attention").spec.expiry.is_some());
    }

    // #2220: reaping is idempotent for absence and refuses another ensure's
    // convoy before crossing the lifecycle port. One scenario covers the glue.
    #[tokio::test]
    async fn reap_checks_ownership_and_delegates_once() {
        let (controller, admission, _) = fixture().await;
        let ensure = admission.backend.using::<ConvoyEnsure>("test").get("standing").await.expect("ensure fixture");
        controller.start_ensured_convoy(&admission, "test", &ensure).await.expect("fixture operation succeeds");
        assert!(controller.reap_ensured_convoy(&admission, "test", "foreign", "generation-1", false).await.is_err());
        assert_eq!(admission.reaps.load(Ordering::SeqCst), 0);
        controller.reap_ensured_convoy(&admission, "test", "standing", "generation-1", false).await.expect("fixture operation succeeds");
        controller.reap_ensured_convoy(&admission, "test", "standing", "generation-1", false).await.expect("fixture operation succeeds");
        assert_eq!(admission.reaps.load(Ordering::SeqCst), 1);
    }
    // #2220: a named admission dependency arriving invalidates a refusal's
    // deadline and allows the next pass to retry immediately.
    #[tokio::test]
    async fn dependency_arrival_invalidates_admission_backoff() {
        let (controller, admission, _) = fixture().await;
        let ensure = admission.backend.using::<ConvoyEnsure>("test").get("standing").await.expect("ensure fixture");
        let absent = controller.ensure_admission_dependency_hash(&admission, "test", &ensure).await.expect("fixture operation succeeds");
        admission.refused.store(true, Ordering::SeqCst);
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_err());
        admission
            .backend
            .using::<WorkflowTemplate>("test")
            .create(&InputMeta::builder().name("standing".into()).build(), &WorkflowTemplateSpec::builder().build())
            .await
            .expect("fixture operation succeeds");
        assert_ne!(
            absent,
            controller.ensure_admission_dependency_hash(&admission, "test", &ensure).await.expect("fixture operation succeeds")
        );
        admission.refused.store(false, Ordering::SeqCst);
        assert_eq!(
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds")
                .len(),
            1
        );
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
    }
}
