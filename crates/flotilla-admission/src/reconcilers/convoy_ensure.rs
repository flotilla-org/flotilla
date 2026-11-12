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
use crate::standing::{self, Decision, Ending, Generation, HoldAttention, StandingHold, StandingReading, StandingRole};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_charter::ops_entry::{
    materialized_workflow_name, ENSURED_FROM_ANNOTATION, ENSURE_PROVENANCE_ANNOTATION, MATERIALIZED_PROJECT_ANNOTATION,
    PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION, SOURCE_REPOSITORY_ANNOTATION,
};
use flotilla_protocol::{PlacementTargetHost, PrincipalRef};
use flotilla_resources::attentions::{Attention, Attentions, Fact, OpenAttention, Sender, Subject};
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

/// When `convoy` was admitted. The engine's view says so as its start (#3108): the old store's
/// object built from an engine record keeps no creation time and reads as the epoch.
fn admitted_at(convoy: &ResourceObject<ResourceConvoy>) -> DateTime<Utc> {
    convoy.status.as_ref().and_then(|status| status.started_at).unwrap_or(convoy.metadata.creation_timestamp)
}

/// A generation as [`standing::derive`] reads it: admitted at its creation, under `declared` when
/// its admitted configuration is the role's now, and ended as its phase says, at its finish (its
/// admission when none was recorded).
fn generation(convoy: &ResourceObject<ResourceConvoy>, declared: &ConvoyEnsureSpec) -> Generation {
    let status = convoy.status.as_ref();
    let admitted_at = admitted_at(convoy);
    let ending = status.and_then(|status| match status.phase {
        ConvoyPhase::Failed => Some(Ending::Failed),
        ConvoyPhase::Abandoned => Some(Ending::Operator),
        ConvoyPhase::Landed => Some(Ending::Delivered),
        _ => None,
    });
    Generation {
        admitted_at,
        current_config: status.and_then(|status| status.ensure_admission.as_ref()) == Some(declared),
        ended: ending.map(|ending| (ending, status.and_then(|status| status.finished_at).unwrap_or(admitted_at))),
    }
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
}

