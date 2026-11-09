//! Work-credential delivery, refresh, and agent-environment composition.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_capability_contract::CredentialCapability;
use flotilla_capability_contract::SessionCapabilitySource;
use flotilla_credentials::{
    codex_central_auth_path, compose_agent_environment, AgentMaterialRegistry, CodexCentralRefresher, CredentialRefreshError,
    GithubAppScope,
};
use flotilla_crew::ports::WorkCredentialReconciler;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use flotilla_resources::attentions::{Attention, Attentions, Sender, Subject};
use flotilla_resources::{Convoy, RepositoryTrust, Vessel};
use flotilla_store::ResourceBackend;
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
            None if references.is_empty() => Ok(Vec::new()),
            None => Err("credential delivery observations unavailable".into()),
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
            let record = state.environment(environment).await?.ok_or_else(|| format!("environment {environment} not found"))?;
            if let Some(docker) = record.spec.docker {
                for (key, value) in &docker.env {
                    if let Some((destination, address)) = flotilla_capability_contract::endpoint_for_env(key, value) {
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
    async fn reconcile(&self, _namespace: &str, environment_ref: &str) -> Result<(), String> {
        let state = self.state.upgrade().ok_or_else(|| "credential controller is unavailable".to_string())?;
        if state.credential_store.is_none() {
            return Ok(());
        }
        state.credential_events.deliver(environment_ref).await
    }

    async fn ledger_delivery_environment(&self, namespace: &str, environment_ref: &str) -> Result<BTreeMap<String, String>, String> {
        self.reconcile(namespace, environment_ref).await?;
        let state = self.state.upgrade().ok_or_else(|| "credential controller is unavailable".to_string())?;
        let store = state.credential_store.as_ref().ok_or("credential store is unavailable")?;
        store.ledger_delivery_environment(environment_ref).await
    }
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

/// The credentials an open attention names, one per line of its reason.
fn told(reason: &str) -> BTreeSet<String> {
    reason.lines().filter_map(|line| line.split_once(": ")).map(|(credential, _)| credential.to_owned()).collect()
}

/// Each vessel's attention names the credentials that keep failing. It is raised when that set
/// changes, cleared when none fails, and a person's acknowledgement stands for its episode: from
/// the earliest failure of the vessel's failing credentials until a full fix ends it. A credential that starts failing while the
/// attention is acknowledged begins a new episode, so the person is told again.
pub(super) async fn reconcile_credential_refresh_attention_in(
    attentions: &dyn Attentions,
    backend: &ResourceBackend,
    namespace: &str,
    errors: &[CredentialRefreshError],
) -> Result<(), String> {
    let open: BTreeMap<Subject, BTreeSet<String>> = attentions
        .open(namespace, Some(Sender::Capabilities), None)
        .await?
        .into_iter()
        .map(|open| (open.subject, told(&open.reason)))
        .collect();
    let vessels = backend.using::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?;
    let convoys =
        backend.query_declared::<Convoy>(namespace, &flotilla_store::live_convoys_query()).await.map_err(|error| error.to_string())?;
    let convoys = convoys.items.into_iter().map(|source| (source.object.metadata.name.clone(), source.object)).collect::<BTreeMap<_, _>>();
    // Every vessel that holds a credential, with its failing ones: why, and whether persistent enough to tell a person.
    let mut held: BTreeMap<(String, String), BTreeMap<String, (String, bool)>> = BTreeMap::new();
    // When each vessel's credentials began failing: the episode its attention tells.
    let mut since: BTreeMap<(String, String), DateTime<Utc>> = BTreeMap::new();
    for vessel in &vessels.items {
        let Some(convoy) = convoys.get(&vessel.spec.convoy_ref) else { continue };
        let Some(requirement) = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|requirement| requirement.name == vessel.spec.vessel_name))
            .filter(|requirement| !requirement.credential_refs.is_empty())
        else {
            continue;
        };
        let key = (convoy.metadata.name.clone(), vessel.spec.vessel_name.clone());
        let failing = held.entry(key.clone()).or_default();
        for error in errors {
            let Some(credential) = error.credential_name.as_ref() else { continue };
            if vessel.status.as_ref().and_then(|status| status.environment_ref.as_ref()) == Some(&error.environment_ref)
                && requirement.credential_refs.contains(credential)
            {
                failing.insert(credential.clone(), (error.message.clone(), error.should_surface));
                // The episode began with the vessel's earliest failing credential.
                if let Some(began) = error.first_failure_at {
                    since.entry(key.clone()).and_modify(|since| *since = (*since).min(began)).or_insert(began);
                }
            }
        }
    }
    for ((convoy, vessel), credentials) in held {
        let began_at = since.get(&(convoy.clone(), vessel.clone())).copied().unwrap_or_else(Utc::now);
        let subject = Subject::Vessel { convoy: convoy.clone(), vessel: vessel.clone() };
        let before = open.get(&subject);
        // A credential already told stays told while it keeps failing, persistent or not.
        let told: BTreeMap<String, String> = credentials
            .into_iter()
            .filter(|(credential, (_, surface))| *surface || before.is_some_and(|before| before.contains(credential)))
            .map(|(credential, (why, _))| (credential, why))
            .collect();
        if told.is_empty() {
            attentions.clear(namespace, Sender::Capabilities, &subject).await?;
            continue;
        }
        if before.is_some_and(|before| told.keys().eq(before.iter())) {
            continue;
        }
        let began_at = match before {
            Some(_) => began_at,
            // None open: one a person acknowledged answers only the credentials it named.
            None => match attentions.acknowledged(namespace, Sender::Capabilities, &subject, began_at).await? {
                Some(seen) if told.keys().all(|credential| self::told(&seen).contains(credential)) => continue,
                Some(_) => Utc::now(),
                None => began_at,
            },
        };
        attentions.raise(namespace, &Attention::credential_refresh(&convoy, &vessel, told, began_at)).await?;
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
            if let Err(error) = redeliver_codex_credentials(&state).await {
                warn!(%error, "failed to redeliver the central Codex credential to live crews");
            }
            // #4037: every crew home holding a copy, on Docker and SSH hosts alike.
            if let Err(error) = super::access_layer::redeliver_central_logins(&state, &namespace).await {
                warn!(%error, "failed to push the central Codex login into live crew homes");
            }
        }
    })
}

