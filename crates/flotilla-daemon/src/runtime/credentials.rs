//! Work-credential delivery, refresh, and agent-environment composition.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_core::{
    crew_capabilities::{CredentialCapability, SessionCapabilitySource},
    in_process::{InProcessDaemon, WorkCredentialReconciler},
    providers::{discovery::EnvVars, CommandRunner},
};
use flotilla_credentials::{
    codex_central_auth_path, compose_agent_environment, AgentMaterialRegistry, CodexCentralRefresher, CredentialRefreshError,
    CredentialStore, GithubAppScope,
};
use flotilla_resources::Resource;
use flotilla_resources::{
    ControllerRetry, ControllerRetryDisposition, Convoy, Demand, DemandKind, DemandSpec, Environment, EnvironmentPhase,
    EnvironmentStatusPatch, InputMeta, Repository, RepositoryTrust, ResourceBackend, ResourceError, RetryBackoff, TerminalSession,
    TerminalSessionSource, Vessel, VesselStatusPatch,
};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::state::ControllerRuntimeState;
use super::tasks::{spawn_periodic_task, PeriodicTaskStart};

/// Staging and launch resolve the same adapter defaults, delivered values, and
/// explicit home overrides. Selection metadata is carried only for staging.
pub(super) fn agent_material_environment(
    material: &AgentMaterialRegistry,
    required_adapters: &BTreeSet<String>,
    declared: &BTreeMap<String, String>,
    delivered: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<(String, String)>, String> {
    let mut environment = compose_agent_environment(material.fragments(required_adapters, declared))?.environment;
    for (key, value) in delivered {
        environment.retain(|(existing, _)| existing != &key);
        environment.push((key, value));
    }
    for key in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        if let Some(value) = declared.get(key) {
            environment.retain(|(existing, _)| existing != key);
            environment.push((key.into(), value.clone()));
        }
    }
    Ok(environment)
}

pub(super) async fn stage_agent_environment(runner: &dyn CommandRunner, fallback: &Path, contents: &str) -> Result<PathBuf, String> {
    let config_base =
        runner.writable_config_base(None, fallback).await.map_err(|error| format!("resolve agent environment path: {error}"))?;
    let path = config_base.join("agent-environment");
    runner.write_file(&path, contents).await.map_err(|error| format!("stage composed agent environment: {error}"))?;
    Ok(path)
}

pub(super) struct RuntimeSessionCapabilities {
    pub(super) state: Weak<ControllerRuntimeState>,
}

#[async_trait]
impl SessionCapabilitySource for RuntimeSessionCapabilities {
    async fn credentials(&self, environment: &str, references: &BTreeSet<String>) -> Result<Vec<CredentialCapability>, String> {
        let state = self.state.upgrade().ok_or("session capability runtime unavailable")?;
        match &state.credential_store {
            Some(store) => store.credentials(environment, references).await,
            None => Ok(Vec::new()),
        }
    }

    async fn endpoints(&self, environment: &str, session: &str) -> Result<BTreeMap<String, String>, String> {
        let state = self.state.upgrade().ok_or("session capability runtime unavailable")?;
        let mut endpoints = match &state.credential_store {
            Some(store) => store.endpoints(environment, session).await?,
            None => BTreeMap::new(),
        };
        if environment == state.host_direct_environment_name {
            if let Some(socket) = state.daemon.daemon_socket_path().await {
                endpoints.insert("Flotilla daemon".into(), socket.display().to_string());
            }
        } else {
            let record = state
                .daemon
                .resource_backend()
                .including_replicas::<Environment>(&state.namespace)
                .get(environment)
                .await
                .map_err(|error| error.to_string())?
                .object;
            if let Some(docker) = record.spec.docker {
                for (key, value) in &docker.env {
                    if let Some((destination, address)) = flotilla_core::crew_capabilities::endpoint_for_env(key, value) {
                        endpoints.insert(destination, address);
                    }
                }
            }
        }
        Ok(endpoints)
    }
}

pub(super) struct RuntimeWorkCredentialReconciler {
    pub(super) state: Weak<ControllerRuntimeState>,
}

