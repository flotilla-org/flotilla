//! Standing-convoy reconciliation and its complete transaction guard.

use flotilla_resources::canonical_placement_host_ref_from_sources;
use flotilla_store::ResolvedCascadeStoreExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex},
};

use crate::convoy_admission::PreparedConvoyAdmission;
use crate::convoy_ensure::ConvoyEnsureAdmission;
use crate::convoy_ensure::ConvoyEnsureReconciler;
use crate::convoy_ensure::StandingConvoyBackingInspector;
use crate::ports::ConvoyTeardown;
use crate::standing::{self, Decision, Ending, GenerationEnd, HoldAttention, StandingHold, StandingReading, StandingRole};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_charter::ops_entry::{
    materialized_workflow_name, ENSURED_FROM_ANNOTATION, ENSURE_PROVENANCE_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION,
    PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
};
use flotilla_protocol::{PlacementTargetHost, PrincipalRef};
use flotilla_resources::attentions::{Attention, Attentions, Fact, OpenAttention, Sender, Subject};
use flotilla_resources::convoy::engine::{launch_of, standing_name};
use flotilla_resources::definition_provenance::{MANIFEST_RECONCILER_ROOT_ANNOTATION, MANIFEST_SOURCE_ANNOTATION};
use flotilla_resources::standing_roles::{RunningValues, StandingRoles};
use flotilla_resources::{
    config_changes, Clock, ControllerRetry, ControllerRetryDisposition, Convoy as ResourceConvoy, ConvoyEnsure, ConvoyEnsureSpec,
    ConvoyPhase, FulfilmentKind, Host as ResourceHost, Project, ReadResourceObject, Repository, ResourceError, ResourceObject,
    ResourceProvenance, WorkflowTemplate, LOCAL_RETRY_LIMIT,
};
use flotilla_store::{ResourceBackend, StoreQuery};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// The note a refused admission leaves: the only state a standing role's controller holds of its
/// own. It is in memory and resets when the controller restarts, which retries at once: a refusal
/// admitted no generation, so nothing durable records it, and every host derives the rest of the
/// role's state from its generations and their attention (#3108).
#[derive(Debug, Clone)]
struct EnsureAdmissionRetry {
    config_hash: String,
    dependency_hash: String,
    retry: ControllerRetry,
    failure: String,
    refused_at: DateTime<Utc>,
}

impl EnsureAdmissionRetry {
    /// Whether the refusals reached [`LOCAL_RETRY_LIMIT`]: the role then waits for an event.
    fn past_limit(&self) -> bool {
        self.retry.next_attempt_at().is_none()
    }
}

/// What a role past the refusal limit needs before it is admitted again.
const ADMISSION_REFUSED_NEEDS: &str = "a changed declaration or dependency, an acknowledgement, or reconcile-now";

/// The reason a standing role's restart-limit hold gives its person: `failures` consecutive
/// generations of it failed.
pub fn restart_limit_reason(failures: u32) -> String {
    format!("ensured convoy entered a terminal failure phase; {failures} consecutive generations failed")
}

fn record_ensure_admission_retry(
    retries: &mut HashMap<(String, String), EnsureAdmissionRetry>,
    key: (String, String),
    config_hash: String,
    dependency_hash: String,
    now: DateTime<Utc>,
    failure: String,
) -> (u32, Option<DateTime<Utc>>) {
    let previous = retries.get(&key).map(|entry| &entry.retry);
    let mut retry = ControllerRetry::retryable(previous, now, flotilla_resources::PROVISIONING_RETRY_BACKOFF);
    if retry.attempts >= LOCAL_RETRY_LIMIT {
        retry.disposition = ControllerRetryDisposition::Terminal { needs: ADMISSION_REFUSED_NEEDS.to_owned() };
    }
    let result = (retry.attempts, retry.next_attempt_at());
    retries.insert(key, EnsureAdmissionRetry { config_hash, dependency_hash, retry, failure, refused_at: now });
    result
}

#[derive(Clone, Copy)]
enum EnsureConvoyScope {
    Local,
    IncludingReplicas,
}

/// The standing role `ensure` declares, as its drift and refusal attentions name it.
fn standing_subject(ensure: &ResourceObject<ConvoyEnsure>) -> Subject {
    Subject::StandingRole { project: ensure.spec.project_ref.clone(), role: ensure.spec.role.clone() }
}

/// Whether a generation has ended: an ended one admits a successor only through the role's
/// strike run.
fn ended(convoy: &ResourceObject<ResourceConvoy>) -> bool {
    convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal())
}

/// An ended generation's end as [`standing::fold`] reads it: admitted at its creation under the
/// configuration its admission recorded, and ended as its phase says, at its finish (its admission
/// when none was recorded). `None` while it runs.
pub fn generation_end(convoy: &ResourceObject<ResourceConvoy>) -> Result<Option<GenerationEnd>, String> {
    let Some(status) = convoy.status.as_ref() else { return Ok(None) };
    let ending = match status.phase {
        ConvoyPhase::Failed => Ending::Failed,
        ConvoyPhase::Abandoned => Ending::Operator,
        ConvoyPhase::Landed => Ending::Delivered,
        _ => return Ok(None),
    };
    let admitted_at = convoy.metadata.creation_timestamp;
    // A generation whose admission recorded no configuration continues no run.
    let config = status.ensure_admission.as_ref().map(ensure_config_hash).transpose()?.unwrap_or_default();
    Ok(Some(GenerationEnd {
        launch: convoy.spec.generation,
        config,
        admitted_at,
        ending,
        finished_at: status.finished_at.unwrap_or(admitted_at),
        reason: status.message.clone(),
    }))
}

/// The name of the role's launch `n`: what its hold attention is about.
fn launch_name(ensure: &ResourceObject<ConvoyEnsure>, n: u64) -> Result<String, String> {
    Ok(format!("{}#{n}", standing_name(&ensure.spec.project_ref, &ensure.spec.role)?))
}

