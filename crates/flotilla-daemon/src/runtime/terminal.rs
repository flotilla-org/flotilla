//! Terminal resource runtime and confirmed input delivery.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_agent::adapter::AgentAdapter;
use flotilla_agent::adapter::AgentAdapterRegistry;
use flotilla_agent::adapter::AgentLaunchRequest;
use flotilla_agent::adapter::CapabilityTable;
use flotilla_agent::{CLAUDE_CODE_HOOK_TOKEN_ENV, CLAUDE_CODE_HOOK_URL_ENV};
use flotilla_capability_contract::SessionCapabilitySource;
use flotilla_credentials::crew_git_identity_environment;
use flotilla_crew::reconcilers::terminal_session::{
    TerminalDeliveryFailure, TerminalDeliveryOutcome, TerminalDeliveryReadiness, TerminalLiveness, TerminalObservation, TerminalRuntime,
    TerminalRuntimeState,
};
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_environment::environment::{vessel_ops::RunnerVesselAgent, VesselAgent};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{ConfiguredResourceLimits, EnvironmentId, TerminalStatus};
use flotilla_provider_runtime::execution_context::BoundLayer;
use flotilla_provider_runtime::CommandRunner;
use flotilla_resources::{
    Checkout, Convoy, FulfilmentKind, ResourceObject, TerminalAttentionSource, TerminalAttentionState, TerminalOccupancy, TerminalSession,
    TerminalSessionPhase, TerminalSessionSource, TerminalSessionTarget, Vessel, CREDENTIAL_PERMISSIONS_SESSION_TAG,
    CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_SESSION_TAG,
};
use flotilla_store::ResourceBackend;
use flotilla_terminal::{ScreenActivity, TerminalPool, TerminalSessionLiveness, TerminalSize, TerminalSubmission};
use flotilla_vcs::vcs::CheckoutVcsResolver as _;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::access_layer::{deliver_access, deliver_host_account, AccessDelivered, AccessLaunch};
use super::account_access::AccountLaunch;
use super::agentless_vessels::{apply_reach_environment, compose_session_environment, vessel_base, VesselAccess};
use super::credentials::RuntimeSessionCapabilities;
use super::state::ControllerRuntimeState;
use crate::blob_store::BlobDigest;
use crate::blob_store::BlobStore;
use flotilla_agent::adapter::FLOTILLA_ENVIRONMENT_ID_ENV;

pub(super) struct PendingTerminalDelivery {
    pub(super) message_batch: Option<String>,
    pub(super) message: String,
    pub(super) task: JoinHandle<Result<TerminalDeliveryOutcome, String>>,
    /// Present for an engine turn, which owns its receipt (#3894).
    pub(super) engine_turn: Option<EngineTurn>,
}

/// An engine turn's teardown state: teardown stops a turn not yet typed, and
/// lets a typed one settle, reserved, so the delivery adapter records its receipt.
pub(super) struct EngineTurn {
    /// Cancelled by teardown; the turn stops if it has not yet been typed.
    pub(super) stop: tokio_util::sync::CancellationToken,
    /// When teardown first waited on this turn.
    pub(super) teardown_since: Option<tokio::time::Instant>,
}

impl EngineTurn {
    pub(super) fn new(stop: tokio_util::sync::CancellationToken) -> Self {
        Self { stop, teardown_since: None }
    }
}

pub(super) enum TerminalDeliveryLookup {
    InFlight,
    Vacant,
    Taken(PendingTerminalDelivery),
}

pub(super) fn lookup_terminal_delivery(
    deliveries: &StdMutex<HashMap<String, PendingTerminalDelivery>>,
    session_id: &str,
    message: &str,
) -> TerminalDeliveryLookup {
    let mut deliveries = deliveries.lock().expect("terminal deliveries lock poisoned");
    match deliveries.get(session_id) {
        Some(delivery) if delivery.message_batch.is_some() => TerminalDeliveryLookup::InFlight,
        Some(delivery) if delivery.message == message && !delivery.task.is_finished() => TerminalDeliveryLookup::InFlight,
        Some(_) => TerminalDeliveryLookup::Taken(deliveries.remove(session_id).expect("observed terminal delivery")),
        None => TerminalDeliveryLookup::Vacant,
    }
}

pub(super) struct TerminalControllerRuntime {
    pub(super) state: Arc<ControllerRuntimeState>,
}

// The dummy fleet (`crate::dummy_fleet`) warns when its turn delay is below
// STABLE_FOR + POLL; keep that warning in mind when changing either.
pub(crate) const DELIVERY_CONFIRMATION_POLL: Duration = Duration::from_millis(200);

pub(super) const DELIVERY_CONFIRMATION_GRACE: Duration = Duration::from_secs(4);
/// How long teardown lets a typed engine turn settle before aborting it (#3894):
/// derived from the confirmation grace, with room for the submission itself.
/// The finalizer rechecks without blocking, so other sessions' teardown proceeds.
const ENGINE_TURN_SETTLE_FOR: Duration = Duration::from_secs(2 * DELIVERY_CONFIRMATION_GRACE.as_secs() + 2);

pub(crate) const DELIVERY_CONFIRMATION_STABLE_FOR: Duration = Duration::from_secs(1);