#[async_trait]
impl WorkCredentialReconciler for RuntimeWorkCredentialReconciler {
    async fn reconcile(&self, namespace: &str, environment_ref: &str) -> Result<(), String> {
        let state = self.state.upgrade().ok_or_else(|| "credential controller is unavailable".to_string())?;
        reconcile_work_credentials_for_environment(&state, namespace, environment_ref).await
    }

    async fn ledger_delivery_environment(&self, namespace: &str, environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        let state = self.state.upgrade().ok_or_else(|| "credential controller is unavailable".to_string())?;
        reconcile_work_credentials_for_environment(&state, namespace, environment_ref).await?;
        let store = state.credential_store.as_ref().ok_or("credential store is unavailable")?;
        store.ledger_delivery_environment(environment_ref).await
    }
}

/// Keep crew credentials staged for the lifetime of their terminal sessions.
pub(super) async fn reconcile_work_credentials(state: &ControllerRuntimeState, namespace: &str) -> Result<(), String> {
    reconcile_work_credentials_filtered(state, namespace, None).await
}

pub(super) async fn reconcile_work_credentials_for_environment(
    state: &ControllerRuntimeState,
    namespace: &str,
    environment_ref: &str,
) -> Result<(), String> {
    reconcile_work_credentials_filtered(state, namespace, Some(environment_ref)).await
}