impl EnsureReconciler {
    fn pass<'a>(&'a self, admission: &'a dyn ConvoyEnsureAdmission) -> EnsurePass<'a> {
        EnsurePass::builder()
            .attentions(self.resource_backend.attentions())
            .teardown(&self.teardown)
            .resource_backend(&self.resource_backend)
            .manifest_roots(Arc::clone(&self.manifest_roots))
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
        // Every host reads the role from every home's generations.
        let generations = newest_generations_first(&self.resource_backend, namespace, &ensure.metadata.name, true).await?;
        let now = self.clock.now();
        let (reading, attention) = standing_reading(&*attentions, namespace, ensure, &generations, now, false).await?;
        let newest = generations.first();
        let live = newest.filter(|convoy| !ended(convoy));
        let hold = match reading.decision {
            Decision::Held { .. } => Some(StandingHold::RestartLimit),
            _ if live.is_none() && attention.open => Some(StandingHold::BackingUnverified),
            _ => None,
        };
        let held_on = newest.filter(|_| hold.is_some());
        // Only the host that admits the role holds a refusal note.
        let note = self.ensure_admission_retries.lock().await.get(&(namespace.to_owned(), ensure.metadata.name.clone())).cloned();
        let next_attempt = match reading.decision {
            Decision::BackOff { until } => Some(until),
            _ => note.as_ref().and_then(|note| note.retry.next_attempt_at()),
        };
        let backing = match (hold, held_on) {
            (Some(StandingHold::BackingUnverified), Some(convoy)) => attentions
                .open(namespace, Some(Sender::Standing), Some(&Subject::convoy(convoy.metadata.name.clone())))
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
            convoy: live.or(held_on).map(|convoy| convoy.metadata.name.clone()),
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
    /// establish home. ManifestRoots are home-bound; an undeclared definition
    /// belongs to the home whose copy sorts first by origin, since every engine
    /// copy reads its creation time as the epoch. A later same-name copy at a
    /// lower-sorting home also acts; owner question open (#3108).
    async fn driverless_ensure_is_home(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<bool, ResourceError> {
        if let Some(root) = ensure.metadata.annotations.get("flotilla.work/manifest-reconciler-root") {
            if let Some(root) = self.manifest_roots.root(namespace, root).await.map_err(ResourceError::other)? {
                return Ok(root.local);
            }
            // ADR 0047: previous-generation provenance named the host, not the ManifestRoot.
            // Match its source too, as manifest resolution does; remove this fallback after one roll.
            let Some(source) = ensure.metadata.annotations.get("flotilla.work/manifest-source") else {
                // The declaration may not have replicated yet. Never adopt its ensure.
                return Ok(false);
            };
            let roots = self.manifest_roots.roots(namespace).await.map_err(ResourceError::other)?;
            let mut matching = roots.into_iter().filter(|candidate| candidate.spec.host == *root && candidate.spec.source == *source);
            let first = matching.next();
            return Ok(matching.next().is_none() && first.is_some_and(|root| root.local));
        }
        let local_root = self.resource_backend.local_root()?;
        // Read raw sources: the definitions resolver intentionally reports a
        // merged object as local, including when all its sources are replicas.
        let sources = self
            .resource_backend
            .query_declared::<ConvoyEnsure>(namespace, &StoreQuery::names([ensure.metadata.name.clone()], true))
            .await?;
        // The engine keeps no creation time (every copy reads as the epoch), so the origin
        // tie-break alone decides which home's copy is first.
        let first = sources.items.into_iter().filter(|source| source.object.metadata.deletion_timestamp.is_none()).min_by_key(|source| {
            let origin = match &source.provenance {
                ResourceProvenance::Local => &local_root,
                ResourceProvenance::Replica { origin_root, .. } => origin_root,
            };
            (source.object.metadata.creation_timestamp, origin.clone())
        });
        Ok(first.is_some_and(|source| matches!(source.provenance, ResourceProvenance::Local)))
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
        let mut query = flotilla_store::live_convoys_query();
        // Add the ensure selector without replacing any inherited live predicates.
        query.fields.insert(format!("metadata.annotations.\"{ENSURED_FROM_ANNOTATION}\""), name.into());
        query.include_replicas = matches!(scope, EnsureConvoyScope::IncludingReplicas);
        self.resource_backend
            .query_declared::<ResourceConvoy>(namespace, &query)
            .await
            .map(|rows| rows.items)
            .map_err(|error| error.to_string())
    }

    async fn ensured_convoy_generations(&self, namespace: &str, name: &str) -> Result<Vec<ResourceObject<ResourceConvoy>>, String> {
        self.resource_backend
            .using::<ResourceConvoy>(namespace)
            .query_declared(&StoreQuery {
                fields: BTreeMap::from([(format!("metadata.annotations.\"{ENSURED_FROM_ANNOTATION}\""), name.into())]),
                ..Default::default()
            })
            .await
            .map(|rows| rows.items.into_iter().map(|source| source.object).collect())
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
            let version =
                kinds.get(name).await.map(|kind| kind.object.metadata.resource_version).unwrap_or_else(|error| format!("absent:{error}"));
            versions.insert(format!("FulfilmentKind/{name}"), version);
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

        let generations = self.newest_generations_first(namespace, ensure).await?;
        let now = self.clock.now();
        let (reading, attention) = self.standing_reading(namespace, ensure, &generations, now, force_now).await?;
        let latest = generations.first();
        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let config_hash = ensure_config_hash(&ensure.spec)?;
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        match reading.decision {
            Decision::Running => return Ok(None),
            Decision::Held { raise } => {
                if raise {
                    let latest = latest.expect("strikes need a generation");
                    let failure = restart_limit_reason(reading.strikes);
                    self.raise_ensure_attention(ensure, latest, &failure, admitted_at(latest)).await?;
                    return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
                }
                return Ok(None);
            }
            Decision::BackOff { until } => {
                // Automatic passes require positive death evidence before admitting a successor.
                backing_inspector.verify_backing_dead(latest.expect("strikes need a generation")).await?;
                self.due_at(namespace, until);
                return Ok(None);
            }
            Decision::Admit => {}
        }
        // `reconcile-now` is the operator's explicit acknowledgement that any missing backing
        // records were deliberately wiped; a released hold admits at once.
        let released = force_now || attention.acknowledged_at.is_some();
        if attention.open {
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
        }
        if !released && reading.strikes > 0 {
            backing_inspector.verify_backing_dead(latest.expect("strikes need a generation")).await?;
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
        let generations = self.newest_generations_first(namespace, ensure).await?;
        if let Some(convoy) = generations.first().filter(|convoy| !ended(convoy)) {
            self.observe_ensure_config_drift(namespace, ensure, convoy).await?;
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            return Ok(None);
        }

        let retry_key = (namespace.to_string(), ensure.metadata.name.clone());
        let dependency_hash = self.ensure_admission_dependency_hash(namespace, ensure).await?;
        let Some(latest) = generations.first() else {
            if self.admission_waits(namespace, ensure, &retry_key, &config_hash, &dependency_hash, force_now, force_now).await? {
                return Ok(None);
            }
            return self.restart_absent_ensured_convoy(namespace, ensure, now).await;
        };
        let (reading, attention) = self.standing_reading(namespace, ensure, &generations, now, force_now).await?;
        match reading.decision {
            Decision::Running => return Ok(None),
            Decision::Held { raise } => {
                if !raise {
                    return Ok(None);
                }
                // Raised once, as the hold begins; one on its backing earlier is another episode.
                self.raise_ensure_attention(ensure, latest, &restart_limit_reason(reading.strikes), now).await?;
                return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
            }
            Decision::BackOff { .. } | Decision::Admit => {}
        }

        let operator_forced = latest.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
        // A forced pass is the recovery boundary for recordless teardown: the
        // operator has acknowledged that raw deletion destroyed the evidence.
        if !operator_forced && !force_now {
            if let Err(reason) = backing_inspector.verify_backing_dead(latest).await {
                let failure = format!("standing convoy teardown held: {reason}");
                warn!(ensure = %ensure.metadata.name, convoy = %latest.metadata.name, %failure, "standing convoy backing evidence refused");
                // The hold on a generation begins as it ends.
                let ended_at = generation(latest, &ensure.spec).ended.map_or(now, |(_, at)| at);
                self.raise_ensure_attention(ensure, latest, &failure, ended_at).await?;
                if attention.open {
                    return Ok(None);
                }
                return Ok(Some(format!("ConvoyEnsure/{} held for operator attention", ensure.metadata.name)));
            }
        }
        // The backing is verified dead: a hold on it is answered.
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
        self.clear_hold(namespace, &latest.metadata.name).await?;
        if let Decision::BackOff { until } = reading.decision {
            self.due_at(namespace, until);
            return Ok(None);
        }
        if self.admission_waits(namespace, ensure, &retry_key, &config_hash, &dependency_hash, force_now, force_now).await? {
            return Ok(None);
        }
        // The ended generation remains as history. A successful restart admits the next
        // generation under a fresh record name.
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
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
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

    /// The role's generations homed here, newest first.
    async fn newest_generations_first(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
    ) -> Result<Vec<ResourceObject<ResourceConvoy>>, String> {
        newest_generations_first(self.resource_backend, namespace, &ensure.metadata.name, false).await
    }

    async fn standing_reading(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        generations: &[ResourceObject<ResourceConvoy>],
        now: DateTime<Utc>,
        forced: bool,
    ) -> Result<(StandingReading, HoldAttention), String> {
        standing_reading(&*self.attentions, namespace, ensure, generations, now, forced).await
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
        self.attentions.raise(namespace, &Attention::new(fact, reason, admitted_at(convoy))).await
    }

    async fn raise_ensure_attention(
        &self,
        ensure: &ResourceObject<ConvoyEnsure>,
        convoy: &ResourceObject<ResourceConvoy>,
        reason: &str,
        began_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let fact = Fact::Hold { ensure: ensure.metadata.name.clone(), convoy: convoy.metadata.name.clone() };
        self.attentions.raise(&convoy.metadata.namespace, &Attention::new(fact, reason, began_at)).await
    }

    /// Withdraws the hold on `convoy` if it is open.
    async fn clear_hold(&self, namespace: &str, convoy: &str) -> Result<(), String> {
        self.attentions.clear(namespace, Sender::Standing, &Subject::convoy(convoy)).await
    }

    /// Clears the hold on any of the ensure's generations.
    async fn clear_ensure_attention(&self, namespace: &str, ensure_name: &str) -> Result<(), String> {
        let holds: Vec<_> =
            self.attentions.open(namespace, Some(Sender::Standing), None).await?.into_iter().filter(OpenAttention::is_hold).collect();
        if holds.is_empty() {
            return Ok(());
        }
        let generations: HashSet<String> =
            self.ensured_convoy_generations(namespace, ensure_name).await?.into_iter().map(|convoy| convoy.metadata.name).collect();
        for hold in holds {
            if matches!(&hold.subject, Subject::Convoy { convoy } if generations.contains(convoy)) {
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
        let latest = |generations: Vec<ResourceObject<ResourceConvoy>>| generations.into_iter().max_by_key(|convoy| convoy.spec.generation);
        // A listing that fails here aborts the start before anything is admitted; the next pass retries.
        let before = latest(self.ensured_convoy_generations(namespace, &ensure.metadata.name).await?).map(|convoy| convoy.spec.generation);
        match self.commit_ensured_convoy(namespace, &ensure, admission).await {
            Ok(name) => Ok(name),
            // A commit that fails after creating its convoy has still admitted a generation; the
            // caller records it, or its end would never count against the restart budget (#4310).
            // A generation newer than `before` is this commit's: each reconciliation pass holds the ensure
            // reconciliation lock, so no other pass admits for this ensure meanwhile.
            Err(error) => match self.ensured_convoy_generations(namespace, &ensure.metadata.name).await {
                // The listing failing too says nothing of the commit; its own error is the one to report.
                Err(listing) => Err(format!("{error}; and the generations could not be listed after it: {listing}")),
                Ok(generations) => match latest(generations).filter(|convoy| before.is_none_or(|before| convoy.spec.generation > before)) {
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

/// The role `ensure`'s generations homed here, or at every home with `everywhere`, newest first.
async fn newest_generations_first(
    backend: &ResourceBackend,
    namespace: &str,
    ensure: &str,
    everywhere: bool,
) -> Result<Vec<ResourceObject<ResourceConvoy>>, String> {
    let mut generations = backend
        .using::<ResourceConvoy>(namespace)
        .query_declared(&StoreQuery {
            fields: BTreeMap::from([(format!("metadata.annotations.\"{ENSURED_FROM_ANNOTATION}\""), ensure.into())]),
            include_replicas: everywhere,
            ..Default::default()
        })
        .await
        .map(|rows| rows.items.into_iter().map(|source| source.object).collect::<Vec<_>>())
        .map_err(|error| error.to_string())?;
    generations.sort_by_key(|convoy| std::cmp::Reverse(convoy.spec.generation));
    Ok(generations)
}

/// The role's strike run and decision ([`standing::derive`]) from `generations`, newest first,
/// and the hold attention about them: whether one is open on the newest, and when the newest
/// generation a person acknowledged a hold on was admitted. The acknowledgement ends the run, so
/// the walk stops at the first generation that is no strike.
async fn standing_reading(
    attentions: &dyn Attentions,
    namespace: &str,
    ensure: &ResourceObject<ConvoyEnsure>,
    generations: &[ResourceObject<ResourceConvoy>],
    now: DateTime<Utc>,
    forced: bool,
) -> Result<(StandingReading, HoldAttention), String> {
    let read: Vec<Generation> = generations.iter().map(|convoy| generation(convoy, &ensure.spec)).collect();
    let mut attention = HoldAttention::default();
    if let Some(newest) = generations.first() {
        // A standing role's attention about a convoy is its hold.
        let subject = Subject::convoy(newest.metadata.name.clone());
        attention.open = !attentions.open(namespace, Some(Sender::Standing), Some(&subject)).await?.is_empty();
    }
    for (convoy, generation) in generations.iter().zip(&read) {
        let Some((Ending::Failed, finished_at)) = generation.ended.filter(|_| generation.current_config) else { break };
        // A hold on a generation begins once it has ended.
        let subject = Subject::convoy(convoy.metadata.name.clone());
        if attentions.acknowledged(namespace, Sender::Standing, &subject, finished_at).await?.is_some() {
            attention.acknowledged_at = Some(generation.admitted_at);
            break;
        }
    }
    Ok((standing::derive(&read, attention, now, forced), attention))
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
    use flotilla_protocol::CanonicalHostId;
    use flotilla_resources::ConvoySpec;
    use flotilla_resources::ConvoyStatus;
    use flotilla_resources::ProjectSpec;
    use flotilla_resources::WorkflowTemplateSpec;
    use flotilla_store_memory::InMemoryBackend;
    use flotilla_store_testkit::VirtualClock;

    use super::*;
    use flotilla_engine_messages_testkit::{designate_fleet, EngineMessagesHome};
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
        /// The engine Message path its attentions are raised on, kept open for the test.
        #[builder(skip)]
        _messages: Option<Arc<EngineMessagesHome>>,
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
                .update_status(
                    &name,
                    &convoy.metadata.resource_version,
                    &ConvoyStatus { ensure_admission: Some(ensure.spec.clone()), ..Default::default() },
                )
                .await
                .map_err(|e| e.to_string())?;
            if self.fails_after_create.load(Ordering::SeqCst) {
                return Err("status patch retry budget exhausted".into());
            }
            Ok(name)
        }
    }

    #[async_trait]
    impl ConvoyTeardown for Admission {
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

    // Both live scopes and local generation history decode only their indexed matches.
    #[tokio::test]
    async fn ensured_convoys_read_only_matching_rows() {
        use flotilla_store_testkit::{measure_reads, ReadCountsBackendExt};
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
        let convoys = backend.using::<ResourceConvoy>("test");
        for index in 0..312 {
            let name = format!("c-{index}");
            let created = convoys
                .create(
                    &InputMeta::builder()
                        .name(name.clone())
                        .annotations(BTreeMap::from([(
                            ENSURED_FROM_ANNOTATION.into(),
                            if index == 0 || index >= 310 { "wanted" } else { "other" }.into(),
                        )]))
                        .build(),
                    &ConvoySpec::builder().workflow_ref("workflow".into()).build(),
                )
                .await
                .expect("convoy");
            if index != 311 {
                convoys
                    .update_status(
                        &name,
                        &created.metadata.resource_version,
                        &ConvoyStatus { phase: if index < 300 { ConvoyPhase::Landed } else { ConvoyPhase::Active }, ..Default::default() },
                    )
                    .await
                    .expect("status");
            }
        }
        let mut replica = convoys.list().await.expect("local fixture");
        for object in &mut replica.items {
            object.metadata.name = format!("replica-{}", object.metadata.name);
        }
        backend
            .replica_writer::<ResourceConvoy>(flotilla_protocol::NodeId::new("remote"), "test")
            .replace(&replica, Utc::now())
            .await
            .expect("replicas");
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let controller = EnsureReconciler::builder()
            .teardown(Arc::new(Admission::builder().backend(backend.clone()).build()))
            .resource_backend(backend.clone())
            .clock(clock)
            .manifest_roots(Arc::new(flotilla_store_testkit::OldStoreManifestRoots::new(backend.clone())))
            .build();
        let admission = Admission::builder().backend(backend.clone()).build();
        for scope in [EnsureConvoyScope::Local, EnsureConvoyScope::IncludingReplicas] {
            let (rows, reads) = measure_reads(controller.pass(&admission).active_ensured_convoys("test", "wanted", scope)).await;
            let expected = if matches!(scope, EnsureConvoyScope::Local) { 2 } else { 4 };
            assert_eq!(rows.expect("query").len(), expected);
            assert_eq!(reads.of(&backend).objects_by_kind().get("Convoy"), Some(&expected));
        }
        let (rows, reads) =
            measure_reads(controller.pass(&admission).active_ensured_convoys("test", "absent", EnsureConvoyScope::IncludingReplicas)).await;
        assert!(rows.expect("empty query").is_empty());
        assert_eq!(reads.of(&backend).objects_by_kind().get("Convoy"), Some(&0));
        let (rows, reads) = measure_reads(controller.pass(&admission).ensured_convoy_generations("test", "wanted")).await;
        assert_eq!(rows.expect("local generations").len(), 3);
        assert_eq!(reads.of(&backend).objects_by_kind().get("Convoy"), Some(&3));
    }

    async fn fixture() -> (EnsureReconciler, Admission, Arc<VirtualClock>) {
        let backend = ResourceBackend::new(InMemoryBackend::default());
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
        designate_fleet(&backend, "test").await;
        let messages = EngineMessagesHome::open_with_clock("home", "test", &backend, clock.clone());
        let admission = Admission { _messages: Some(Arc::new(messages)), ..Admission::builder().backend(backend.clone()).build() };
        let controller = EnsureReconciler::builder()
            .teardown(Arc::new(admission.clone()))
            .resource_backend(backend.clone())
            .clock(clock.clone())
            .manifest_roots(Arc::new(flotilla_store_testkit::OldStoreManifestRoots::new(backend.clone())))
            .build();
        (controller, admission, clock)
    }

    /// Whether the standing role's hold on its convoy is open.
    async fn hold_open(admission: &Admission) -> bool {
        let open = admission.backend.attentions().open("test", Some(Sender::Standing), None).await.expect("attentions");
        open.iter().any(OpenAttention::is_hold)
    }
    /// The role's newest generation.
    fn current(admission: &Admission) -> String {
        format!("generation-{}", admission.commits.load(Ordering::SeqCst))
    }
    async fn standing(controller: &EnsureReconciler) -> StandingRole {
        controller.standing_role("test", "project", "standing").await.expect("role reads").expect("role declared")
    }
    async fn fail_current(admission: &Admission, clock: &VirtualClock) {
        let name = current(admission);
        let convoys = admission.backend.using::<ResourceConvoy>("test");
        let convoy = convoys.get(&name).await.expect("admitted convoy");
        let mut status = convoy.status.expect("resource status");
        status.phase = ConvoyPhase::Failed;
        status.finished_at = Some(clock.now());
        convoys.update_status(&name, &convoy.metadata.resource_version, &status).await.expect("fixture operation succeeds");
    }

    // #2875: a long-lived driver's generation must back off from its failure,
    // not its old creation time; reconstructing the controller retains the deadline.
    #[tokio::test]
    async fn driver_readmission_backs_off_from_failure_time() {
        let (controller, admission, clock) = fixture().await;
        controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead).await.expect("admit");
        fail_current(&admission, &clock).await;
        let ensures = admission.backend.using::<ConvoyEnsure>("test");
        let convoys = admission.backend.using::<ResourceConvoy>("test");
        let name = &current(&admission);
        let failed = convoys.get(name).await.expect("failed convoy");
        clock.advance(failed.metadata.creation_timestamp + ChronoDuration::hours(24) - clock.now());
        let mut status = failed.status.expect("status");
        status.finished_at = Some(clock.now());
        convoys.update_status(name, &failed.metadata.resource_version, &status).await.expect("failure time");
        for _ in 0..3 {
            let controller = EnsureReconciler::builder()
                .teardown(Arc::new(admission.clone()))
                .resource_backend(admission.backend.clone())
                .clock(clock.clone())
                .manifest_roots(Arc::new(flotilla_store_testkit::OldStoreManifestRoots::new(admission.backend.clone())))
                .build();
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
        let controller = EnsureReconciler::builder()
            .teardown(Arc::new(admission.clone()))
            .resource_backend(admission.backend.clone())
            .clock(clock.clone())
            .manifest_roots(Arc::new(flotilla_store_testkit::OldStoreManifestRoots::new(admission.backend.clone())))
            .build();
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
        assert_eq!(role.convoy.as_deref(), Some("generation-1"));
        assert_eq!(admission.commits.load(Ordering::SeqCst), 1);
    }

    // #3108: refusals with the same hashes stop retrying on time at LOCAL_RETRY_LIMIT and tell a
    // person once. Reconcile-now and an acknowledgement each admit once more; a changed declaration
    // withdraws the attention and resets the count; an admission ends both.
    #[tokio::test]
    async fn refusals_past_the_limit_wait_for_events_with_one_attention() {
        let (controller, admission, clock) = fixture().await;
        let messages = admission._messages.clone().expect("engine messages");
        let refusal_messages = || {
            messages
                .primitives()
                .messages()
                .expect("messages")
                .into_iter()
                .filter(|(_, message)| {
                    message.spec().subject().is_some_and(|subject| subject.address.as_str() == "project:project/role:standing")
                })
                .count()
        };
        let pass = || controller.reconcile_convoy_ensures_once_with_backing_inspector(&admission, "test", &Backing::Dead);
        admission.refused.store(true, Ordering::SeqCst);
        for _ in 0..LOCAL_RETRY_LIMIT {
            assert!(pass().await.is_err());
            clock.advance(ChronoDuration::minutes(16));
        }
        assert_eq!(admission.prepares.load(Ordering::SeqCst), LOCAL_RETRY_LIMIT);
        assert_eq!(refusal_messages(), 1, "the limit tells a person");
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
        assert_eq!(refusal_messages(), 1, "repeated refusals with the same hashes are one message");

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
        assert_eq!(refusal_messages(), 2);

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
        assert_eq!(standing(&controller).await.convoy.as_deref(), Some("generation-1"));
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
        fail_current(&admission, &clock).await;
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
            fail_current(&admission, &clock).await;
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
        fail_current(&admission, &clock).await;
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