// Adapter classification supplies attention and acceptance evidence. Cleat owns
// the composer judgement that gates all writes.
pub(super) async fn observe_terminal_screen(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
    now: chrono::DateTime<Utc>,
) -> Result<Option<TerminalObservation>, String> {
    let Some(session) = pool.list_sessions().await?.into_iter().find(|session| session.session_name == session_id) else {
        return Ok(None);
    };
    let occupancy = match session.status {
        TerminalStatus::Running => TerminalOccupancy::Occupied,
        TerminalStatus::Disconnected | TerminalStatus::Exited(_) => TerminalOccupancy::Vacant,
    };
    let mut output_digest = None;
    let mut state = None;
    let mut obstruction = None;
    if let Some(adapter) = adapter {
        match pool.capture_screen(session_id).await {
            Ok(Some(screen)) => {
                output_digest = adapter.screen_output_digest(&screen);
                state = adapter.classify_screen_attention(&screen);
                obstruction = adapter.classify_screen_obstruction(&screen);
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(%session_id, %error, "could not capture terminal screen for attention observation"),
        }
    }
    let state = state.or_else(|| {
        session.screen_activity.map(|activity| match activity {
            ScreenActivity::Active => TerminalAttentionState::Working,
            ScreenActivity::Stable => TerminalAttentionState::Idle,
        })
    });
    Ok(Some(TerminalObservation {
        output_digest,
        obstruction,
        attention: state.map(|state| flotilla_resources::TerminalAttention { state, as_of: now, source: TerminalAttentionSource::Screen }),
        occupancy,
    }))
}

pub(super) async fn session_busy_after_delivery_grace(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
) -> Result<bool, String> {
    // Require one second of consecutive positive observations within the
    // confirmation window. A redraw's isolated Working/Idle sample cannot
    // establish whether the harness accepted a queued turn.
    let mut positive_since = None;
    let deadline = tokio::time::Instant::now() + DELIVERY_CONFIRMATION_GRACE;
    loop {
        let observation = observe_terminal_screen(pool, adapter, session_id, Utc::now())
            .await?
            .ok_or_else(|| format!("terminal session {session_id} disappeared after message delivery"))?;
        // Pools without attention evidence retain legacy best-effort behavior,
        // but only after the same stability window.
        let positive = observation
            .attention
            .is_none_or(|attention| matches!(attention.state, TerminalAttentionState::Working | TerminalAttentionState::NeedsInput));
        let now = tokio::time::Instant::now();
        if positive {
            let since = positive_since.get_or_insert(now);
            if now.duration_since(*since) >= DELIVERY_CONFIRMATION_STABLE_FOR {
                return Ok(true);
            }
        } else {
            positive_since = None;
        }
        if now >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(DELIVERY_CONFIRMATION_POLL).await;
    }
}

pub(super) async fn deliver_and_confirm(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
    message: &str,
    _readiness: TerminalDeliveryReadiness,
) -> Result<TerminalDeliveryOutcome, String> {
    // All writes are gated atomically by cleat, including startup deliveries.
    // Adapter observations remain acceptance evidence only.
    submit_and_confirm(pool, adapter, session_id, message).await
}

/// Revalidate batch freshness immediately before the atomic guarded write.
pub(super) async fn deliver_guarded_and_confirm(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session: &str,
    text: &str,
    inbox: &flotilla_messaging::MessageInbox,
    members: &[String],
) -> Result<TerminalDeliveryOutcome, String> {
    // Cleat is the only write gate. No adapter classifier or hold deadline may
    // turn a known-unsent queued message into an operator-only hold.
    match inbox.validate_delivery_members(members, Utc::now()).await {
        Ok(true) => {}
        Ok(false) => {
            info!(%session, "batch member superseded before input; reforming the unsent batch");
            return Ok(TerminalDeliveryOutcome::BatchSuperseded);
        }
        Err(error) => {
            info!(%session, %error, "turn condition not yet observable; no input sent");
            return Ok(TerminalDeliveryOutcome::Deferred("turn condition not yet observable"));
        }
    }
    submit_and_confirm(pool, adapter, session, text).await
}

pub(super) async fn submit_and_confirm(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
    message: &str,
) -> Result<TerminalDeliveryOutcome, String> {
    let submission = pool.guarded_deliver(session_id, message).await;
    match submission {
        Ok(TerminalSubmission::Submitted) => {}
        Ok(TerminalSubmission::Refused(reason)) => {
            debug!(%session_id, %reason, "cleat refused atomic submission; keeping delivery queued");
            // Keep the in-flight batch at the FIFO head. A refusal consumes
            // neither a receipt nor the bounded transport-failure budget.
            let reason = match reason.split_once(':').map(|(code, _)| code) {
                Some("not-matched") => "cleat composer selector did not match; no input sent",
                Some("role-absent") => "cleat composer role is absent; no input sent",
                Some("unsupported") => "cleat composer recognizer is unsupported; no input sent",
                _ => "cleat controller is active; no input sent",
            };
            return Ok(TerminalDeliveryOutcome::Deferred(reason));
        }
        Err(error) => {
            warn!(%session_id, %error, "message write has an ambiguous outcome; holding without resending");
            return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
        }
    }
    match session_busy_after_delivery_grace(pool, adapter, session_id).await {
        Ok(true) => Ok(TerminalDeliveryOutcome::Confirmed),
        result => {
            warn!(%session_id, ?result, "message submission lacks stable evidence; holding without resending");
            Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed))
        }
    }
}