pub(super) async fn reconcile_work_credentials_filtered(
    state: &ControllerRuntimeState,
    namespace: &str,
    target_environment: Option<&str>,
) -> Result<(), String> {
    let Some(store) = &state.credential_store else { return Ok(()) };
    let backend = state.daemon.resource_backend();
    let vessels = backend.using::<Vessel>(namespace).list().await.map_err(|error| format!("list credential vessels: {error}"))?;
    let vessels = vessels
        .items
        .into_iter()
        .filter(|vessel| {
            target_environment
                .is_none_or(|target| vessel.status.as_ref().and_then(|status| status.environment_ref.as_deref()) == Some(target))
        })
        .collect::<Vec<_>>();
    let live_crew_vessels = backend
        .using::<TerminalSession>(namespace)
        .list()
        .await
        .map_err(|error| format!("list crew terminal sessions for credential delivery: {error}"))?
        .items
        .into_iter()
        .filter_map(|session| {
            let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return None };
            if context.namespace != namespace
                || !session.status.as_ref().is_none_or(|status| {
                    matches!(
                        status.phase,
                        flotilla_resources::TerminalSessionPhase::Starting | flotilla_resources::TerminalSessionPhase::Running
                    )
                })
            {
                return None;
            }
            Some((context.vessel_ref.clone(), context.convoy.clone(), session.spec.env_ref.clone()))
        })
        .collect::<BTreeSet<_>>();
    let convoys = if target_environment.is_some() {
        let mut convoys = BTreeMap::new();
        for name in vessels.iter().map(|vessel| &vessel.spec.convoy_ref).collect::<BTreeSet<_>>() {
            let convoy = backend
                .including_replicas::<Convoy>(namespace)
                .get(name)
                .await
                .map_err(|error| format!("read credential convoy {name}: {error}"))?
                .object;
            convoys.insert(name.clone(), convoy);
        }
        convoys
    } else {
        backend
            .including_replicas::<Convoy>(namespace)
            .list()
            .await
            .map_err(|error| format!("list credential convoys: {error}"))?
            .items
            .into_iter()
            .map(|source| (source.object.metadata.name.clone(), source.object))
            .collect::<BTreeMap<_, _>>()
    };
    let needs_grants = vessels.iter().any(|vessel| {
        convoys
            .get(&vessel.spec.convoy_ref)
            .and_then(|convoy| convoy.status.as_ref())
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == vessel.spec.vessel_name))
            .is_some_and(|requirement| !requirement.credential_scopes.is_empty())
    });
    let (grants, repository_trust) = if needs_grants {
        let grants = backend
            .including_replicas::<flotilla_resources::CredentialGrant>(namespace)
            .list()
            .await
            .map_err(|error| format!("list credential grants: {error}"))?
            .items
            .into_iter()
            .map(|grant| grant.object.spec)
            .collect::<Vec<_>>();
        let repository_trust = backend
            .including_replicas::<Repository>(namespace)
            .list()
            .await
            .map_err(|error| format!("list repositories for credential grants: {error}"))?
            .items
            .into_iter()
            .map(|source| {
                (
                    flotilla_resources::RepositoryKey(source.object.metadata.name),
                    if source.object.spec.is_fork() { RepositoryTrust::Fork } else { RepositoryTrust::Own },
                )
            })
            .collect::<BTreeMap<_, _>>();
        (grants, repository_trust)
    } else {
        (Vec::new(), BTreeMap::new())
    };
    type Delivery = (
        BTreeSet<String>,
        BTreeSet<String>,
        BTreeMap<String, BTreeSet<flotilla_resources::RepositoryKey>>,
        BTreeMap<String, GithubAppScope>,
        BTreeMap<String, Option<BTreeMap<String, String>>>,
    );
    let mut deliveries = BTreeMap::<String, Delivery>::new();
    let mut permission_conflicts = BTreeMap::<String, String>::new();
    for vessel in vessels {
        let Some(environment_ref) = vessel.status.as_ref().and_then(|status| status.environment_ref.as_ref()) else { continue };
        let Some(convoy) = convoys.get(&vessel.spec.convoy_ref) else { continue };
        let Some(status) = &convoy.status else { continue };
        let Some(requirement) = status
            .workflow_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == vessel.spec.vessel_name))
        else {
            continue;
        };
        let live = live_crew_vessels.contains(&(vessel.metadata.name.clone(), vessel.spec.convoy_ref.clone(), environment_ref.clone()));
        let (granted, running, scopes, live_scopes, permissions) = deliveries.entry(environment_ref.clone()).or_default();
        granted.extend(requirement.credential_refs.iter().cloned());
        if live {
            running.extend(requirement.credential_refs.iter().cloned());
            for name in &requirement.credential_refs {
                let incoming = requirement.credential_permissions.get(name).cloned();
                if let Some(existing) = permissions.get(name) {
                    if existing != &incoming {
                        permission_conflicts.insert(
                            environment_ref.clone(),
                            format!("crews sharing environment `{environment_ref}` require different minted permissions for `{name}`"),
                        );
                    }
                } else {
                    permissions.insert(name.clone(), incoming);
                }
            }
            for (name, repositories) in &requirement.credential_scopes {
                scopes.entry(name.clone()).or_default().extend(repositories.iter().cloned());
            }
            if requirement.credential_scopes.is_empty() {
                continue;
            }
            for (name, repositories) in &requirement.credential_scopes {
                let mut scope = github_app_scope_from_grants(
                    name,
                    repositories,
                    convoy.spec.project_ref.as_deref(),
                    &requirement.crew.iter().map(|crew| crew.role.clone()).collect(),
                    &repository_trust,
                    requirement.repository_refs.is_some(),
                    &grants,
                );
                scope.permissions = requirement.credential_permissions.get(name).cloned();
                let entry = live_scopes.entry(name.clone()).or_default();
                entry.fixed_repositories.extend(scope.fixed_repositories);
                entry.projects.extend(scope.projects);
                entry.permissions = scope.permissions;
            }
        }
    }
    let current_environments = deliveries.keys().cloned().collect::<BTreeSet<_>>();
    for (environment_ref, previously_delivered) in store.tracked_work_deliveries().await {
        if target_environment.is_some_and(|target| environment_ref != target) {
            continue;
        }
        deliveries.entry(environment_ref).or_default().0.extend(previously_delivered);
    }
    let mut errors = Vec::new();
    for (environment_ref, (granted, running, scopes, live_scopes, permissions)) in deliveries {
        if let Some(error) = permission_conflicts.remove(&environment_ref) {
            record_credential_delivery_retry(&backend, namespace, &environment_ref, Some(error.clone()), false).await?;
            errors.push(error);
            continue;
        }
        let permissions = permissions.into_iter().filter_map(|(name, value)| value.map(|value| (name, value))).collect();
        if granted.is_empty() {
            record_credential_delivery_retry(&backend, namespace, &environment_ref, None, false).await?;
            continue;
        }
        if let Ok(environment) = backend.clone().using::<Environment>(namespace).get(&environment_ref).await {
            if let Some(retry) = environment.status.as_ref().and_then(|status| status.credential_delivery_retry.as_ref()) {
                match retry.disposition {
                    ControllerRetryDisposition::Terminal { .. } => {
                        mirror_credential_retry_to_vessels(&backend, namespace, &environment_ref, Some(retry.clone()), false).await?;
                        continue;
                    }
                    ControllerRetryDisposition::Retryable { next_attempt_at } if Utc::now() < next_attempt_at => continue,
                    _ => {}
                }
            }
        }
        if let Some(error) = credential_quarantine_need(&backend, namespace, &granted).await? {
            record_credential_delivery_retry(&backend, namespace, &environment_ref, Some(error.clone()), true).await?;
            errors.push(error);
            continue;
        }
        let result = async {
            let runner = match state.daemon.command_runner_for_environment_ref(&environment_ref) {
                Some(runner) => runner,
                None if !current_environments.contains(&environment_ref) => {
                    // The vessel and its contained filesystem are gone. Clear
                    // refresh registrations and cached material as well.
                    store.forget_environment(&environment_ref).await?;
                    return Ok(());
                }
                None => return Err(format!("command runner unavailable for credential delivery to environment {environment_ref}")),
            };
            if !running.is_empty() {
                store
                    .adopt_github_app_deliveries_with_permissions(&environment_ref, &running, &scopes, &permissions, runner.clone())
                    .await
                    .map_err(|error| format!("mint work credentials for environment {environment_ref}: {}", error.message))?;
                store.set_github_app_scopes(&environment_ref, &live_scopes).await;
            }
            store
                .reconcile_work_delivery_with_permissions(&environment_ref, &granted, &running, &scopes, &permissions, runner)
                .await
                .map_err(|error| format!("reconcile work credentials for environment {environment_ref}: {error}"))
        }
        .await;
        if let Err(error) = result {
            record_credential_delivery_retry(&backend, namespace, &environment_ref, Some(error.clone()), false).await?;
            errors.push(error);
        } else {
            record_credential_delivery_retry(&backend, namespace, &environment_ref, None, false).await?;
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(super) async fn credential_quarantine_need(
    backend: &ResourceBackend,
    namespace: &str,
    granted: &BTreeSet<String>,
) -> Result<Option<String>, String> {
    Ok(backend.diagnostics().await.map_err(|error| format!("inspect credential declaration quarantine: {error}"))?.and_then(
        |diagnostics| {
            diagnostics
                .decode_quarantines
                .iter()
                .find(|quarantine| {
                    quarantine.kind == "CredentialSpec" && quarantine.namespace == namespace && granted.contains(&quarantine.name)
                })
                .map(|quarantine| format!("credential spec `{}` does not decode: {}", quarantine.name, quarantine.error))
        },
    ))
}

pub(super) async fn record_credential_delivery_retry(
    backend: &ResourceBackend,
    namespace: &str,
    environment_ref: &str,
    error: Option<String>,
    terminal: bool,
) -> Result<(), String> {
    let environments = backend.clone().using::<Environment>(namespace);
    let environment = match environments.get(environment_ref).await {
        Ok(environment) => environment,
        Err(ResourceError::NotFound { .. }) => return Ok(()),
        Err(error) => return Err(format!("read credential delivery environment: {error}")),
    };
    let previous = environment.status.as_ref().and_then(|status| status.credential_delivery_retry.as_ref());
    if !terminal
        && error.is_some()
        && previous.is_some_and(
            |retry| matches!(retry.disposition, ControllerRetryDisposition::Retryable { next_attempt_at } if Utc::now() < next_attempt_at),
        )
    {
        mirror_credential_retry_to_vessels(backend, namespace, environment_ref, previous.cloned(), false).await?;
        return Ok(());
    }
    let retry = error.map(|error| {
        if terminal {
            ControllerRetry::terminal(previous, Utc::now(), error)
        } else {
            ControllerRetry::retryable(
                previous,
                Utc::now(),
                RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) },
            )
        }
    });
    if previous == retry.as_ref() {
        mirror_credential_retry_to_vessels(backend, namespace, environment_ref, retry, false).await?;
        return Ok(());
    }
    flotilla_resources::apply_status_patch(
        &environments,
        environment_ref,
        &EnvironmentStatusPatch::CredentialDelivery { retry: retry.clone() },
    )
    .await
    .map_err(|error| format!("record credential delivery disposition: {error}"))?;
    mirror_credential_retry_to_vessels(backend, namespace, environment_ref, retry, false).await?;
    Ok(())
}