pub(super) async fn redeliver_codex_credentials(state: &ControllerRuntimeState) -> Result<(), String> {
    let Some(registry) = state.agent_material.as_deref() else {
        return Ok(());
    };
    for environment in state.live_environments().await? {
        let Some(spec) = environment.spec.docker.as_ref() else {
            continue;
        };
        if !spec.required_agent_adapters.contains("codex") || spec.env.contains_key("CODEX_HOME") || !environment.is_ready() {
            continue;
        }
        // A redelivery failure is never fatal to the environment: the crew keeps
        // the copy it already has, and the next tick retries.
        let mounts = state
            .daemon
            .environment_manager()
            .provisioned_mounts(&flotilla_protocol::EnvironmentId::new(environment.name.as_str()))
            .unwrap_or_else(|| {
                tracing::warn!(environment = %environment.name,
                    "provisioned environment mounts unavailable; falling back to pushed Codex credential delivery");
                Vec::new()
            });
        // The fallback writes only auth.json through the guest runner. If the
        // home is actually Shared, this reaches the same RW host mount and
        // preserves its session files while rotating the credential.
        let result = if registry.codex_home_is_shared(&environment.name, &mounts) {
            registry.refresh_delivered_credentials(&environment.name).await
        } else {
            match state.daemon.command_runner_for_environment_ref(&environment.name) {
                Some(runner) => registry.refresh_delivered_credentials_through(&environment.name, runner.as_ref()).await,
                None => Err("agent runner unavailable for Codex auth rotation".into()),
            }
        };
        match result {
            Ok(true) => info!(environment = %environment.name, "redelivered the central Codex credential"),
            Ok(false) => {}
            Err(error) => warn!(environment = %environment.name, %error, "failed to redeliver the central Codex credential"),
        }
    }
    Ok(())
}