fn ensure_config_hash(spec: &ConvoyEnsureSpec) -> Result<String, String> {
    let encoded = serde_json::to_vec(spec).map_err(|error| format!("serialize ensure config: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Owns standing-convoy retries and serializes periodic, explicit and roll passes.
#[derive(bon::Builder)]
pub struct EnsureReconciler {
    resource_backend: ResourceBackend,
    teardown: Arc<dyn ConvoyTeardown>,
    clock: Arc<dyn Clock>,
    #[builder(skip)]
    ensure_admission_retries: Mutex<HashMap<(String, String), EnsureAdmissionRetry>>,
    /// Keep periodic and explicit ensure passes in one transaction, including
    /// status reads, backing inspection, admission, and status publication.
    #[builder(skip)]
    ensure_reconciliation: Mutex<()>,
    /// Per namespace, the earliest time the last pass deferred an ensure to.
    #[builder(skip)]
    next_due: StdMutex<HashMap<String, DateTime<Utc>>>,
    /// The daemon's ManifestRoots (#3824 layer 2c).
    manifest_roots: Arc<dyn flotilla_resources::manifest_roots::ManifestRoots>,
    /// Each standing role's running values, which this home's passes fold its generations' ends into.
    standing_roles: Arc<dyn StandingRoles>,
}

impl EnsureReconciler {
    fn pass<'a>(&'a self, admission: &'a dyn ConvoyEnsureAdmission) -> EnsurePass<'a> {
        EnsurePass::builder()
            .attentions(self.resource_backend.attentions())
            .teardown(&self.teardown)
            .resource_backend(&self.resource_backend)
            .manifest_roots(Arc::clone(&self.manifest_roots))
            .standing_roles(&*self.standing_roles)
            .clock(&self.clock)
            .ensure_admission_retries(&self.ensure_admission_retries)
            .ensure_reconciliation(&self.ensure_reconciliation)
            .next_due(&self.next_due)
            .admission(admission)
            .build()
    }
}
impl EnsureReconciler {
    // Test entry point for the integration controller used by orchestration-testkit.
    #[doc(hidden)]
    pub async fn ensure_admission_dependency_hash(
        &self,
        admission: &dyn ConvoyEnsureAdmission,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<String, String> {
        self.pass(admission).ensure_admission_dependency_hash(namespace, ensure).await
    }
    // Test entry point for the integration controller used by orchestration-testkit.
    #[doc(hidden)]
    pub async fn start_ensured_convoy(
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
    attentions: Arc<dyn Attentions>,
    teardown: &'a Arc<dyn ConvoyTeardown>,
    resource_backend: &'a ResourceBackend,
    manifest_roots: Arc<dyn flotilla_resources::manifest_roots::ManifestRoots>,
    standing_roles: &'a dyn StandingRoles,
    clock: &'a Arc<dyn Clock>,
    ensure_admission_retries: &'a Mutex<HashMap<(String, String), EnsureAdmissionRetry>>,
    ensure_reconciliation: &'a Mutex<()>,
    next_due: &'a StdMutex<HashMap<String, DateTime<Utc>>>,
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
    fn next_due(&self, namespace: &str) -> Option<DateTime<Utc>> {
        self.next_due.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(namespace).copied()
    }
    async fn standing_role(&self, namespace: &str, project: &str, role: &str) -> Result<Option<StandingRole>, String> {
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).list().await.map_err(|e| e.to_string())?;
        let Some(ensure) = ensures.into_iter().find(|ensure| ensure.spec.project_ref == project && ensure.spec.role == role) else {
            return Ok(None);
        };
        self.role_of(namespace, &ensure).await.map(Some)
    }
}

impl EnsureReconciler {
    /// The standing role `ensure` declares, as every host reads it.
    async fn role_of(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<StandingRole, String> {
        let attentions = self.resource_backend.attentions();
        // Every host reads the role from every home's generations and its admitting home's values;
        // an end not folded in yet reads as folded.
        let newest = newest_generation(&self.resource_backend, namespace, ensure, true).await?;
        let mut values = self.standing_roles.get(&ensure.metadata.name).await?.unwrap_or_default();
        if let Some(end) = newest.as_ref().map(generation_end).transpose()?.flatten() {
            values = standing::fold(&values, &end);
        }
        let now = self.clock.now();
        let (reading, attention, values) = standing_reading(&*attentions, namespace, ensure, newest.as_ref(), &values, now, false).await?;
        let live = newest.as_ref().filter(|convoy| !ended(convoy));
        let hold = match reading.decision {
            Decision::Held { .. } => Some(StandingHold::RestartLimit),
            _ if live.is_none() && attention.open => Some(StandingHold::BackingUnverified),
            _ => None,
        };
        // A hold is on the generation whose end it follows: the last counted.
        let held_on = values.counted.filter(|_| hold.is_some()).map(|n| launch_name(ensure, n)).transpose()?;
        // Only the host that admits the role holds a refusal note.
        let note = self.ensure_admission_retries.lock().await.get(&(namespace.to_owned(), ensure.metadata.name.clone())).cloned();
        let next_attempt = match reading.decision {
            Decision::BackOff { until } => Some(until),
            _ => note.as_ref().and_then(|note| note.retry.next_attempt_at()),
        };
        let backing = match (hold, held_on.as_ref()) {
            (Some(StandingHold::BackingUnverified), Some(convoy)) => attentions
                .open(namespace, Some(Sender::Standing), Some(&Subject::convoy(convoy.clone())))
                .await?
                .into_iter()
                .next()
                .map(|open| open.reason),
            _ => None,
        };
        let drift = live
            .map(|convoy| config_changes(&ensure.spec, convoy.status.as_ref().and_then(|status| status.ensure_admission.as_ref())))
            .filter(|changes| !changes.is_empty())
            .map(|changes| format!("ConfigDrift: {}", changes.join("; ")));
        let last_failure = declaration_refused(&self.resource_backend, namespace, ensure)
            .await?
            .map(|refused| format!("DeclarationRefused: {refused}"))
            .or(drift)
            .or_else(|| matches!(hold, Some(StandingHold::RestartLimit)).then(|| restart_limit_reason(reading.strikes)))
            .or(backing)
            .or_else(|| matches!(reading.decision, Decision::BackOff { .. }).then(|| TERMINAL_FAILURE.to_owned()))
            .or_else(|| note.map(|note| note.failure));
        Ok(StandingRole {
            convoy: live.map(|convoy| convoy.metadata.name.clone()).or(held_on),
            hold,
            strikes: reading.strikes,
            next_attempt,
            last_failure,
        })
    }
}

/// What a standing role's failed generation says of it while it backs off.
const TERMINAL_FAILURE: &str = "ensured convoy entered a terminal failure phase";

impl EnsurePass<'_> {
    /// Records that an ensure deferred its work until `at`; the pass's caller wakes then.
    fn due_at(&self, namespace: &str, at: DateTime<Utc>) {
        let mut due = self.next_due.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let earliest = due.entry(namespace.to_owned()).or_insert(at);
        *earliest = (*earliest).min(at);
    }

    /// Definitions reads merge every host's copy, so local presence does not
    /// establish home. ManifestRoots are home-bound; undeclared definitions
    /// belong to their earliest creation, before later local reapplications.
    /// A declaration owns the name: while any live copy carries a manifest
    /// stamp, its root's home decides and undeclared copies confer nothing.
    async fn driverless_ensure_is_home(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<bool, ResourceError> {
        if ensure.metadata.annotations.contains_key(MANIFEST_RECONCILER_ROOT_ANNOTATION) {
            return self.declaring_root_is_local(namespace, &ensure.metadata.annotations).await;
        }
        let local_root = self.resource_backend.local_root()?;
        // Read raw sources: the definitions resolver intentionally reports a
        // merged object as local, including when all its sources are replicas.
        let sources = self
            .resource_backend
            .query_declared::<ConvoyEnsure>(namespace, &StoreQuery::names([ensure.metadata.name.clone()], true))
            .await?;
        let live = sources.items.into_iter().filter(|source| source.object.metadata.deletion_timestamp.is_none()).collect::<Vec<_>>();
        // Reads prefer this home's copy, so another home's declared copy shows only here.
        if let Some(declared) =
            live.iter().find(|source| source.object.metadata.annotations.contains_key(MANIFEST_RECONCILER_ROOT_ANNOTATION))
        {
            return self.declaring_root_is_local(namespace, &declared.object.metadata.annotations).await;
        }
        // Replication preserves each origin's creation_timestamp; it never
        // substitutes the replica's arrival time. Reapplications create later sources.
        let first = live.into_iter().min_by_key(|source| {
            let origin = match &source.provenance {
                ResourceProvenance::Local => &local_root,
                ResourceProvenance::Replica { origin_root, .. } => origin_root,
            };
            (source.object.metadata.creation_timestamp, origin.clone())
        });
        Ok(first.is_some_and(|source| matches!(source.provenance, ResourceProvenance::Local)))
    }

    /// Whether the ManifestRoot a declared copy's `annotations` name is held here.
    async fn declaring_root_is_local(&self, namespace: &str, annotations: &BTreeMap<String, String>) -> Result<bool, ResourceError> {
        let Some(root) = annotations.get(MANIFEST_RECONCILER_ROOT_ANNOTATION) else { return Ok(false) };
        if let Some(root) = self.manifest_roots.root(namespace, root).await.map_err(ResourceError::other)? {
            return Ok(root.local);
        }
        // ADR 0047: previous-generation provenance named the host, not the ManifestRoot.
        // Match its source too, as manifest resolution does; remove this fallback after one roll.
        let Some(source) = annotations.get(MANIFEST_SOURCE_ANNOTATION) else {
            // The declaration may not have replicated yet. Never adopt its ensure.
            return Ok(false);
        };
        let roots = self.manifest_roots.roots(namespace).await.map_err(ResourceError::other)?;
        let mut matching = roots.into_iter().filter(|candidate| candidate.spec.host == *root && candidate.spec.source == *source);
        let first = matching.next();
        Ok(matching.next().is_none() && first.is_some_and(|root| root.local))
    }

    pub async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        // Each full pass tells afresh when it next owes work.
        self.next_due.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(namespace);
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).list().await.map_err(|e| e.to_string())?;
        let ensure_names = ensures.iter().map(|ensure| ensure.metadata.name.clone()).collect::<HashSet<_>>();
        self.ensure_admission_retries
            .lock()
            .await
            .retain(|(retry_namespace, retry_name), _| retry_namespace != namespace || ensure_names.contains(retry_name));
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
                // Only the driver admits. Whether it is known and reachable is read from its Host
                // records by whoever asks; the ensure keeps no condition of it.
                let target = match canonical_placement_host_ref(self.resource_backend, namespace, driver_ref).await {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        debug!(ensure = %ensure.metadata.name, %driver_ref, "standing convoy driver host is unknown");
                        continue;
                    }
                    Err(error) => {
                        errors.push(format!(
                            "ConvoyEnsure/{}: driver host `{driver_ref}` could not be resolved: {error}",
                            ensure.metadata.name
                        ));
                        continue;
                    }
                };
                if self.admission.local_host_id().as_ref() != Some(&target.reference) {
                    continue;
                }
                match self.reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector, false).await {
                    Ok(Some(change)) => changes.push(change),
                    Ok(None) => {}
                    Err(error) => errors.push(format!("ConvoyEnsure/{}: {error}", ensure.metadata.name)),
                }
                continue;
            } else {
                match self.driverless_ensure_is_home(namespace, &ensure).await {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(ensure = %ensure.metadata.name, "skipping standing convoy ensure away from its definition home");
                        continue;
                    }
                    Err(error) => {
                        errors.push(format!("ConvoyEnsure/{}: could not resolve definition home: {error}", ensure.metadata.name));
                        continue;
                    }
                }
                match self.resource_backend.definitions::<Project>(namespace).get(&ensure.spec.project_ref).await {
                    Ok(project) if project.metadata.deletion_timestamp.is_none() => {}
                    Ok(_) | Err(ResourceError::NotFound { .. }) => {
                        errors.push(format!("ConvoyEnsure/{}: parent Project/{} is absent", ensure.metadata.name, ensure.spec.project_ref));
                        continue;
                    }
                    Err(error) => {
                        errors.push(format!(
                            "ConvoyEnsure/{}: could not resolve Project/{}: {error}",
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

    /// Drive one ensure's admission synchronously, past its backoff. Below the refusal limit
    /// that resets its retry budget; past it, a refusal waits for the next event again.
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
        let ensure = match self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).get(name).await {
            Ok(ensure) => ensure,
            Err(ResourceError::NotFound { .. }) => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let mut query = flotilla_store::live_convoys_query();
        // Add the ensure selector without replacing any inherited live predicates; its generations
        // are read through the launch index by their project and the role's launch name.
        query.fields.insert(format!("metadata.annotations.\"{ENSURED_FROM_ANNOTATION}\""), name.into());
        query.labels = generation_labels(&ensure)?;
        query.include_replicas = matches!(scope, EnsureConvoyScope::IncludingReplicas);
        self.resource_backend
            .query_declared::<ResourceConvoy>(namespace, &query)
            .await
            .map(|rows| rows.items)
            .map_err(|error| error.to_string())
    }

    /// Explicitly retire a drifted running generation and admit its successor.
    /// Admission is prepared before abandonment so invalid declarations cannot
    /// interrupt the current crew. The ordinary lifecycle reclaims old backing.
    pub async fn roll_convoy_ensure(&self, namespace: &str, name: &str) -> Result<String, String> {
        let _reconciliation = self.ensure_reconciliation.lock().await;
        let ensure = self.resource_backend.definitions::<ConvoyEnsure>(namespace).get(name).await.map_err(|error| error.to_string())?;
        if self.declaration_refused(namespace, &ensure).await?.is_some() {
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
        self.teardown
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
        self.ensure_admission_retries.lock().await.remove(&(namespace.to_string(), name.to_string()));
        if let Some(target) = driver_target {
            if self.admission.local_host_id().as_ref() != Some(&target.reference) {
                return Ok(format!("ConvoyEnsure/{name} retired its old generation; admission awaits driver {}", target.reference));
            }
        }
        let replacement = self.commit_ensured_convoy(namespace, &ensure, admission).await?;
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
            let kinds = self.resource_backend.including_replicas::<FulfilmentKind>(namespace);
            match kinds.get(name).await {
                Ok(kind) => {
                    versions.insert(format!("FulfilmentKind/{name}"), kind.object.metadata.resource_version.clone());
                    // The harnesses the placement's host publishes for it, as admission reads
                    // them: an adapter refusal ends once they change, and no other host
                    // observation resets the refusals.
                    let adapters = match crate::convoy_admission::placement_agent_adapters(
                        self.resource_backend,
                        namespace,
                        &kind.object,
                        false,
                    )
                    .await
                    {
                        Ok((adapters, detail)) => format!("{adapters:?} ({detail})"),
                        Err(refused) => format!("refused:{refused}"),
                    };
                    versions.insert(format!("PlacementAdapters/{name}"), adapters);
                }
                Err(error) => {
                    versions.insert(format!("FulfilmentKind/{name}"), format!("absent:{error}"));
                }
            }
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

        let latest = self.newest_generation(namespace, ensure).await?;
        let now = self.clock.now();
        let (reading, attention, values) = self.standing_reading(namespace, ensure, latest.as_ref(), now, force_now).await?;
        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let config_hash = ensure_config_hash(&ensure.spec)?;
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        match reading.decision {
            Decision::Running => return Ok(None),
            Decision::Held { raise } => {
                if raise {
                    let failure = restart_limit_reason(reading.strikes);
                    // Raised from the held generation's admission while its record stays, else from its end.
                    let began_at = latest.as_ref().map(|latest| latest.metadata.creation_timestamp).or(values.backoff_from).unwrap_or(now);
                    self.raise_ensure_attention(ensure, &held_on(ensure, &values)?, &failure, began_at).await?;
                    return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
                }
                return Ok(None);
            }
            Decision::BackOff { until } => {
                // Automatic passes require positive death evidence before admitting a successor; a
                // retired generation holds nothing.
                if let Some(latest) = &latest {
                    backing_inspector.verify_backing_dead(latest).await?;
                }
                self.due_at(namespace, until);
                return Ok(None);
            }
            Decision::Admit => {}
        }
        // `reconcile-now` is the operator's explicit acknowledgement that any missing backing
        // records were deliberately wiped; a released hold admits at once.
        let released = force_now || attention.acknowledged;
        if attention.open {
            self.clear_ensure_attention(namespace, ensure).await?;
        }
        if let Some(latest) = latest.as_ref().filter(|_| !released && reading.strikes > 0) {
            backing_inspector.verify_backing_dead(latest).await?;
        }
        if self.admission_waits(namespace, ensure, &retry_key, &config_hash, &dependency_hash, released, force_now).await? {
            return Ok(None);
        }

        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(_) => self.admitted(namespace, ensure, &retry_key).await,
            Err(error) => self.admission_refused(namespace, ensure, retry_key, config_hash, dependency_hash, now, error).await,
        }
    }

    async fn reconcile_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
        force_now: bool,
    ) -> Result<Option<String>, String> {
        let now = self.clock.now();
        let config_hash = ensure_config_hash(&ensure.spec)?;
        let newest = self.newest_generation(namespace, ensure).await?;
        if let Some(convoy) = newest.as_ref().filter(|convoy| !ended(convoy)) {
            self.observe_ensure_config_drift(namespace, ensure, convoy).await?;
            self.clear_ensure_attention(namespace, ensure).await?;
            return Ok(None);
        }

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        let (reading, attention, values) = self.standing_reading(namespace, ensure, newest.as_ref(), now, force_now).await?;
        // A role none of whose generations ended here has nothing to count or hold.
        if values.counted.is_none() {
            if self.admission_waits(namespace, ensure, &retry_key, &config_hash, &dependency_hash, force_now, force_now).await? {
                return Ok(None);
            }
            return self.restart_absent_ensured_convoy(namespace, ensure, now).await;
        }
        let held_on = held_on(ensure, &values)?;
        match reading.decision {
            Decision::Running => return Ok(None),
            Decision::Held { raise } => {
                if !raise {
                    return Ok(None);
                }
                // Raised once, as the hold begins; one on its backing earlier is another episode.
                self.raise_ensure_attention(ensure, &held_on, &restart_limit_reason(reading.strikes), now).await?;
                return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
            }
            Decision::BackOff { .. } | Decision::Admit => {}
        }

        // A retired generation holds nothing: its backing is released.
        if let Some(latest) = &newest {
            let operator_forced = latest.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
            // A forced pass is the recovery boundary for recordless teardown: the
            // operator has acknowledged that raw deletion destroyed the evidence.
            if !operator_forced && !force_now {
                if let Err(reason) = backing_inspector.verify_backing_dead(latest).await {
                    let failure = format!("standing convoy teardown held: {reason}");
                    warn!(ensure = %ensure.metadata.name, convoy = %latest.metadata.name, %failure, "standing convoy backing evidence refused");
                    // The hold on a generation begins as it ends.
                    let ended_at = generation_end(latest)?.map_or(now, |end| end.finished_at);
                    self.raise_ensure_attention(ensure, &latest.metadata.name, &failure, ended_at).await?;
                    if attention.open {
                        return Ok(None);
                    }
                    return Ok(Some(format!("ConvoyEnsure/{} held for operator attention", ensure.metadata.name)));
                }
            }
        }
        // The backing is verified dead: a hold on it is answered.
        self.clear_ensure_attention(namespace, ensure).await?;
        self.clear_hold(namespace, &held_on).await?;
        if let Decision::BackOff { until } = reading.decision {
            self.due_at(namespace, until);
            return Ok(None);
        }
        if self.admission_waits(namespace, ensure, &retry_key, &config_hash, &dependency_hash, force_now, force_now).await? {
            return Ok(None);
        }
        // The ended generation's end is folded in, and it retires once nothing holds it. A
        // successful restart admits the next generation under a fresh record name.
        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(_) => self.admitted(namespace, ensure, &retry_key).await,
            Err(error) => self.admission_refused(namespace, ensure, retry_key, config_hash, dependency_hash, now, error).await,
        }
    }

    async fn restart_absent_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, String> {
        self.clear_ensure_attention(namespace, ensure).await?;
        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        // With no running convoy, no admitted configuration can have drifted. While refusals
        // run, the role's attention is their own, which only an admission or a changed
        // declaration ends.
        if !self.ensure_admission_retries.lock().await.contains_key(&retry_key) {
            self.attentions.clear(namespace, Sender::Standing, &standing_subject(ensure)).await?;
        }
        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(_) => self.admitted(namespace, ensure, &retry_key).await,
            Err(error) => {
                let config_hash = ensure_config_hash(&ensure.spec)?;
                let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
                self.admission_refused(namespace, ensure, retry_key, config_hash, dependency_hash, now, error).await
            }
        }
    }

    /// A generation was admitted: the refusal note ends, and with it the refusal attention.
    async fn admitted(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        retry_key: &(String, String),
    ) -> Result<Option<String>, String> {
        self.ensure_admission_retries.lock().await.remove(retry_key);
        self.attentions.clear(namespace, Sender::Standing, &standing_subject(ensure)).await?;
        Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
    }

    /// Records a refused admission: a warning with its cause and the in-memory refusal note, which
    /// backs the next attempt off. At [`LOCAL_RETRY_LIMIT`] refusals in a row the role stops
    /// retrying on time and tells a person, once while the refusals run. A refusal is no strike:
    /// no generation was admitted.
    #[allow(clippy::too_many_arguments)]
    async fn admission_refused(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        retry_key: (String, String),
        config_hash: String,
        dependency_hash: String,
        now: DateTime<Utc>,
        error: String,
    ) -> Result<Option<String>, String> {
        warn!(ensure = %ensure.metadata.name, %error, "standing convoy admission refused");
        let (refusals, retry_at) = {
            let mut retries = self.ensure_admission_retries.lock().await;
            record_ensure_admission_retry(&mut retries, retry_key, config_hash, dependency_hash, now, error.clone())
        };
        let Some(retry_at) = retry_at else {
            let fact = Fact::Refused {
                ensure: ensure.metadata.name.clone(),
                project: ensure.spec.project_ref.clone(),
                role: ensure.spec.role.clone(),
            };
            // The same refusal is one open attention; a person's acknowledgement answers the
            // refusals before it, and a later one tells them again.
            self.attentions.raise(namespace, &Attention::new(fact, format!("AdmissionRefused: {error}"), now)).await?;
            return Err(format!("admission refused ({refusals} consecutive refusals); waiting for {ADMISSION_REFUSED_NEEDS}: {error}"));
        };
        self.due_at(namespace, retry_at);
        Err(format!("admission refused ({refusals} consecutive refusals); retry at {retry_at}: {error}"))
    }

    /// Whether a refused admission still waits, telling the pass when a timed wait ends. A changed
    /// declaration or dependency ends the wait and resets the count; a changed declaration also
    /// withdraws the refusal attention. Below [`LOCAL_RETRY_LIMIT`] refusals the wait ends at its
    /// deadline or when `released`. Past it, only reconcile-now (`forced`) or a person's
    /// acknowledgement since the last refusal admits, and the note stays, so the next refusal
    /// waits again.
    #[allow(clippy::too_many_arguments)]
    async fn admission_waits(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        retry_key: &(String, String),
        config_hash: &str,
        dependency_hash: &str,
        released: bool,
        forced: bool,
    ) -> Result<bool, String> {
        let Some(note) = self.ensure_admission_retries.lock().await.get(retry_key).cloned() else { return Ok(false) };
        if note.config_hash != config_hash || note.dependency_hash != dependency_hash {
            self.ensure_admission_retries.lock().await.remove(retry_key);
            if note.past_limit() && note.config_hash != config_hash {
                self.attentions.clear(namespace, Sender::Standing, &standing_subject(ensure)).await?;
            }
            return Ok(false);
        }
        if !note.past_limit() {
            if released {
                self.ensure_admission_retries.lock().await.remove(retry_key);
                return Ok(false);
            }
            let retry_at = note.retry.next_attempt_at().expect("a note below the limit is retryable");
            if retry_at <= self.clock.now() {
                return Ok(false);
            }
            self.due_at(namespace, retry_at);
            return Ok(true);
        }
        if forced {
            return Ok(false);
        }
        let acknowledged = self.attentions.acknowledged(namespace, Sender::Standing, &standing_subject(ensure), note.refused_at).await?;
        Ok(acknowledged.is_none())
    }

    /// The role's newest generation homed here that has not retired.
    async fn newest_generation(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<Option<ResourceObject<ResourceConvoy>>, String> {
        newest_generation(self.resource_backend, namespace, ensure, false).await
    }

    /// The role's reading, its hold attention and its running values, with the end of `newest`
    /// folded in and a person's acknowledgement of the hold taken: each written once, here, where
    /// the role is admitted. Folding the end releases the generation to retire.
    async fn standing_reading(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        newest: Option<&ResourceObject<ResourceConvoy>>,
        now: DateTime<Utc>,
        forced: bool,
    ) -> Result<(StandingReading, HoldAttention, RunningValues), String> {
        let stored = self.standing_roles.get(&ensure.metadata.name).await?;
        let mut values = stored.clone().unwrap_or_default();
        if let Some(end) = newest.map(generation_end).transpose()?.flatten() {
            values = standing::fold(&values, &end);
        }
        let (reading, attention, values) = standing_reading(&*self.attentions, namespace, ensure, newest, &values, now, forced).await?;
        if stored.as_ref() != Some(&values) && values != RunningValues::default() {
            self.standing_roles.put(&ensure.metadata.name, &values).await?;
        }
        Ok((reading, attention, values))
    }

    /// Raises or clears the role's drift attention from how its declaration differs from the
    /// configuration that admitted `convoy`, its current generation.
    async fn observe_ensure_config_drift(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &ResourceObject<ResourceConvoy>,
    ) -> Result<(), String> {
        let changes = config_changes(&ensure.spec, convoy.status.as_ref().and_then(|status| status.ensure_admission.as_ref()));
        // Only the convoy's authority host raises runtime attention.
        match self.resource_backend.using::<ResourceConvoy>(namespace).get(&convoy.metadata.name).await {
            Ok(_) => {}
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        }
        let role = standing_subject(ensure);
        if changes.is_empty() {
            return self.attentions.clear(namespace, Sender::Standing, &role).await;
        }
        let fact = Fact::Drift {
            ensure: ensure.metadata.name.clone(),
            project: ensure.spec.project_ref.clone(),
            role: ensure.spec.role.clone(),
            convoy: convoy.metadata.name.clone(),
        };
        let reason = format!("ConfigDrift: {}; run flotilla ensure roll {}", changes.join("; "), ensure.metadata.name);
        // A generation's drift is one episode from its admission: a person who acknowledged it
        // answered every change to this generation's declaration.
        self.attentions.raise(namespace, &Attention::new(fact, reason, convoy.metadata.creation_timestamp)).await
    }

    async fn raise_ensure_attention(
        &self,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &str,
        reason: &str,
        began_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let fact = Fact::Hold { ensure: ensure.metadata.name.clone(), convoy: convoy.to_owned() };
        self.attentions.raise(&ensure.metadata.namespace, &Attention::new(fact, reason, began_at)).await
    }

    /// Withdraws the hold on `convoy` if it is open.
    async fn clear_hold(&self, namespace: &str, convoy: &str) -> Result<(), String> {
        self.attentions.clear(namespace, Sender::Standing, &Subject::convoy(convoy)).await
    }

    /// Clears the hold on any of the ensure's generations: each is a launch of the role's standing name.
    async fn clear_ensure_attention(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<(), String> {
        let standing = standing_name(&ensure.spec.project_ref, &ensure.spec.role)?;
        let holds = self.attentions.open(namespace, Some(Sender::Standing), None).await?.into_iter().filter(OpenAttention::is_hold);
        for hold in holds {
            if matches!(&hold.subject, Subject::Convoy { convoy } if launch_of(convoy) == standing) {
                self.attentions.clear(namespace, Sender::Standing, &hold.subject).await?;
            }
        }
        Ok(())
    }

    /// The refusal the role's project declaration carries, if any: an ensure is refused with it.
    async fn declaration_refused(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<Option<String>, String> {
        declaration_refused(self.resource_backend, namespace, ensure).await
    }

    async fn start_ensured_convoy(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let (ensure, admission) = self.admission.prepare(namespace, ensure).await?;
        match self.commit_ensured_convoy(namespace, &ensure, admission).await {
            Ok(name) => Ok(name),
            // A commit that fails after creating its convoy has still admitted a generation; the
            // caller records it, or its end would never count against the restart budget (#4310).
            // A live generation homed here is this commit's: a role is admitted only with none live,
            // and each reconciliation pass holds the ensure reconciliation lock, so no other pass
            // admits for this ensure meanwhile.
            Err(error) => match self.active_ensured_convoys(namespace, &ensure.metadata.name, EnsureConvoyScope::Local).await {
                // The read failing too says nothing of the commit; its own error is the one to report.
                Err(listing) => Err(format!("{error}; and the live generation could not be read after it: {listing}")),
                Ok(live) => match live.into_iter().map(|source| source.object).max_by_key(|convoy| convoy.spec.generation) {
                    Some(admitted) => {
                        let (ensure, convoy) = (&ensure.metadata.name, &admitted.metadata.name);
                        warn!(%ensure, %convoy, %error, "standing convoy admitted, though its commit failed after");
                        Ok(admitted.metadata.name)
                    }
                    None => Err(error),
                },
            },
        }
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
        self.teardown.reap(namespace, convoy_name, force).await
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

/// The role `ensure`'s newest generation homed here, or at every home with `everywhere`, that has
/// not retired: the live one, or the last ended and still held. A retired generation's end is
/// folded into the role's values, and it is read no more.
async fn newest_generation(
    backend: &ResourceBackend,
    namespace: &str,
    ensure: &ResourceObject<ConvoyEnsure>,
    everywhere: bool,
) -> Result<Option<ResourceObject<ResourceConvoy>>, String> {
    backend
        .using::<ResourceConvoy>(namespace)
        .query_declared(&StoreQuery {
            fields: BTreeMap::from([(format!("metadata.annotations.\"{ENSURED_FROM_ANNOTATION}\""), ensure.metadata.name.clone())]),
            labels: generation_labels(ensure)?,
            include_replicas: everywhere,
            ..Default::default()
        })
        .await
        .map(|rows| rows.items.into_iter().map(|source| source.object).max_by_key(|convoy| convoy.spec.generation))
        .map_err(|error| error.to_string())
}

/// The labels `ensure`'s generations hold: its project, and its role's launch name, which the
/// launch index reads them by.
fn generation_labels(ensure: &ResourceObject<ConvoyEnsure>) -> Result<BTreeMap<String, String>, String> {
    Ok(BTreeMap::from([
        (flotilla_resources::labels::PROJECT_LABEL.to_owned(), ensure.spec.project_ref.clone()),
        (flotilla_resources::labels::LAUNCH_OF_LABEL.to_owned(), standing_name(&ensure.spec.project_ref, &ensure.spec.role)?),
    ]))
}

/// The generation a held role's hold is on: its last counted, whose end the hold follows.
fn held_on(ensure: &ResourceObject<ConvoyEnsure>, values: &RunningValues) -> Result<String, String> {
    launch_name(ensure, values.counted.ok_or("a held role has counted a generation")?)
}

/// The role's strike run and decision ([`standing::derive`]) from its running `values`, whether
/// `newest` is live, and the hold attention about its last counted generation: whether one is
/// open, and whether a person acknowledged it since the last strike ended, which ends the run.
/// Returns `values` as the acknowledgement leaves them.
async fn standing_reading(
    attentions: &dyn Attentions,
    namespace: &str,
    ensure: &ResourceObject<ConvoyEnsure>,
    newest: Option<&ResourceObject<ResourceConvoy>>,
    values: &RunningValues,
    now: DateTime<Utc>,
    forced: bool,
) -> Result<(StandingReading, HoldAttention, RunningValues), String> {
    let mut attention = HoldAttention::default();
    let mut values = values.clone();
    if let Some(counted) = values.counted {
        // A standing role's attention about a convoy is its hold.
        let subject = Subject::convoy(launch_name(ensure, counted)?);
        attention.open = !attentions.open(namespace, Some(Sender::Standing), Some(&subject)).await?.is_empty();
        // A hold on a generation begins once it has ended.
        if let Some(since) = values.backoff_from.filter(|_| values.strikes > 0) {
            attention.acknowledged = attentions.acknowledged(namespace, Sender::Standing, &subject, since).await?.is_some();
        }
    }
    if attention.acknowledged {
        values = standing::acknowledge(&values);
    }
    let live = newest.is_some_and(|convoy| !ended(convoy));
    let reading = standing::derive(&values, live, &ensure_config_hash(&ensure.spec)?, attention, now, forced);
    Ok((reading, attention, values))
}

/// The refusal `ensure`'s project declaration carries, if any: its ensures are refused with it.
async fn declaration_refused(
    backend: &ResourceBackend,
    namespace: &str,
    ensure: &ResourceObject<ConvoyEnsure>,
) -> Result<Option<String>, String> {
    match backend.definitions::<Project>(namespace).get(&ensure.spec.project_ref).await {
        Ok(project) => Ok(project.status.and_then(|status| status.declaration_refused).map(|refused| refused.message)),
        Err(ResourceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use chrono::{Duration as ChronoDuration, TimeZone};
    use flotilla_engine_convoys_testkit::EngineHome;
    use flotilla_protocol::CanonicalHostId;
    use flotilla_resources::ConvoySpec;
    use flotilla_resources::ConvoyStatus;
    use flotilla_resources::ProjectSpec;
    use flotilla_resources::WorkflowTemplateSpec;
    use flotilla_store_testkit::VirtualClock;

    use super::*;
    use flotilla_engine_messages_testkit::designate_fleet;
    use flotilla_resources::InputMeta;

    // Stands in for provider-backed admission and lifecycle operations. Resource
    // storage, status patches, the clock and the reconcile loop are real.
    #[derive(bon::Builder, Clone)]
    struct Admission {
        backend: ResourceBackend,
        #[builder(skip)]
        refused: Arc<AtomicBool>,
        #[builder(skip)]
        fails_after_create: Arc<AtomicBool>,
        #[builder(skip)]
        prepares: Arc<AtomicU32>,
        #[builder(skip)]
        commits: Arc<AtomicU32>,
        #[builder(skip)]
        reaps: Arc<AtomicU32>,
        /// The engine its generations are admitted on and its attentions raised on.
        home: Option<Arc<EngineHome>>,
        /// The launches admitted, oldest first.
        #[builder(skip)]
        launches: Arc<std::sync::Mutex<Vec<String>>>,
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
                    .evidence(self.backend.clone())
                    .name("unused".into())
                    .spec(ConvoySpec::builder().workflow_ref("standing".into()).project_ref(ensure.spec.project_ref.clone()).build())
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
            // The generation is admitted on the engine, as the next launch of its role.
            let spec = ConvoySpec::builder()
                .workflow_ref("standing".into())
                .project_ref(ensure.spec.project_ref.clone())
                .role(ensure.spec.role.clone())
                .generation(u64::from(generation))
                .build();
            let status = ConvoyStatus {
                ensure_admission: Some(ensure.spec.clone()),
                ..flotilla_engine_convoys_testkit::single_agent(&ensure.spec.role)
            };
            let mut convoy = flotilla_engine_convoys_testkit::convoy(namespace, &ensure.metadata.name, spec, status);
            convoy.metadata.annotations = annotations;
            let launch = self.home().convoys.launch(convoy).await;
            self.launches.lock().expect("launches").push(launch.clone());
            if self.fails_after_create.load(Ordering::SeqCst) {
                return Err("status patch retry budget exhausted".into());
            }
            Ok(launch)
        }
    }

    impl Admission {
        fn home(&self) -> &EngineHome {
            self.home.as_deref().expect("the engine home")
        }
    }

    #[async_trait]
    impl ConvoyTeardown for Admission {
        async fn abandon(&self, _: &str, _: &str, _: &str, _: Option<&PrincipalRef>) -> Result<(), String> {
            unreachable!("not rolling in these contracts")
        }
        async fn reap(&self, namespace: &str, name: &str, _: bool) -> Result<(), String> {
            self.reaps.fetch_add(1, Ordering::SeqCst);
            // The engine ends the launch; it stays readable, ended.
            let convoy = self.backend.using::<ResourceConvoy>(namespace).get(name).await.map_err(|e| e.to_string())?;
            let path = self.home().convoys.path();
            path.abandon(&convoy, "operator", "reaped").await
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

    // Both live scopes and local generation history read only the role's generations, through the
    // launch index by their project and the role's launch name.
    #[tokio::test]
    async fn ensured_convoys_read_only_matching_rows() {
        use flotilla_store::load_metrics::{measure, ReadCounts};
        /// The Convoys the backend's reads returned, as the engine's port counts them.
        fn convoys_read(reads: &ReadCounts) -> u64 {
            reads.get("Convoy port").map_or(0, |counts| counts.rows)
        }
        /// Admits the same 312 generations at `home`: three of ensure `wanted` (one ended), the
        /// rest of `other`, ended but ten. A standing generation never lands: it ends failed.
        async fn admit_generations(home: &EngineHome) {
            for index in 0..312 {
                let ensure = if index == 0 || index >= 310 { "wanted" } else { "other" };
                // Named as admission names a standing role's generations.
                let mut convoy = flotilla_engine_convoys_testkit::convoy(
                    "test",
                    &standing_name("test", ensure).expect("standing name"),
                    ConvoySpec::builder().workflow_ref("workflow".into()).project_ref("test".into()).role(ensure.into()).build(),
                    flotilla_engine_convoys_testkit::single_agent(ensure),
                );
                convoy.metadata.annotations = BTreeMap::from([(ENSURED_FROM_ANNOTATION.into(), ensure.into())]);
                if index < 300 {
                    convoy.status.as_mut().expect("status").phase = ConvoyPhase::Failed;
                }
                home.convoys.launch(convoy).await;
            }
        }
        let home = Arc::new(EngineHome::open_in("home", "test"));
        for ensure in ["wanted", "other"] {
            home.backend()
                .using::<ConvoyEnsure>("test")
                .create(
                    &InputMeta::builder().name(ensure.into()).build(),
                    &ConvoyEnsureSpec::builder()
                        .project_ref("test".into())
                        .role(ensure.into())
                        .workflow_ref("workflow".into())
                        .repositories(Vec::new())
                        .build(),
                )
                .await
                .expect("declare the role");
        }
        admit_generations(&home).await;
        // Another home's generations, replicated here as its log arrives.
        let remote = EngineHome::open_in("remote", "test");
        admit_generations(&remote).await;
        remote.replicate_to(&home);
        let backend = home.backend().clone();
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let admission = Admission::builder().backend(backend.clone()).home(Arc::clone(&home)).build();
        let controller = EnsureReconciler::builder()
            .teardown(Arc::new(admission.clone()))
            .resource_backend(backend.clone())
            .clock(clock)
            .manifest_roots(Arc::clone(&home.engine.ports.manifest_roots))
            .standing_roles(Arc::clone(&home.engine.ports.standing_roles))
            .build();
        for scope in [EnsureConvoyScope::Local, EnsureConvoyScope::IncludingReplicas] {
            let (rows, reads) = measure(controller.pass(&admission).active_ensured_convoys("test", "wanted", scope)).await;
            // The role's three generations at each home are read; the two live of them at each answer.
            let (expected, read) = if matches!(scope, EnsureConvoyScope::Local) { (2, 3) } else { (4, 6) };
            assert_eq!(rows.expect("query").len(), expected);
            assert_eq!(convoys_read(&reads), read);
        }
        let (rows, reads) =
            measure(controller.pass(&admission).active_ensured_convoys("test", "absent", EnsureConvoyScope::IncludingReplicas)).await;
        assert!(rows.expect("empty query").is_empty());
        assert_eq!(convoys_read(&reads), 0);
        let wanted = backend.definitions::<ConvoyEnsure>("test").get("wanted").await.expect("the role's declaration");
        let (newest, reads) = measure(newest_generation(&backend, "test", &wanted, false)).await;
        assert!(newest.expect("local generations").is_some());
        assert_eq!(convoys_read(&reads), 3);
    }

    async fn fixture() -> (EnsureReconciler, Admission, Arc<VirtualClock>) {
        let clock = Arc::new(VirtualClock::new(Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).single().expect("fixture timestamp")));
        let home = Arc::new(EngineHome::open_with("home", "test", clock.clone()));
        let backend = home.backend().clone();
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
                    // As project operations materialise it: the engine keeps provenance whole.
                    .annotations(BTreeMap::from([
                        (MATERIALIZED_PROJECT_ANNOTATION.into(), "project".into()),
                        (SOURCE_REPOSITORY_ANNOTATION.into(), "repository".into()),
                        (SOURCE_COMMIT_ANNOTATION.into(), "commit".into()),
                        (SOURCE_ENTRY_PATH_ANNOTATION.into(), "ops/standing.md".into()),
                    ]))
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
        designate_fleet(&backend, "test").await;
        let admission = Admission::builder().backend(backend.clone()).home(home).build();
        let controller = new_controller(&admission, &clock);
        (controller, admission, clock)
    }

    /// A controller over `admission`'s engine, as the daemon composes one: a restarted one keeps
    /// nothing but what the store holds.
    fn new_controller(admission: &Admission, clock: &Arc<VirtualClock>) -> EnsureReconciler {
        EnsureReconciler::builder()
            .teardown(Arc::new(admission.clone()))
            .resource_backend(admission.backend.clone())
            .clock(clock.clone())
            .manifest_roots(Arc::clone(&admission.home().engine.ports.manifest_roots))
            .standing_roles(Arc::clone(&admission.home().engine.ports.standing_roles))
            .build()
    }

    /// Whether the standing role's hold on its convoy is open.
    async fn hold_open(admission: &Admission) -> bool {
        let open = admission.backend.attentions().open("test", Some(Sender::Standing), None).await.expect("attentions");
        open.iter().any(OpenAttention::is_hold)
    }
    /// The role's newest generation.
    fn current(admission: &Admission) -> String {
        admission.launches.lock().expect("launches").last().expect("a generation was admitted").clone()
    }
    /// The role's first generation.
    fn first(admission: &Admission) -> String {
        admission.launches.lock().expect("launches").first().expect("a generation was admitted").clone()
    }
    async fn standing(controller: &EnsureReconciler) -> StandingRole {
        controller.standing_role("test", "project", "standing").await.expect("role reads").expect("role declared")
    }
    /// Fails the newest generation now, as the engine ends it: its failure is stamped by the
    /// home's clock.
    async fn fail_current(admission: &Admission) {
        let convoy = admission.home().convoy(&current(admission)).await;
        let path = admission.home().convoys.path();
        path.fail(&convoy, "operator", "failed").await.expect("fixture operation succeeds");
    }

    // Behaviour (#3108 retirement): a standing generation's end holds it until its role's pass
    // counts it; counted, it retires, and the role still backs off from its strike.
    #[tokio::test]
    async fn a_counted_generation_retires_and_its_strike_remains() {
        let (controller, admission, clock) = fixture().await;
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("admit");
        fail_current(&admission).await;
        let failed = current(&admission);
        let retire = || admission.home().convoys.convoys().retire(&failed, flotilla_schema::Timestamp::from_datetime(&clock.now()));
        let held = format!("{:?}", retire());
        assert!(held.contains("standing-role:standing"), "uncounted, the end holds its generation: {held}");

        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("count");
        assert_eq!(format!("{:?}", retire()), "Ok(Retired)");
        let restarted = new_controller(&admission, &clock);
        restarted.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("back off");
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1, "the retired generation's strike still backs the role off");
        assert_eq!(standing(&restarted).await.strikes, 1);
    }

    // #2875: a long-lived driver's generation must back off from its failure,
    // not its old creation time; reconstructing the controller retains the deadline.
    #[tokio::test]
    async fn driver_readmission_backs_off_from_failure_time() {
        let (controller, admission, clock) = fixture().await;
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("admit");
        let ensures = admission.backend.using::<ConvoyEnsure>("test");
        // The generation fails a day after it was admitted.
        let admitted = admission.home().convoy(&current(&admission)).await;
        clock.advance(admitted.metadata.creation_timestamp + ChronoDuration::hours(24) - clock.now());
        fail_current(&admission).await;
        for _ in 0..3 {
            let controller = new_controller(&admission, &clock);
            let ensure = ensures.get("standing").await.expect("ensure");
            controller.pass(&admission).reconcile_driver_convoy_ensure("test", &ensure, &Backing::Dead, false).await.expect("backoff");
            assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
            clock.advance(ChronoDuration::seconds(10));
        }
        let ensure = ensures.get("standing").await.expect("ensure");
        controller
            .pass(&admission)
            .reconcile_driver_convoy_ensure("test", &ensure, &Backing::Dead, false)
            .await
            .expect("readmit after delay");
        assert_eq!(admission.commits.load(Ordering::SeqCst), 2);
    }

    // #2220: read-only admission refusals obey their deadline and retry without consuming the
    // restart budget. The deadline is the admitting host's note, so a new controller retries at once.
    #[tokio::test]
    async fn admission_backoff_obeys_its_deadline_and_resets_at_restart() {
        let (controller, admission, clock) = fixture().await;
        admission.refused.store(true, Ordering::SeqCst);
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_err());
        let role = standing(&controller).await;
        assert!(role.next_attempt.is_some_and(|at| at > clock.now()), "the admitting host shows its deadline");
        assert_eq!(role.last_failure.as_deref().map(|failure| failure.contains("admission unavailable")), Some(true));
        clock.advance(ChronoDuration::seconds(29));
        assert!(controller
            .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
            .await
            .expect("fixture operation succeeds")
            .is_empty());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), 1);
        let controller = new_controller(&admission, &clock);
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_err());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), 2, "the note is in memory: a restarted controller retries");
        admission.refused.store(false, Ordering::SeqCst);
        clock.advance(ChronoDuration::seconds(30));
        assert_eq!(
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds")
                .len(),
            1
        );
        let role = standing(&controller).await;
        assert_eq!(role.strikes, 0);
        assert_eq!(role.next_attempt, None);
        assert_eq!(role.convoy, Some(first(&admission)));
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
    }

    // #3108: refusals with the same hashes stop retrying on time at LOCAL_RETRY_LIMIT and tell a
    // person once. Reconcile-now and an acknowledgement each admit once more; a changed declaration
    // withdraws the attention and resets the count; an admission ends both.
    #[tokio::test]
    async fn refusals_past_the_limit_wait_for_events_with_one_attention() {
        let (controller, admission, clock) = fixture().await;
        let messages = &admission.home().messages;
        let refusal_messages = || async {
            let primitives = messages.primitives();
            let mut count = 0;
            for inbox in primitives.person_inboxes("test").await.expect("person inboxes") {
                count += primitives
                    .inbox_messages(&inbox)
                    .expect("messages")
                    .into_iter()
                    .filter(|(_, message)| {
                        message.spec().subject().is_some_and(|subject| subject.address.as_str() == "project:project/role:standing")
                    })
                    .count();
            }
            count
        };
        let pass = || controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead);
        admission.refused.store(true, Ordering::SeqCst);
        for _ in 0..LOCAL_RETRY_LIMIT {
            assert!(pass().await.is_err());
            clock.advance(ChronoDuration::minutes(16));
        }
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT);
        assert_eq!(refusal_messages().await, 1, "the limit tells a person");
        let role = standing(&controller).await;
        assert_eq!(role.next_attempt, None, "past the limit nothing is due on time");

        // Time alone admits nothing more.
        clock.advance(ChronoDuration::hours(6));
        assert!(pass().await.expect("waits").is_empty());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT);

        // Reconcile-now tries once; the same refusal raises no second message.
        assert!(controller.reconcile_convoy_ensure_now(&admission, "test", "standing", &Backing::Dead).await.is_err());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT + 1);
        assert!(pass().await.expect("waits again").is_empty());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT + 1);
        assert_eq!(refusal_messages().await, 1, "repeated refusals with the same hashes are one message");

        // A person's acknowledgement admits once more; the refusal after it tells them again.
        let (_, waiting) = messages
            .primitives()
            .person_messages("test")
            .await
            .expect("person messages")
            .into_iter()
            .next()
            .expect("the refusal waits for a person");
        let address: flotilla_address::Address = waiting.name().parse().expect("message address");
        messages.primitives().acknowledge("test", address.name()).await.expect("acknowledged");
        clock.advance(ChronoDuration::seconds(1));
        assert!(pass().await.is_err());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT + 2);
        assert!(pass().await.expect("waits after the answered attempt").is_empty());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT + 2);
        assert_eq!(refusal_messages().await, 2);

        // A changed declaration withdraws the attention and tries again with a fresh count.
        let ensures = admission.backend.using::<ConvoyEnsure>("test");
        let ensure = ensures.get("standing").await.expect("ensure");
        let changed = ConvoyEnsureSpec { workflow_ref: "standing-two".into(), ..ensure.spec.clone() };
        ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &changed).await.expect("declaration changed");
        assert!(pass().await.is_err());
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT + 3);
        let open = admission.backend.attentions().open("test", Some(Sender::Standing), None).await.expect("attentions");
        assert!(open.is_empty(), "a changed declaration ends the refusal attention: {open:?}");
        assert!(standing(&controller).await.next_attempt.is_some(), "a fresh count retries on time");

        // An admission ends the note.
        admission.refused.store(false, Ordering::SeqCst);
        clock.advance(ChronoDuration::minutes(1));
        assert_eq!(pass().await.expect("admits").len(), 1);
        assert_eq!(standing(&controller).await.convoy, Some(first(&admission)));
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
        assert!(hold_open(&admission).await, "live backing holds it for a person");
        for expected_commits in 2..=3 {
            controller
                .reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead)
                .await
                .expect("fixture operation succeeds");
            assert!(!hold_open(&admission).await, "verified death clears the hold");
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
        assert!(hold_open(&admission).await, "the restart limit holds it for a person");
    }

    // #4310: a commit that fails after creating its convoy has admitted a generation all the same;
    // that generation's failure counts against the restart budget, and no other is admitted for it.
    #[tokio::test]
    async fn a_generation_whose_commit_failed_after_creation_still_counts() {
        let (controller, admission, clock) = fixture().await;
        admission.fails_after_create.store(true, Ordering::SeqCst);
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("admitted");
        admission.fails_after_create.store(false, Ordering::SeqCst);
        fail_current(&admission).await;
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("failure counted");
        assert_eq!(standing(&controller).await.strikes, 1, "the generation's failure is a strike");
        clock.advance(ChronoDuration::minutes(3));
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("restarted");
        assert_eq!(admission.commits.load(Ordering::SeqCst), 2, "one successor, after the backoff");
    }

    // #2220: reaping is idempotent for absence and refuses another ensure's
    // convoy before crossing the lifecycle port. One scenario covers the glue.
    #[tokio::test]
    async fn reap_checks_ownership_and_delegates_once() {
        let (controller, admission, _) = fixture().await;
        let ensure = admission.backend.using::<ConvoyEnsure>("test").get("standing").await.expect("ensure fixture");
        controller.start_ensured_convoy(&admission, "test", &ensure).await.expect("fixture operation succeeds");
        let generation = first(&admission);
        assert!(controller.reap_ensured_convoy(&admission, "test", "foreign", &generation, false).await.is_err());
        assert_eq!(admission.reaps.load(Ordering::SeqCst), 0);
        controller.reap_ensured_convoy(&admission, "test", "standing", &generation, false).await.expect("fixture operation succeeds");
        assert_eq!(admission.reaps.load(Ordering::SeqCst), 1);
        // An ended launch stays readable on the engine; a generation that is absent is reaped
        // without crossing the lifecycle port.
        controller.reap_ensured_convoy(&admission, "test", "standing", "absent#9", false).await.expect("fixture operation succeeds");
        assert_eq!(admission.reaps.load(Ordering::SeqCst), 1);
    }
    // d9: a role refused because its pinned placement's host had not yet published the harness
    // is admitted on the next pass once the host publishes it, not at the refusal's deadline.
    // A host observation that leaves the placement's harnesses as they were changes nothing.
    #[tokio::test]
    async fn a_placement_harness_published_ends_the_refusal_wait() {
        use flotilla_resources::{
            FulfilmentFacts, FulfilmentKindSpec, FulfilmentRealisation, HarnessFacts, Host, HostSpec, HostStatus, PlacementImageSource,
        };
        let (controller, admission, _clock) = fixture().await;
        let hosts = admission.backend.using::<Host>("test");
        let host = hosts.create(&InputMeta::builder().name("home".into()).build(), &HostSpec::default()).await.expect("host");
        hosts.update_status("home", &host.metadata.resource_version, &HostStatus::default()).await.expect("host observed");
        admission
            .backend
            .using::<FulfilmentKind>("test")
            .create(
                &InputMeta::builder().name("crews".into()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref("home".into())
                    .pool("pool".into())
                    .realisation(FulfilmentRealisation::DockerPerVessel {
                        image: Some(PlacementImageSource::Literal("crew:pinned".into())),
                    })
                    .build(),
            )
            .await
            .expect("placement");
        let ensures = admission.backend.using::<ConvoyEnsure>("test");
        let ensure = ensures.get("standing").await.expect("ensure fixture");
        let pinned = ConvoyEnsureSpec { placement_policy: Some("crews".into()), ..ensure.spec.clone() };
        ensures.update(&InputMeta::from(&ensure.metadata), &ensure.metadata.resource_version, &pinned).await.expect("pinned");

        admission.refused.store(true, Ordering::SeqCst);
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_err());
        admission.refused.store(false, Ordering::SeqCst);

        // An observation of something else: the refusal still waits for its deadline.
        let observed = hosts.get("home").await.expect("host");
        let mut status = HostStatus { daemon_generation: Some("other".into()), ..HostStatus::default() };
        hosts.update_status("home", &observed.metadata.resource_version, &status).await.expect("host observed");
        assert!(controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.is_ok());
        assert_eq!(admission.commits.load(Ordering::SeqCst), 0, "waits while the placement's harnesses are unchanged");

        // The host publishes the placement's harness: the next pass admits, the clock unmoved.
        let observed = hosts.get("home").await.expect("host");
        status.fulfilment_facts.insert(
            "crews".into(),
            FulfilmentFacts {
                harnesses: [("standing".into(), HarnessFacts { version: "1".into(), models: Default::default() })].into(),
                ..FulfilmentFacts::default()
            },
        );
        hosts.update_status("home", &observed.metadata.resource_version, &status).await.expect("harness published");
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("admitted");
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
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