pub(super) async fn mirror_credential_retry_to_vessels(
    backend: &ResourceBackend,
    namespace: &str,
    environment_ref: &str,
    retry: Option<ControllerRetry>,
    refresh: bool,
) -> Result<(), String> {
    let vessels = backend.clone().using::<Vessel>(namespace);
    for vessel in vessels.list().await.map_err(|error| format!("list credential delivery vessels: {error}"))?.items {
        let Some(status) = vessel.status.as_ref().filter(|status| status.environment_ref.as_deref() == Some(environment_ref)) else {
            continue;
        };
        let current = if refresh { &status.credential_refresh_retry } else { &status.credential_delivery_retry };
        if current == &retry {
            continue;
        }
        let patch = if refresh {
            VesselStatusPatch::CredentialRefresh { retry: retry.clone() }
        } else {
            VesselStatusPatch::CredentialDelivery { retry: retry.clone() }
        };
        flotilla_resources::apply_status_patch(&vessels, &vessel.metadata.name, &patch)
            .await
            .map_err(|error| format!("record vessel credential disposition: {error}"))?;
    }
    Ok(())
}

pub(super) fn github_app_scope_from_grants(
    credential: &str,
    repositories: &BTreeSet<flotilla_resources::RepositoryKey>,
    project: Option<&str>,
    roles: &BTreeSet<String>,
    repository_trust: &BTreeMap<flotilla_resources::RepositoryKey, RepositoryTrust>,
    vessel_repositories_pinned: bool,
    grants: &[flotilla_resources::CredentialGrantSpec],
) -> GithubAppScope {
    let mut scope = GithubAppScope::default();
    let mut matched = false;
    for grant in grants {
        let selected = repositories.iter().filter_map(|key| repository_trust.get(key).map(|trust| (key.clone(), *trust))).collect();
        let matches = (roles.is_empty() && grant.selector.roles.is_empty() && grant.selector.matches(project, &selected, ""))
            || roles.iter().any(|role| grant.selector.matches(project, &selected, role));
        if !grant.credentials.contains(credential) || !matches {
            continue;
        }
        matched = true;
        if !grant.selector.projects.is_empty()
            && grant.selector.repositories.is_empty()
            && grant.selector.repository_trust.is_none()
            && !vessel_repositories_pinned
        {
            if let Some(project) = project {
                scope.projects.insert(project.to_string());
            }
        } else if grant.selector.repositories.is_empty() {
            scope.fixed_repositories.extend(repositories.iter().cloned());
        } else {
            scope.fixed_repositories.extend(grant.selector.repositories.intersection(repositories).cloned());
        }
    }
    if !matched {
        scope.fixed_repositories.extend(repositories.iter().cloned());
    }
    scope
}