/// A terminal context names the Vessel resource, while admission pins use its
/// within-convoy name. Resolve that identity before looking up agent grants.
pub(super) async fn fulfilment_grants_for_terminal(
    backend: &ResourceBackend,
    context: &flotilla_resources::TerminalCrewContext,
) -> Option<BTreeSet<flotilla_resources::FulfilmentGrant>> {
    let convoy = backend.including_replicas::<Convoy>(&context.namespace).get(&context.convoy).await.ok()?.object;
    let vessel_name = backend
        .including_replicas::<Vessel>(&context.namespace)
        .get(&context.vessel_ref)
        .await
        .ok()
        .map(|source| source.object.spec.vessel_name)
        .or_else(|| context.vessel_ref.strip_prefix(&format!("{}-", context.convoy)).map(str::to_string));
    let selected_kind = vessel_name
        .as_deref()
        .and_then(|name| flotilla_resources::vessel_placement_pin(&convoy, name))
        .map(|pin| pin.decision.policy_name)
        .or_else(|| convoy.status.and_then(|status| status.placement_decision.map(|decision| decision.policy_name)))?;
    backend.including_replicas::<FulfilmentKind>(&context.namespace).get(&selected_kind).await.ok().map(|source| source.object.spec.grants)
}

pub(super) fn terminal_liveness_for_source(source: &TerminalSessionSource, liveness: TerminalSessionLiveness) -> TerminalLiveness {
    match liveness {
        TerminalSessionLiveness::Running => TerminalLiveness::Running,
        TerminalSessionLiveness::Stopped => TerminalLiveness::Stopped,
        TerminalSessionLiveness::Absent if matches!(source, TerminalSessionSource::Agent { .. }) => {
            TerminalLiveness::Lost("cleat agent session is absent from its daemon".into())
        }
        TerminalSessionLiveness::Absent => TerminalLiveness::Stopped,
        TerminalSessionLiveness::Lost(reason) => TerminalLiveness::Lost(reason),
    }
}

#[async_trait]
impl TerminalRuntime for TerminalControllerRuntime {
    async fn verify_reclaim(&self, convoy: &ResourceObject<Convoy>) -> Result<(), String> {
        let backend = self.state.daemon.resource_backend();
        let records = backend.including_replicas::<Checkout>(&convoy.metadata.namespace).list().await.map_err(|error| error.to_string())?;
        let checkouts = flotilla_resources::select_convoy_children(convoy, &records.items).into_values().collect::<Vec<_>>();
        self.state.daemon.verify_convoy_teardown_gate_for_checkouts(convoy, &checkouts, false).await
    }

    async fn brief_ready(&self, spec: TerminalSessionTarget<'_>) -> Result<bool, String> {
        let TerminalSessionSource::Agent { brief, .. } = &spec.source else { return Ok(true) };
        let Some(digest) = &brief.artifact_digest else { return Ok(true) };
        let store = self.state.blob_store.as_ref().ok_or("brief blob store unavailable")?;
        Ok(store.get(&BlobDigest::parse(digest)?).await?.is_some())
    }

    async fn cleat_endpoint(
        &self,
        session_id: &str,
        spec: TerminalSessionTarget<'_>,
    ) -> Result<Option<flotilla_protocol::result_set::CleatEndpoint>, String> {
        let pool = self.pool_for_spec(spec).await?;
        pool.cleat_endpoint(session_id).await
    }

    async fn ensure_session(
        &self,
        name: &str,
        spec: TerminalSessionTarget<'_>,
        tags: &[flotilla_terminal::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        // #3792: a crew on an agentless host runs in its vessel's own agent:
        // its providers, files and commands all go through that agent.
        let agentless = self.state.agentless_ssh.get(&spec.env_ref);
        let vessel = self.vessel_for_spec(spec).await?;
        let registry = match &vessel {
            Some(access) => Arc::clone(&access.registry),
            None => self.registry_for_env(&spec.env_ref)?,
        };
        let pool = registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))?;
        let launch_runner = || match &vessel {
            Some(access) => Ok(access.runner()),
            None => self.runner_for_env(&spec.env_ref),
        };

