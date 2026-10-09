//! Terminal resource runtime and confirmed input delivery.

use super::credentials::{agent_material_environment, RuntimeSessionCapabilities};
use super::state::ControllerRuntimeState;
use crate::blob_store::BlobDigest;
use crate::blob_store::BlobStore;
use async_trait::async_trait;
use chrono::Utc;
use flotilla_controllers::reconcilers::{
    TerminalDeliveryFailure, TerminalDeliveryOutcome, TerminalDeliveryReadiness, TerminalLiveness, TerminalObservation, TerminalRuntime,
    TerminalRuntimeState,
};
use flotilla_core::crew_capabilities::SessionCapabilitySource;
use flotilla_core::{
    agent_adapter::{AgentAdapter, AgentLaunchRequest, CapabilityTable},
    path_context::ExecutionEnvironmentPath,
    providers::{
        registry::ProviderRegistry,
        terminal::{ScreenActivity, TerminalPool, TerminalSessionLiveness, TerminalSize},
        CommandRunner,
    },
};
use flotilla_credentials::crew_git_identity_environment;
use flotilla_protocol::{ConfiguredResourceLimits, EnvironmentId, TerminalStatus};
use flotilla_resources::{
    Checkout, Convoy, Environment, FulfilmentKind, ResourceBackend, ResourceError, ResourceObject, TerminalAttentionSource,
    TerminalAttentionState, TerminalOccupancy, TerminalSession, TerminalSessionPhase, TerminalSessionSource, TerminalSessionSpec, Vessel,
    CREDENTIAL_PERMISSIONS_SESSION_TAG, CREDENTIAL_REF_SESSION_TAG, CREDENTIAL_SCOPES_SESSION_TAG,
};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub(super) struct PendingTerminalDelivery {
    pub(super) message_batch: Option<String>,
    pub(super) message: String,
    pub(super) task: JoinHandle<Result<TerminalDeliveryOutcome, String>>,
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

pub(super) const DELIVERY_CONFIRMATION_POLL: Duration = Duration::from_millis(200);

pub(super) const DELIVERY_CONFIRMATION_GRACE: Duration = Duration::from_secs(4);

pub(super) const DELIVERY_CONFIRMATION_STABLE_FOR: Duration = Duration::from_secs(1);

pub(super) const DELIVERY_READY_POLLS: usize = 150;

// Screen classification is the same evidence for observation, turn release, and
// submission. Pixel redraws and principal attachment status do not define turns.
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
    if let Some(adapter) = adapter {
        match pool.capture_screen(session_id).await {
            Ok(Some(screen)) => {
                output_digest = adapter.screen_output_digest(&screen);
                state = adapter.classify_screen_attention(&screen);
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
        attention: state.map(|state| flotilla_resources::TerminalAttention { state, as_of: now, source: TerminalAttentionSource::Screen }),
        occupancy,
    }))
}

pub(super) async fn wait_for_delivery_ready(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
    readiness: TerminalDeliveryReadiness,
) -> Result<bool, String> {
    let mut polls = 0;
    loop {
        let observation = observe_terminal_screen(pool, adapter, session_id, Utc::now())
            .await?
            .ok_or_else(|| format!("terminal session {session_id} disappeared before message delivery"))?;
        // Pools without any attention evidence retain best-effort delivery.
        if observation.attention.is_none_or(|attention| attention.state == TerminalAttentionState::Idle) {
            return Ok(true);
        }
        polls += 1;
        // Startup alone has a deadline (and reports StartupNotReady). A queued
        // mid-session turn waits for its boundary without that startup timeout.
        if readiness == TerminalDeliveryReadiness::Startup && polls >= DELIVERY_READY_POLLS {
            return Ok(false);
        }
        tokio::time::sleep(DELIVERY_CONFIRMATION_POLL).await;
    }
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
    readiness: TerminalDeliveryReadiness,
    clear_before_delivery: bool,
) -> Result<TerminalDeliveryOutcome, String> {
    // This confirmation task is in-memory. A crash after PTY acceptance and
    // before its durable receipt/hold can still cause a resend after restart;
    // crash-atomic delivery needs a pre-write intent and harness acknowledgement.
    // Readiness only observes the session and never writes input, so any error
    // from wait_for_delivery_ready is known-unsent and safe for bounded retry.
    // PTY input sent during agent startup can be consumed before the TUI has
    // enabled its composer input modes. A newly launched agent reports active,
    // so wait for its first idle observation before sending delivery bytes.
    match wait_for_delivery_ready(pool, adapter, session_id, readiness).await {
        Ok(true) => {}
        result => {
            warn!(%session_id, ?result, "agent session readiness failed before any message input was sent");
            return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady));
        }
    }
    submit_and_confirm(pool, adapter, session_id, message, clear_before_delivery).await
}