/// Credential rotation must keep running even when a host heartbeat is slow.
pub(super) fn spawn_credential_refresh_task(
    daemon: Arc<InProcessDaemon>,
    namespace: String,
    store: Arc<CredentialStore>,
) -> JoinHandle<()> {
    spawn_periodic_task(Duration::from_secs(30), PeriodicTaskStart::Immediate, move || {
        let daemon = Arc::clone(&daemon);
        let namespace = namespace.clone();
        let store = Arc::clone(&store);
        async move {
            let errors = store.refresh_due_github_app_tokens().await;
            if let Err(error) = daemon.refresh_capability_cards(&namespace).await {
                warn!(%error, "failed to refresh session capability cards");
            }
            for error in &errors {
                warn!(error = %error.message, environment = %error.environment_ref, "failed to refresh GitHub App credential delivery");
            }
            if let Err(status_error) = reconcile_credential_refresh_attention(&daemon, &namespace, &errors).await {
                warn!(%status_error, "failed to reconcile credential refresh attention");
            }
        }
    })
}

/// Keeps this host's central Codex `auth.json` fresh forever by direct
/// OAuth `grant_type=refresh_token` — the primitive `scripts/codex-token-refresh`
/// also implements — against the well-known path from
/// [`codex_central_auth_path`]. This task is the file's sole writer — crews
/// receive read-only *copies* of it (`crates/flotilla-daemon/src/agent_material.rs`),
/// never the file itself — which matters because Codex refresh tokens are
/// single-use and two refreshers would invalidate each other's rotation.
pub(super) fn spawn_codex_central_refresh_task(env: Arc<dyn EnvVars>, interval: Duration) -> JoinHandle<()> {
    let refresher = Arc::new(CodexCentralRefresher::new(codex_central_auth_path(&*env), &*env));
    spawn_periodic_task(interval, PeriodicTaskStart::Immediate, move || {
        let refresher = Arc::clone(&refresher);
        async move {
            match refresher.refresh_once().await {
                Ok(success) => {
                    info!(
                        rotated = ?success.rotated_fields,
                        access_token_expires_at = ?success.access_token_expires_at,
                        "refreshed central Codex credential"
                    );
                }
                Err(error) => {
                    warn!(%error, "failed to refresh central Codex credential; retrying next tick");
                }
            }
        }
    })
}