        let cwd = ExecutionEnvironmentPath::new(&spec.cwd);
        let credential_refs =
            tags.iter().filter(|tag| tag.key == CREDENTIAL_REF_SESSION_TAG).map(|tag| tag.value.clone()).collect::<BTreeSet<_>>();
        let credential_scopes = tags
            .iter()
            .find(|tag| tag.key == CREDENTIAL_SCOPES_SESSION_TAG)
            .map(|tag| serde_json::from_str(&tag.value).map_err(|error| format!("invalid credential scopes: {error}")))
            .transpose()?
            .unwrap_or_default();
        let credential_permissions = tags
            .iter()
            .find(|tag| tag.key == CREDENTIAL_PERMISSIONS_SESSION_TAG)
            .map(|tag| serde_json::from_str(&tag.value).map_err(|error| format!("invalid credential permissions: {error}")))
            .transpose()?
            .unwrap_or_default();
        let mut pool_tags = tags
            .iter()
            .filter(|tag| {
                tag.key != CREDENTIAL_REF_SESSION_TAG
                    && tag.key != CREDENTIAL_SCOPES_SESSION_TAG
                    && tag.key != CREDENTIAL_PERMISSIONS_SESSION_TAG
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut credential_env = match &self.state.credential_store {
            Some(store) => {
                let runner = launch_runner()?;
                store
                    .prepare_scoped_with_permissions(&spec.env_ref, &credential_refs, &credential_scopes, &credential_permissions, runner)
                    .await?
            }
            None if credential_refs.is_empty() => Vec::new(),
            None => return Err("host-local credential store unavailable".to_string()),
        };
        // An agentless crew's bound layer: what credential delivery and its
        // harness homes produced, with the wrapper directories a declared
        // `PATH` names. No other declared variable joins it.
        let mut bound_material = credential_env.clone();
        bound_material.extend(spec.env.get("PATH").map(|path| ("PATH".to_string(), path.clone())));
        let mut declared_env = spec.env.clone();
        declared_env.extend(credential_env);
        if spec.env_ref == self.state.host_direct_environment_name {
            if let Some(socket) = self.state.daemon.daemon_socket_path().await {
                declared_env.insert("FLOTILLA_DAEMON_SOCKET".into(), socket.display().to_string());
            }
        }
        credential_env = declared_env.into_iter().collect();
        // #3655: containment is a property of the session. A session in a
        // flotilla environment names it, and its harness takes the contained
        // path; the daemon host's own host-direct session does not.
        if let Some(access) = &vessel {
            // #3618, #3792: an agentless host's crews reach the daemon through
            // their vessel agent's forward, with the generation's CLI on PATH.
            apply_reach_environment(&mut credential_env, &spec.env_ref, &access.paths);
        } else if spec.env_ref != self.state.host_direct_environment_name {
            // A provisioned vessel's container already names its environment;
            // the session states it too, so the harness sees what it runs in.
            credential_env.retain(|(key, _)| key != FLOTILLA_ENVIRONMENT_ID_ENV);
            credential_env.push((FLOTILLA_ENVIRONMENT_ID_ENV.to_string(), spec.env_ref.clone()));
        } else {
            // The daemon host's own session is never contained, whatever a
            // declared or delivered environment claims.
            credential_env.retain(|(key, _)| key != FLOTILLA_ENVIRONMENT_ID_ENV);
        }
        let (mut command, mut env, crew) = match &spec.source {
            TerminalSessionSource::Tool { command } => (command.clone(), credential_env.clone(), None),
            TerminalSessionSource::Agent { selector, brief, context, .. } => {
                let mut materialized_brief = brief.clone();
                if let Some(digest) = &brief.artifact_digest {
                    let store = self.state.blob_store.as_ref().ok_or("brief blob store unavailable")?;
                    let digest = BlobDigest::parse(digest)?;
                    let body =
                        store.get(&digest).await?.ok_or_else(|| format!("brief artifact blob {} is unavailable", digest.as_str()))?;
                    materialized_brief.content =
                        String::from_utf8(body).map_err(|error| format!("brief artifact is not UTF-8: {error}"))?;
                }
                let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
                let vcs = self.state.daemon.vcs_for_env_ref(&spec.env_ref, cwd.as_path()).await?;
                let account = AccountLaunch {
                    namespace: &context.namespace,
                    convoy: &context.convoy,
                    role: &spec.role,
                    env_ref: &spec.env_ref,
                    agentless: agentless.is_some(),
                };
                // #3964: a crew aboard a vessel gets its harness home, skills and account
                // material from the access layer, through its vessel agent, before its
                // harness starts. The daemon host's own sessions have no vessel: a
                // requester there starts only in the home its binding delivers (#3907).
                let delivered = if spec.env_ref == self.state.host_direct_environment_name {
                    match requirement.account_access() {
                        Some(access) => deliver_host_account(&self.state, &account, access, &*launch_runner()?).await?,
                        None => AccessDelivered::default(),
                    }
                } else {
                    let agent = self.vessel_agent(spec, vessel.as_deref()).await?;
                    let launch = AccessLaunch {
                        account,
                        vessel_ref: &context.vessel_ref,
                        adapter: &requirement.adapter,
                        access: requirement.account_access(),
                    };
                    deliver_access(&self.state, &launch, agent, &credential_env).await?
                };
                // #3909: a harness the access layer delivered is the one this launch runs.
                let adapter = match &delivered.executable {
                    Some(executable) => {
                        AgentAdapterRegistry::delivered(&requirement.adapter, executable, &delivered.environment, launch_runner()?)
                            .ok_or_else(|| format!("agent adapter {} has no built-in harness", requirement.adapter))?
                    }
                    None => self.session_adapter(spec, &requirement.adapter).await?,
                };
                let accessed = delivered.environment;
                credential_env.retain(|(key, _)| !accessed.iter().any(|(name, _)| name == key));
                // #3913: the access layer's home and token file are the crew's bound layer;
                // the prepared context sets them from there.
                bound_material.retain(|(key, _)| !accessed.iter().any(|(name, _)| name == key));
                bound_material.extend(accessed.iter().cloned());
                credential_env.extend(accessed);
                if let Some(store) = &self.state.credential_store {
                    store.record_capability_endpoints(&spec.env_ref, name, &credential_env).await;
                    let card = async {
                        let credentials = store.credentials(&spec.env_ref, &credential_refs).await?;
                        flotilla_crew::crew_capabilities::session_card(
                            &self.state.daemon.resource_backend(),
                            &context.namespace,
                            &context.convoy,
                            spec.spec,
                            &credentials,
                            &RuntimeSessionCapabilities { state: Arc::downgrade(&self.state) }.endpoints(&spec.env_ref, name).await?,
                            self.state.daemon.lifecycle_reads(),
                            &*self.state.daemon.account_records(),
                        )
                        .await
                    }
                    .await;
                    let card = match card {
                        Ok(card) => card,
                        Err(error) => {
                            warn!(session = %name, %error, "failed to construct advisory launch capability card");
                            format!("{}\n\nCapabilities are currently unavailable. Run `flotilla crew capabilities` when unsure or after the service recovers.\n", flotilla_agent::adapter::CAPABILITIES_HEADING.trim_start())
                        }
                    };
                    if let Err(error) = self.state.daemon.observe_launch_capability_card(&context.namespace, name, &card).await {
                        warn!(session = %name, %error, "failed to persist launch capability observation");
                    }
                    materialized_brief.content.push_str("\n\n");
                    materialized_brief.content.push_str(&card);
                }
                // Host-direct Claude Code delivers hooks over HTTP while flotillad listens on
                // loopback TCP (#3247); vessel sessions keep command hooks until their
                // loopback forwarder (#3485). The token binds the hooks to this terminal.
                if spec.env_ref == self.state.host_direct_environment_name && requirement.adapter == "claude-code" {
                    let hooks = if let Some(endpoint) = self.state.hook_endpoint.as_ref() {
                        endpoint.claude_code_launch(&self.state.daemon.resource_backend(), &context.namespace, name).await
                    } else {
                        None
                    };
                    if let Some(hooks) = hooks {
                        info!(session = %name, url = %hooks.base_url, "Claude Code hooks post over HTTP");
                        credential_env.retain(|(key, _)| key != CLAUDE_CODE_HOOK_URL_ENV && key != CLAUDE_CODE_HOOK_TOKEN_ENV);
                        credential_env.extend([
                            (CLAUDE_CODE_HOOK_URL_ENV.to_string(), hooks.base_url),
                            (CLAUDE_CODE_HOOK_TOKEN_ENV.to_string(), hooks.token),
                        ]);
                    }
                }
                adapter.prepare_with_vcs(&cwd, &materialized_brief, &credential_env, vcs.as_ref()).await?;
                for copy_root in &brief.copies {
                    let copy_root = ExecutionEnvironmentPath::new(copy_root);
                    if copy_root != cwd {
                        let vcs = self.state.daemon.vcs_for_env_ref(&spec.env_ref, copy_root.as_path()).await?;
                        adapter.prepare_with_vcs(&copy_root, &materialized_brief, &credential_env, vcs.as_ref()).await?;
                    }
                }
                let backend = self.state.daemon.resource_backend();
                let fulfilment_grants = fulfilment_grants_for_terminal(&backend, context).await;
                let plan = adapter.launch(&AgentLaunchRequest {
                    role: spec.role.clone(),
                    model: requirement.model.clone(),
                    brief: materialized_brief,
                    environment: credential_env.clone(),
                    fulfilment_grants,
                })?;
                let crew_id = uuid::Uuid::new_v4().to_string();
                let crew = flotilla_resources::CrewSessionStatus::builder()
                    .id(crew_id.clone())
                    .adapter(requirement.adapter)
                    .maybe_model(requirement.model)
                    .stance(plan.stance)
                    .build();
                let mut env = plan.env;
                let git_identity = crew_git_identity_environment();
                env.retain(|(key, _)| !git_identity.iter().any(|(identity_key, _)| identity_key == key));
                env.extend(git_identity);
                env.extend([
                    ("FLOTILLA_CREW_ID".to_string(), crew_id),
                    ("FLOTILLA_CONVOY".to_string(), context.convoy.clone()),
                    ("FLOTILLA_VESSEL".to_string(), context.vessel_ref.clone()),
                    ("FLOTILLA_CREW_ROLE".to_string(), spec.role.clone()),
                    ("FLOTILLA_NAMESPACE".to_string(), context.namespace.clone()),
                    ("FLOTILLA_TERMINAL_SESSION".to_string(), name.to_string()),
                ]);
                (flotilla_agent::process::monitored_command(&plan.command, &crew.id), env, Some(crew))
            }
        };
        // Crews rebuild one checkout across edits, rebases and review fixes. Set this explicitly: Cargo turns
        // incremental compilation off by default when `CI` is set, and harness shells may set it.
        env.push(("CARGO_INCREMENTAL".to_string(), "1".to_string()));

        let is_agent_session = matches!(spec.source, TerminalSessionSource::Agent { .. });
        let host_ref = if spec.env_ref == self.state.host_direct_environment_name {
            Some(self.state.local_host_ref.as_str())
        } else {
            self.state.agentless_ssh.get(&spec.env_ref).map(|profile| profile.provisioning.host_id.as_str())
        };
        let mut configured_limits = None;
        if let Some(host_ref) = host_ref {
            let vessel_runner = vessel.as_ref().map(|access| access.runner());
            let jobs = self.state.rust_build_jobs_through(host_ref, vessel_runner.as_deref()).await?;
            configured_limits =
                Some(ConfiguredResourceLimits { cpus: None, cpu_shares: None, build_jobs: Some(jobs), linker_threads: Some(jobs) });
            let wrapper = self.state.rustc_wrapper_for_environment(&spec.env_ref, vessel_runner.as_deref()).await?;
            // The fulfilment cap owns the workspace wrapper for host-direct
            // terminals, including Tool sessions that can run Cargo. It
            // replaces any wrapper supplied in the terminal environment.
            env.retain(|(name, _)| !matches!(name.as_str(), "CARGO_BUILD_JOBS" | "FLOTILLA_LINKER_THREADS" | "RUSTC_WORKSPACE_WRAPPER"));
            env.extend([
                ("CARGO_BUILD_JOBS".to_string(), jobs.to_string()),
                ("FLOTILLA_LINKER_THREADS".to_string(), jobs.to_string()),
                ("RUSTC_WORKSPACE_WRAPPER".to_string(), wrapper.display().to_string()),
            ]);
        }
        if let (Some(profile), Some(access)) = (agentless, &vessel) {
            // Rendered last, so nothing after it adds to PATH or sets a base
            // variable. Every agentless launch reaches here with its vessel:
            // without one it returned above.
            let bound = BoundLayer::delivered(&bound_material, &vessel_base(profile, &access.paths)?)?;
            command = compose_session_environment(&mut env, &command, &access.paths, profile, bound)?;
        }
        // A dead generation may retain a recording with the old ID. Launch
        // under a fresh ID so cleat cannot resolve the name ambiguously, then
        // mark the old recording for retention after the launch succeeds.
        let recovered_from_lost = matches!(pool.session_liveness(name).await?, TerminalSessionLiveness::Lost(_));
        let session_id = if recovered_from_lost { format!("{name}-{}", uuid::Uuid::new_v4()) } else { name.to_string() };
        if is_agent_session && pool.list_sessions().await?.iter().any(|session| session.session_name == session_id) {
            pool.kill_session(&session_id).await?;
        }
        let initial_size = is_agent_session.then_some(CREW_SESSION_SIZE);
        if let Some(crew) = &crew {
            if matches!(crew.adapter.as_str(), "codex" | "claude-code") {
                pool_tags.push(flotilla_terminal::TerminalSessionTag::new("recognizer", &crew.adapter));
            }
        }
        pool.ensure_session_with_size(&session_id, &command, &cwd, &env, &pool_tags, initial_size).await?;
        if recovered_from_lost {
            if let Err(error) = pool.retain_recovered_recording(name).await {
                tracing::warn!(%error, session = name, "retain old cleat recording after recovery failed");
            }
        }
        Ok(TerminalRuntimeState::builder()
            .maybe_configured_limits(configured_limits)
            .session_id(session_id)
            .maybe_pid(None)
            .started_at(Utc::now())
            .maybe_crew(crew)
            .launch_command(command)
            .maybe_delivered_message_id(None)
            .build())
    }

    async fn session_liveness(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<TerminalLiveness, String> {
        let pool = match self.pool_for_spec(spec).await {
            Ok(pool) => pool,
            Err(message) => return Ok(TerminalLiveness::Unavailable(message)),
        };
        Ok(match pool.session_liveness(session_id).await {
            Ok(liveness) => terminal_liveness_for_source(&spec.source, liveness),
            Err(message) => TerminalLiveness::Unavailable(message),
        })
    }

    async fn agent_exit_code(
        &self,
        spec: TerminalSessionTarget<'_>,
        crew: &flotilla_resources::CrewSessionStatus,
    ) -> Result<Option<i32>, String> {
        let runner = self.runner_for_spec(spec).await?;
        // Receipts are read in batches per runner: an agentless host's
        // vessels each read through their own agent.
        let vessel = self.state.agentless_ssh.contains_key(&spec.env_ref).then_some(spec.vessel_ref).flatten();
        let batch = match vessel {
            Some(vessel) => format!("{}/{vessel}", spec.env_ref),
            None => spec.env_ref.clone(),
        };
        self.state
            .exit_receipts
            .observe(&batch, &*runner, Path::new(&spec.cwd).join(flotilla_agent::process::exit_receipt(&crew.id)), || async {
                let sessions = self
                    .state
                    .daemon
                    .resource_backend()
                    .using::<TerminalSession>(&self.state.namespace)
                    .list()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(sessions
                    .items
                    .into_iter()
                    .filter_map(|session| {
                        if session.spec.env_ref != spec.env_ref
                            || !matches!(session.spec.source, TerminalSessionSource::Agent { .. })
                            || vessel.is_some_and(|vessel| TerminalSessionTarget::from(&session).vessel_ref != Some(vessel))
                        {
                            return None;
                        }
                        let status = session.status?;
                        if status.phase != TerminalSessionPhase::Running {
                            return None;
                        }
                        status.crew.map(|crew| Path::new(&session.spec.cwd).join(flotilla_agent::process::exit_receipt(&crew.id)))
                    })
                    .collect())
            })
            .await
    }

    async fn remove_exit_receipt(&self, spec: TerminalSessionTarget<'_>, launch_id: &str) -> Result<(), String> {
        let runner = self.runner_for_spec(spec).await?;
        flotilla_agent::process::remove_exit_receipt(&*runner, Path::new(&spec.cwd), launch_id).await
    }

    async fn daemon_reach(&self, spec: TerminalSessionTarget<'_>) -> Result<Option<String>, String> {
        if !self.state.agentless_ssh.contains_key(&spec.env_ref) {
            return Ok(None);
        }
        // Also reconnects, or bootstraps, the vessel's agent for sessions
        // that outlived a controller restart. An agent that cannot be reached
        // is a reason, not a log line.
        match self.vessel_for_spec(spec).await {
            Ok(_) => Ok(None),
            Err(error) => Ok(Some(format!("vessel agent unavailable: {error}"))),
        }
    }

    async fn observe_attention(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<Option<TerminalObservation>, String> {
        let pool = self.pool_for_spec(spec).await?;
        if let TerminalSessionSource::Agent { selector, .. } = &spec.source {
            let harness = match spec.harness {
                Some(harness) => harness.to_owned(),
                None => CapabilityTable::seeded().resolve_selector(selector)?.adapter,
            };
            if matches!(harness.as_str(), "codex" | "claude-code") {
                pool.ensure_recognizer(session_id, &harness)
                    .await
                    .map_err(|error| format!("cannot attach {harness} recognizer to {session_id}: {error}"))?;
            }
        }
        let adapter = self.adapter_for_spec(spec).await?;
        observe_terminal_screen(&*pool, adapter.as_deref(), session_id, Utc::now()).await
    }

    async fn agent_exit_failure(
        &self,
        session_id: &str,
        spec: TerminalSessionTarget<'_>,
        exit_code: i32,
    ) -> Result<Option<String>, String> {
        if exit_code == 0 {
            return Ok(None);
        }
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let adapter = self.session_adapter(spec, &requirement.adapter).await?;
        let screen = match self.pool_for_spec(spec).await {
            Ok(pool) => match pool.capture_screen(session_id).await {
                Ok(screen) => screen.unwrap_or_default(),
                Err(error) => {
                    debug!(%error, %session_id, exit_code, "cannot capture exited agent diagnostic");
                    String::new()
                }
            },
            Err(error) => {
                debug!(%error, %session_id, exit_code, "cannot resolve exited agent terminal pool");
                String::new()
            }
        };
        Ok(adapter.classify_exit_failure(exit_code, &screen))
    }

    async fn observe_failure(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<Option<String>, String> {
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let adapter = self.session_adapter(spec, &requirement.adapter).await?;
        let pool = self.pool_for_spec(spec).await?;
        let stable = pool
            .list_sessions()
            .await?
            .into_iter()
            .find(|session| session.session_name == session_id)
            .and_then(|session| session.screen_activity)
            == Some(ScreenActivity::Stable);
        if !stable {
            return Ok(None);
        }
        let screen = match pool.capture_screen(session_id).await {
            Ok(Some(screen)) => screen,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::debug!(%session_id, %error, "could not capture terminal screen for failure observation");
                return Ok(None);
            }
        };
        let Some(reason) = adapter.classify_screen_failure(&screen) else { return Ok(None) };

        Ok(Some(format!(
            "{} authentication failed for the selected credential in environment {}: {reason}",
            requirement.adapter, spec.env_ref
        )))
    }

    async fn deliver_message(
        &self,
        session_id: &str,
        spec: TerminalSessionTarget<'_>,
        message: &str,
        readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        let TerminalSessionSource::Agent { .. } = &spec.source else {
            return Err("crew message delivery requires an agent terminal".to_string());
        };
        match lookup_terminal_delivery(&self.state.terminal_deliveries, session_id, message) {
            TerminalDeliveryLookup::InFlight => return Ok(TerminalDeliveryOutcome::Pending),
            TerminalDeliveryLookup::Taken(delivery) if delivery.message == message => {
                return match delivery.task.await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        warn!(%session_id, %error, "delivery task failed with an ambiguous submission outcome");
                        Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed))
                    }
                }
            }
            TerminalDeliveryLookup::Taken(delivery) => {
                delivery.task.abort();
                let _ = delivery.task.await;
            }
            TerminalDeliveryLookup::Vacant => {}
        };
        let pool = self.pool_for_spec(spec).await?;
        let adapter = self.adapter_for_spec(spec).await?;
        let session_id_owned = session_id.to_string();
        let message_owned = message.to_string();
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        // Recheck under the insertion lock: another controller may reserve the
        // session while an old replacement task is being drained.
        if deliveries.contains_key(session_id) {
            return Ok(TerminalDeliveryOutcome::Pending);
        }
        let task =
            tokio::spawn(
                async move { deliver_and_confirm(&*pool, adapter.as_deref(), &session_id_owned, &message_owned, readiness).await },
            );
        deliveries.insert(
            session_id.to_string(),
            PendingTerminalDelivery { message_batch: None, message: message.to_string(), task, engine_turn: None },
        );
        Ok(TerminalDeliveryOutcome::Pending)
    }

    async fn teardown_pending(&self, session_id: &str) -> bool {
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        let Some(delivery) = deliveries.get_mut(session_id) else { return false };
        let finished = delivery.task.is_finished();
        let Some(turn) = delivery.engine_turn.as_mut() else { return false };
        turn.stop.cancel();
        if finished {
            return false;
        }
        let since = *turn.teardown_since.get_or_insert_with(tokio::time::Instant::now);
        since.elapsed() < ENGINE_TURN_SETTLE_FOR
    }

    async fn kill_session(&self, session_id: &str, spec: TerminalSessionTarget<'_>) -> Result<(), String> {
        {
            // A settled engine turn stays reserved: the delivery adapter records its
            // receipt. Any other reservation, or an engine turn that outlasted its
            // settle bound, is aborted.
            let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
            if deliveries.get(session_id).is_some_and(|delivery| delivery.engine_turn.is_none() || !delivery.task.is_finished()) {
                let delivery = deliveries.remove(session_id).expect("reserved delivery");
                if delivery.engine_turn.is_some() {
                    warn!(%session_id, "an engine turn did not settle before teardown; aborting it");
                }
                delivery.task.abort();
            }
        }
        let pool = self.pool_for_spec(spec).await?;
        if pool.tracks_session_liveness() {
            match pool.list_sessions().await {
                Ok(sessions) => {
                    let Some(session) = sessions.iter().find(|session| session.session_name == session_id) else {
                        return Ok(());
                    };
                    if session.status == TerminalStatus::Running {
                        if let TerminalSessionSource::Agent { context, .. } = &spec.source {
                            warn!(%session_id, convoy = %context.convoy, vessel = %context.vessel_ref, "convoy teardown is terminating an attached terminal session");
                        } else {
                            warn!(%session_id, "convoy teardown is terminating an attached terminal session");
                        }
                    }
                }
                Err(error) => warn!(%session_id, %error, "could not inspect terminal session before teardown; attempting kill"),
            }
        }
        pool.kill_session(session_id).await
    }

    async fn cleanup_session_artifacts(&self, spec: TerminalSessionTarget<'_>) -> Result<(), String> {
        let TerminalSessionSource::Agent { selector, brief, .. } = &spec.source else {
            return Ok(());
        };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let adapter = self.session_adapter(spec, &requirement.adapter).await?;

        let mut roots = BTreeSet::from([spec.cwd.clone()]);
        roots.extend(brief.copies.iter().cloned());
        for root in roots {
            adapter.cleanup(&ExecutionEnvironmentPath::new(root), brief).await?;
        }
        Ok(())
    }
}