/// Readiness and freshness are both pre-write steps under one hold deadline.
pub(super) async fn deliver_guarded_and_confirm(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session: &str,
    text: &str,
    inbox: &flotilla_resources::MessageInbox,
    members: &[String],
) -> Result<TerminalDeliveryOutcome, String> {
    let deadline =
        tokio::time::Instant::now() + flotilla_resources::delivery_hold::DELIVERY_HOLD_FOR.to_std().expect("positive hold bound");
    let ready =
        tokio::time::timeout_at(deadline, wait_for_delivery_ready(pool, adapter, session, TerminalDeliveryReadiness::TurnBoundary)).await;
    if ready.is_err() {
        // Stop waiting; the durable intent remains for the operator gate.
        return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
    }
    if !matches!(ready, Ok(Ok(true))) {
        return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady));
    }
    match tokio::time::timeout_at(deadline, inbox.validate_delivery_members(members, Utc::now())).await {
        Ok(Ok(true)) => {}
        Err(_) => return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed)),
        Ok(Err(error)) => {
            warn!(%session, %error, "message freshness is unavailable before input; holding the unsent batch");
            return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady));
        }
        Ok(Ok(false)) => return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::StartupNotReady)),
    }
    // timeout_at polls an immediately-ready future before its timer; enforce
    // the bound even if readiness and validation complete at the deadline.
    if tokio::time::Instant::now() >= deadline {
        return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
    }
    // No second readiness wait may separate validation from terminal input.
    submit_and_confirm(pool, adapter, session, text, false).await
}