pub(super) const CREDENTIAL_REFRESH_REASON_ANNOTATION: &str = "flotilla.work/credential-refresh-reason";

pub(super) const CREDENTIAL_REFRESH_VESSEL_ANNOTATION: &str = "flotilla.work/credential-refresh-vessel";

pub(super) async fn reconcile_credential_refresh_attention(
    daemon: &InProcessDaemon,
    namespace: &str,
    errors: &[CredentialRefreshError],
) -> Result<(), String> {
    let backend = daemon.resource_backend();
    record_credential_refresh_dispositions(&backend, namespace, errors).await?;
    let demands = backend.using::<Demand>(namespace);
    let existing = demands.list().await.map_err(|error| error.to_string())?;
    if errors.is_empty()
        && existing.items.iter().all(|demand| !demand.metadata.annotations.contains_key(CREDENTIAL_REFRESH_REASON_ANNOTATION))
    {
        return Ok(());
    }
    let vessels = backend.using::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?;
    let convoys = backend.including_replicas::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?;
    let convoys = convoys.items.into_iter().map(|source| (source.object.metadata.name.clone(), source.object)).collect::<BTreeMap<_, _>>();
    let mut desired = BTreeMap::new();
    let mut still_failing = BTreeSet::new();
    for error in errors {
        let Some(credential) = error.credential_name.as_ref() else { continue };
        for vessel in &vessels.items {
            if vessel.status.as_ref().and_then(|status| status.environment_ref.as_ref()) != Some(&error.environment_ref) {
                continue;
            }
            let Some(convoy) = convoys.get(&vessel.spec.convoy_ref) else { continue };
            let holds_credential = convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).is_some_and(|snapshot| {
                snapshot
                    .vessels
                    .iter()
                    .any(|requirement| requirement.name == vessel.spec.vessel_name && requirement.credential_refs.contains(credential))
            });
            if !holds_credential {
                continue;
            }
            let name = format!("credential-refresh-{}-{credential}", vessel.metadata.name);
            still_failing.insert(name.clone());
            if !error.should_surface {
                continue;
            }
            let target = flotilla_protocol::ResourceRef::new(
                flotilla_resources::api_version(Convoy::API_PATHS),
                Convoy::API_PATHS.kind,
                namespace,
                &convoy.metadata.name,
            );
            let spec = DemandSpec::for_dispatching_principal(target, DemandKind::HumanGate, convoy.spec.dispatching_principal_ref.clone());
            let meta = InputMeta::builder()
                .name(name.clone())
                .annotations(BTreeMap::from([
                    (CREDENTIAL_REFRESH_REASON_ANNOTATION.to_string(), error.message.clone()),
                    (CREDENTIAL_REFRESH_VESSEL_ANNOTATION.to_string(), vessel.spec.vessel_name.clone()),
                ]))
                .build();
            desired.insert(name, (meta, spec));
        }
    }
    for demand in existing.items {
        if !demand.metadata.annotations.contains_key(CREDENTIAL_REFRESH_REASON_ANNOTATION) {
            continue;
        }
        if let Some((meta, spec)) = desired.remove(&demand.metadata.name) {
            if !matches!(
                demand.status.as_ref().map_or(flotilla_resources::DemandState::Raised, |status| status.state),
                flotilla_resources::DemandState::Raised | flotilla_resources::DemandState::Escalated
            ) {
                demands.delete(&demand.metadata.name).await.map_err(|error| error.to_string())?;
                demands.create(&meta, &spec).await.map_err(|error| error.to_string())?;
            } else if demand.metadata.annotations != meta.annotations || demand.spec != spec {
                demands.update(&meta, &demand.metadata.resource_version, &spec).await.map_err(|error| error.to_string())?;
            }
        } else if !still_failing.contains(&demand.metadata.name) {
            demands.delete(&demand.metadata.name).await.map_err(|error| error.to_string())?;
        }
    }
    for (_, (meta, spec)) in desired {
        demands.create(&meta, &spec).await.map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(super) async fn record_credential_refresh_dispositions(
    backend: &ResourceBackend,
    namespace: &str,
    errors: &[CredentialRefreshError],
) -> Result<(), String> {
    let environments = backend.clone().using::<Environment>(namespace);
    let listed = environments.list().await.map_err(|error| format!("list credential refresh environments: {error}"))?;
    for environment in listed.items {
        let previous = environment.status.as_ref().and_then(|status| status.credential_refresh_retry.as_ref());
        let error = errors.iter().find(|error| error.environment_ref == environment.metadata.name);
        if error.is_some()
            && previous.is_some_and(|retry| {
                matches!(retry.disposition, ControllerRetryDisposition::Retryable { next_attempt_at } if Utc::now() < next_attempt_at)
            })
        {
            continue;
        }
        let retry = error.map(|_| {
            ControllerRetry::retryable(
                previous,
                Utc::now(),
                RetryBackoff { initial: Duration::from_secs(30), maximum: Duration::from_secs(120) },
            )
        });
        if previous == retry.as_ref() {
            continue;
        }
        flotilla_resources::apply_status_patch(
            &environments,
            &environment.metadata.name,
            &EnvironmentStatusPatch::CredentialRefresh { retry: retry.clone() },
        )
        .await
        .map_err(|error| format!("record credential refresh disposition: {error}"))?;
        mirror_credential_retry_to_vessels(backend, namespace, &environment.metadata.name, retry, true).await?;
    }
    Ok(())
}

/// Keeps every live Codex crew's read-only `auth.json` copy current with the
/// central login this host refreshes.
///
/// The credential is static, so there is nothing to lease, queue for, or
/// release — but a vessel can outlive the access token it was provisioned with,
/// and a crew holding a read-only copy cannot refresh for itself
/// (flotilla-org/flotilla#1906). Re-copying on the controller resync interval is
/// what keeps "crews never refresh" true for a long-lived convoy, and replaces
/// the reacquire-on-resume step the retired lease loop used to provide.
pub(super) fn spawn_codex_credential_redelivery_task(
    state: Arc<ControllerRuntimeState>,
    namespace: String,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let state = Arc::clone(&state);
        let namespace = namespace.clone();
        async move {
            if let Err(error) = redeliver_codex_credentials(&state, &namespace).await {
                warn!(%error, "failed to redeliver the central Codex credential to live crews");
            }
        }
    })
}

pub(super) async fn redeliver_codex_credentials(state: &ControllerRuntimeState, namespace: &str) -> Result<(), String> {
    let Some(registry) = state.agent_material.as_deref() else {
        return Ok(());
    };
    let environments = state.daemon.resource_backend().using::<Environment>(namespace);
    for environment in environments.list().await.map_err(|error| error.to_string())?.items {
        let Some(spec) = environment.spec.docker.as_ref() else {
            continue;
        };
        if !spec.required_agent_adapters.contains("codex")
            || spec.env.contains_key("CODEX_HOME")
            || environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Ready)
        {
            continue;
        }
        // A redelivery failure is never fatal to the environment: the crew keeps
        // the copy it already has, and the next tick retries.
        match registry.refresh_delivered_credentials(&environment.metadata.name).await {
            Ok(true) => info!(environment = %environment.metadata.name, "redelivered the central Codex credential"),
            Ok(false) => {}
            Err(error) => warn!(environment = %environment.metadata.name, %error, "failed to redeliver the central Codex credential"),
        }
    }
    Ok(())
}