impl TerminalControllerRuntime {
    pub(super) fn runner_for_env(&self, env_ref: &str) -> Result<Arc<dyn CommandRunner>, String> {
        self.state
            .daemon
            .command_runner_for_environment_ref(env_ref)
            .ok_or_else(|| format!("command runner unavailable for environment {env_ref}"))
    }

    pub(super) fn registry_for_env(&self, env_ref: &str) -> Result<Arc<ProviderRegistry>, String> {
        if env_ref == self.state.host_direct_environment_name {
            return Ok(Arc::clone(&self.state.local_registry));
        }
        self.state
            .daemon
            .resolve_environment_ref(env_ref)
            .and_then(|environment| self.state.daemon.environment_registry_for_environment(&environment.id))
            .ok_or_else(|| format!("provider registry unavailable for environment {env_ref}"))
    }

    /// #3792: the prepared vessel a session on an agentless host runs in, and
    /// `None` on every other environment. Both agent and tool crews name a vessel.
    pub(super) async fn vessel_for_spec(&self, spec: TerminalSessionTarget<'_>) -> Result<Option<Arc<VesselAccess>>, String> {
        let Some(profile) = self.state.agentless_ssh.get(&spec.env_ref) else { return Ok(None) };
        let vessel = spec.vessel_ref.ok_or_else(|| format!("agentless host {} requires a session vessel identity", spec.env_ref))?;
        self.state.vessel_access(profile, vessel).await.map(Some)
    }