pub(super) async fn submit_and_confirm(
    pool: &dyn TerminalPool,
    adapter: Option<&dyn AgentAdapter>,
    session_id: &str,
    message: &str,
    clear_before_delivery: bool,
) -> Result<TerminalDeliveryOutcome, String> {
    let submission =
        if clear_before_delivery { pool.retry_delivery(session_id, message).await } else { pool.deliver(session_id, message).await };
    // A transport error may occur after a partial write, or after acceptance
    // with the reply lost. It does not prove that retyping is safe.
    if let Err(error) = submission {
        warn!(%session_id, %error, "message write has an ambiguous outcome; holding without resending");
        return Ok(TerminalDeliveryOutcome::Unconfirmed(TerminalDeliveryFailure::SubmissionUnconfirmed));
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

    async fn brief_ready(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<bool, String> {
        let TerminalSessionSource::Agent { brief, .. } = &spec.source else { return Ok(true) };
        let Some(digest) = &brief.artifact_digest else { return Ok(true) };
        let store = self.state.blob_store.as_ref().ok_or("brief blob store unavailable")?;
        Ok(store.get(&BlobDigest::parse(digest)?).await?.is_some())
    }

    async fn cleat_endpoint(
        &self,
        session_id: &str,
        spec: &TerminalSessionSpec,
    ) -> Result<Option<flotilla_protocol::result_set::CleatEndpoint>, String> {
        let pool = self.pool_for_spec(spec)?;
        pool.cleat_endpoint(session_id).await
    }

    async fn ensure_session(
        &self,
        name: &str,
        spec: &TerminalSessionSpec,
        tags: &[flotilla_resources::TerminalSessionTag],
    ) -> Result<TerminalRuntimeState, String> {
        let registry = self.registry_for_env(&spec.env_ref)?;
        let pool = registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))?;

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
        let pool_tags = tags
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
                let runner = self.runner_for_env(&spec.env_ref)?;
                store
                    .prepare_scoped_with_permissions(&spec.env_ref, &credential_refs, &credential_scopes, &credential_permissions, runner)
                    .await?
            }
            None if credential_refs.is_empty() => Vec::new(),
            None => return Err("host-local credential store unavailable".to_string()),
        };
        let mut declared_env = spec.env.clone();
        declared_env.extend(credential_env);
        if spec.env_ref == self.state.host_direct_environment_name {
            if let Some(socket) = self.state.daemon.daemon_socket_path().await {
                declared_env.insert("FLOTILLA_DAEMON_SOCKET".into(), socket.display().to_string());
            }
        }
        credential_env = declared_env.into_iter().collect();
        let (command, mut env, crew) = match &spec.source {
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
                let adapter = registry
                    .agent_adapters
                    .get(&requirement.adapter)
                    .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;
                let vcs = if spec.env_ref == self.state.host_direct_environment_name {
                    self.state.daemon.local_vcs_for_checkout(cwd.as_path()).await?
                } else {
                    self.state.daemon.vcs_for_checkout(&EnvironmentId::new(&spec.env_ref), cwd.as_path()).await?
                };
                if let Some(material) = &self.state.agent_material {
                    let environment =
                        self.state.daemon.resource_backend().including_replicas::<Environment>(&context.namespace).get(&spec.env_ref).await;
                    let docker = match environment {
                        Ok(environment) => environment.object.spec.docker,
                        Err(ResourceError::NotFound { .. }) => None,
                        Err(error) => return Err(error.to_string()),
                    };
                    if let Some(docker) = docker.filter(|docker| docker.env.contains_key("FLOTILLA_CREW_SKILLS")) {
                        let required = BTreeSet::from([requirement.adapter.clone()]);
                        let environment = agent_material_environment(material, &required, &docker.env, credential_env.iter().cloned())?;
                        let runner = self.runner_for_env(&spec.env_ref)?;
                        credential_env = material.crew_environment(&spec.role, &required, &environment, &*runner).await?;
                    }
                }
                if let Some(store) = &self.state.credential_store {
                    store.record_capability_endpoints(&spec.env_ref, name, &credential_env).await;
                    let card = async {
                        let credentials = store.credentials(&spec.env_ref, &credential_refs).await?;
                        flotilla_core::crew_capabilities::session_card(
                            &self.state.daemon.resource_backend(),
                            &context.namespace,
                            &context.convoy,
                            spec,
                            &credentials,
                            &RuntimeSessionCapabilities { state: Arc::downgrade(&self.state) }.endpoints(&spec.env_ref, name).await?,
                        )
                        .await
                    }
                    .await;
                    let card = match card {
                        Ok(card) => card,
                        Err(error) => {
                            warn!(session = %name, %error, "failed to construct advisory launch capability card");
                            format!("{}\n\nCapabilities are currently unavailable. Run `flotilla crew capabilities` when unsure or after the service recovers.\n", flotilla_core::crew_capabilities::CAPABILITIES_HEADING.trim_start())
                        }
                    };
                    if let Err(error) = flotilla_core::crew_capabilities::observe_launch_card(
                        &self.state.daemon.resource_backend(),
                        &context.namespace,
                        name,
                        &card,
                    )
                    .await
                    {
                        warn!(session = %name, %error, "failed to persist launch capability observation");
                    }
                    materialized_brief.content.push_str("\n\n");
                    materialized_brief.content.push_str(&card);
                }
                adapter.prepare_with_vcs(&cwd, &materialized_brief, &credential_env, vcs.as_ref()).await?;
                for copy_root in &brief.copies {
                    let copy_root = ExecutionEnvironmentPath::new(copy_root);
                    if copy_root != cwd {
                        let vcs = if spec.env_ref == self.state.host_direct_environment_name {
                            self.state.daemon.local_vcs_for_checkout(copy_root.as_path()).await?
                        } else {
                            self.state.daemon.vcs_for_checkout(&EnvironmentId::new(&spec.env_ref), copy_root.as_path()).await?
                        };
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
                (flotilla_core::agent_process::monitored_command(&plan.command, &crew.id), env, Some(crew))
            }
        };
        env.push(("CARGO_INCREMENTAL".to_string(), "0".to_string()));

        let is_agent_session = matches!(spec.source, TerminalSessionSource::Agent { .. });
        let host_ref = if spec.env_ref == self.state.host_direct_environment_name {
            Some(self.state.local_host_ref.as_str())
        } else {
            self.state.agentless_ssh.get(&spec.env_ref).map(|profile| profile.provisioning.host_id.as_str())
        };
        let mut configured_limits = None;
        if let Some(host_ref) = host_ref {
            let jobs = self.state.rust_build_jobs(host_ref).await?;
            configured_limits = Some(ConfiguredResourceLimits { cpus: None, build_jobs: Some(jobs), linker_threads: Some(jobs) });
            let wrapper = self.state.rustc_wrapper_for_environment(&spec.env_ref).await?;
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
        // A dead generation may retain a recording with the old ID. Launch
        // under a fresh ID so cleat cannot resolve the name ambiguously, then
        // mark the old recording for retention after the launch succeeds.
        let recovered_from_lost = matches!(pool.session_liveness(name).await?, TerminalSessionLiveness::Lost(_));
        let session_id = if recovered_from_lost { format!("{name}-{}", uuid::Uuid::new_v4()) } else { name.to_string() };
        if is_agent_session && pool.list_sessions().await?.iter().any(|session| session.session_name == session_id) {
            pool.kill_session(&session_id).await?;
        }
        let initial_size = is_agent_session.then_some(CREW_SESSION_SIZE);
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

    async fn session_liveness(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<TerminalLiveness, String> {
        let pool = match self.pool_for_spec(spec) {
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
        spec: &TerminalSessionSpec,
        crew: &flotilla_resources::CrewSessionStatus,
    ) -> Result<Option<i32>, String> {
        let runner = self.runner_for_env(&spec.env_ref)?;
        self.state
            .exit_receipts
            .observe(&spec.env_ref, &*runner, Path::new(&spec.cwd).join(flotilla_core::agent_process::exit_receipt(&crew.id)), || async {
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
                        if session.spec.env_ref != spec.env_ref || !matches!(session.spec.source, TerminalSessionSource::Agent { .. }) {
                            return None;
                        }
                        let status = session.status?;
                        if status.phase != TerminalSessionPhase::Running {
                            return None;
                        }
                        status.crew.map(|crew| Path::new(&session.spec.cwd).join(flotilla_core::agent_process::exit_receipt(&crew.id)))
                    })
                    .collect())
            })
            .await
    }

    async fn remove_exit_receipt(&self, spec: &TerminalSessionSpec, launch_id: &str) -> Result<(), String> {
        let runner = self.runner_for_env(&spec.env_ref)?;
        flotilla_core::agent_process::remove_exit_receipt(&*runner, Path::new(&spec.cwd), launch_id).await
    }

    async fn observe_attention(&self, session_id: &str, spec: &TerminalSessionSpec) -> Result<Option<TerminalObservation>, String> {
        let pool = self.pool_for_spec(spec)?;
        let adapter = self.adapter_for_spec(spec)?;
        observe_terminal_screen(&*pool, adapter.as_deref(), session_id, Utc::now()).await
    }

    async fn agent_exit_failure(&self, session_id: &str, spec: &TerminalSessionSpec, exit_code: i32) -> Result<Option<String>, String> {
        if exit_code == 0 {
            return Ok(None);
        }
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let registry = self.registry_for_env(&spec.env_ref)?;
        let adapter = registry
            .agent_adapters
            .get(&requirement.adapter)
            .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;
        let screen = match self.pool_for_spec(spec) {
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

    async fn observe_failure(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<Option<String>, String> {
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let registry = self.registry_for_env(&spec.env_ref)?;
        let adapter = registry
            .agent_adapters
            .get(&requirement.adapter)
            .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;
        let pool = self.pool_for_spec(spec)?;
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

        // Static material means every crew runs the same central login, so the
        // operator-facing name is that login's path, not a leased slot: if a
        // crew saw an auth failure, the host's refresher is what needs looking at.
        let material = match &self.state.agent_material {
            Some(registry) => format!("the central Codex credential {}", registry.codex_credential_source().display()),
            None => format!("the central Codex credential (no material registry for environment {})", spec.env_ref),
        };
        Ok(Some(format!("Codex authentication failed for {material} in environment {}: {reason}", spec.env_ref)))
    }

    async fn deliver_message(
        &self,
        session_id: &str,
        spec: &TerminalSessionSpec,
        message: &str,
        readiness: TerminalDeliveryReadiness,
    ) -> Result<TerminalDeliveryOutcome, String> {
        let TerminalSessionSource::Agent { .. } = &spec.source else {
            return Err("crew message delivery requires an agent terminal".to_string());
        };
        let clear_before_delivery = match lookup_terminal_delivery(&self.state.terminal_deliveries, session_id, message) {
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
                true
            }
            TerminalDeliveryLookup::Vacant => false,
        };
        let pool = self.pool_for_spec(spec)?;
        let adapter = self.adapter_for_spec(spec)?;
        let session_id_owned = session_id.to_string();
        let message_owned = message.to_string();
        let mut deliveries = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned");
        // Recheck under the insertion lock: another controller may reserve the
        // session while an old replacement task is being drained.
        if deliveries.contains_key(session_id) {
            return Ok(TerminalDeliveryOutcome::Pending);
        }
        let task = tokio::spawn(async move {
            deliver_and_confirm(&*pool, adapter.as_deref(), &session_id_owned, &message_owned, readiness, clear_before_delivery).await
        });
        deliveries.insert(session_id.to_string(), PendingTerminalDelivery { message_batch: None, message: message.to_string(), task });
        Ok(TerminalDeliveryOutcome::Pending)
    }

    async fn kill_session(&self, session_id: &str, spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        if let Some(delivery) = self.state.terminal_deliveries.lock().expect("terminal deliveries lock poisoned").remove(session_id) {
            delivery.task.abort();
        }
        let pool = self.pool_for_spec(spec)?;
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

    async fn cleanup_session_artifacts(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<(), String> {
        let TerminalSessionSource::Agent { selector, brief, .. } = &spec.source else {
            return Ok(());
        };
        let registry = self.registry_for_env(&spec.env_ref)?;
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let adapter = registry
            .agent_adapters
            .get(&requirement.adapter)
            .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))?;

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
            .environment_registry_for_environment(&EnvironmentId::new(env_ref.to_string()))
            .ok_or_else(|| format!("provider registry unavailable for environment {env_ref}"))
    }

    pub(super) fn adapter_for_spec(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<Option<Arc<dyn AgentAdapter>>, String> {
        let TerminalSessionSource::Agent { selector, .. } = &spec.source else { return Ok(None) };
        let requirement = CapabilityTable::seeded().resolve_selector(selector)?;
        let registry = self.registry_for_env(&spec.env_ref)?;
        registry
            .agent_adapters
            .get(&requirement.adapter)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("agent adapter {} unavailable for environment {}", requirement.adapter, spec.env_ref))
    }

    pub(super) fn pool_for_spec(&self, spec: &flotilla_resources::TerminalSessionSpec) -> Result<Arc<dyn TerminalPool>, String> {
        let registry = self.registry_for_env(&spec.env_ref)?;
        registry
            .terminal_pools
            .get(&spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", spec.pool, spec.env_ref))
    }
}

pub(super) const CREW_SESSION_SIZE: TerminalSize = TerminalSize::new(200, 50);