    /// The vessel agent a session's access layer runs through: the agentless host's
    /// vessel agent, or the provisioned environment's.
    async fn vessel_agent(&self, spec: TerminalSessionTarget<'_>, vessel: Option<&VesselAccess>) -> Result<Arc<dyn VesselAgent>, String> {
        if let Some(access) = vessel {
            return Ok(access.handle.agent());
        }
        // Provisioned environments register under their resource name, so the
        // `env_ref` is their handle id; host environments fall through to the
        // resolved runner below.
        let id = EnvironmentId::new(&spec.env_ref);
        let handle = self
            .state
            .provisioned_environments
            .lock()
            .await
            .values()
            .find(|active| active.handle.id() == &id)
            .map(|active| active.handle.clone());
        Ok(match handle {
            Some(handle) => handle.agent(),
            // An environment without a provisioned handle is its host: its agent
            // works in process, through the host runner.
            None => Arc::new(RunnerVesselAgent(self.runner_for_env(&spec.env_ref)?)),
        })
    }

    pub(super) async fn runner_for_spec(&self, spec: TerminalSessionTarget<'_>) -> Result<Arc<dyn CommandRunner>, String> {
        match self.vessel_for_spec(spec).await? {
            Some(access) => Ok(access.runner()),
            None => self.runner_for_env(&spec.env_ref),
        }
    }

    pub(super) async fn registry_for_spec(&self, spec: TerminalSessionTarget<'_>) -> Result<Arc<ProviderRegistry>, String> {
        match self.vessel_for_spec(spec).await? {
            Some(access) => Ok(Arc::clone(&access.registry)),
            None => self.registry_for_env(&spec.env_ref),
        }
    }

    pub(super) async fn adapter_for_spec(&self, spec: TerminalSessionTarget<'_>) -> Result<Option<Arc<dyn AgentAdapter>>, String> {
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        self.session_adapter(spec, &requirement.adapter).await.map(Some)
    }

    /// The adapter a session's harness runs under: the place's discovered one, or, on an
    /// agentless vessel, whose access layer delivers the pinned harness (#3909), the
    /// built-in adapter for it even when the host has none installed. Observation and
    /// cleanup never run its binary; launch uses the executable delivery returns.
    async fn session_adapter(&self, spec: TerminalSessionTarget<'_>, adapter: &str) -> Result<Arc<dyn AgentAdapter>, String> {
        let unavailable = || format!("agent adapter {adapter} unavailable for environment {}", spec.env_ref);
        if let Some(found) = self.registry_for_spec(spec).await?.agent_adapters.get(adapter) {
            return Ok(Arc::clone(found));
        }
        let Some(vessel) = self.vessel_for_spec(spec).await? else { return Err(unavailable()) };
        let name = flotilla_agent::adapter::harness_binary(adapter).ok_or_else(unavailable)?;
        AgentAdapterRegistry::delivered(adapter, Path::new(name), &[], vessel.runner()).ok_or_else(unavailable)
    }

    pub(super) async fn pool_for_spec(&self, spec: TerminalSessionTarget<'_>) -> Result<Arc<dyn TerminalPool>, String> {
        let registry = self.registry_for_spec(spec).await?;
        registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))
    }
}

pub(super) const CREW_SESSION_SIZE: TerminalSize = TerminalSize::new(200, 50);
