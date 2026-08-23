//! In-process daemon implementation.
//!
//! `InProcessDaemon` owns repos, runs refresh loops, executes commands,
//! and broadcasts events — all within the same process.

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use flotilla_protocol::{
    arg::{flatten, Arg},
    commands::{AttachMode, RepositoryIdentityChange},
    qualified_path::{HostId, QualifiedPath},
    result_set::{CheckoutRow, ConvoyChangeRequest, ConvoyRow, ResultSet, Rows},
    AttachBinding, CanonicalHostId, Change, Command, CommandAction, CommandValue, ConvoyDispatchRegard, ConvoyExplanation,
    CredentialAttention, CredentialAttentionSeverity, CrewAttention, CrewCommandContext, CrewListMember, CrewListResponse, DaemonEvent,
    DispatchQueueResponse, DispatchQueueRow, EntryOp, EnvironmentId, EvidenceFreshness, ExplainedChangeRequest, ExplainedCheckout,
    ExplainedCondition, ExplainedCrewDelivery, ExplainedDecisionLedger, ExplainedLeafFiring, ExplainedSettlement, ExplainedSubscription,
    ExplainedUnmetExpectation, FleetHealthResponse, FleetHostRow, FleetHostStaleness, FleetListResponse, FleetListRow,
    FleetObservationAgreement, FleetReplicaSnapshot, FleetReplicaStatus, FleetStaleness, HostListResponse, HostName, HostProviderStatus,
    HostProvidersResponse, HostStatusResponse, HostSummary, ManagedTerminal, NodeId, NodeInfo, PeerConnectionState, PlacementDecision,
    PlacementRefusal, PlacementTargetHost, PlacementViableCandidate, PrincipalRef, ProjectListEntry, ProjectListRepository,
    ProjectListResponse, ProviderData, ProviderInfo, QueryCursor, RepoDelta, RepoIdentity, RepoInfo, RepoProvidersResponse, RepoSummary,
    ResolvedAttachAction, ResolvedAttachPlan, ResourceCursor, ResourceJsonResponse, ResourceReadEnvelope, ResourceReadRecord,
    ResourceRecordProvenance, ResourceRecordType, ResourceRef, StatusResponse, StepStatus, StreamKey, SurfaceDeclaration, TopologyResponse,
    TopologyRoute, ViewAddress, AGENT_ADAPTER_PROVIDER_CATEGORY, TERMINAL_POOL_PROVIDER_CATEGORY,
};
use flotilla_resources::{
    api_version, apply_resource_document, apply_status_patch as apply_resource_status_patch,
    apply_status_patch_checked as apply_resource_status_patch_checked, bound_change_request_record_name,
    controller::delete_lifecycle_owned_matching, ensure_repository, evaluate_landing_settlement, expected_change_request_leaves,
    expected_checkout_refs, external_patches as convoy_external_patches, get_resource_kind_including_replicas, list_resource_kind,
    list_resource_kind_including_replicas, normalize_project_spec, repository_display_labels, resolve_project_issue_sources,
    terminal_session_attach_target, watch_resource_kind, watch_resource_kind_from, watch_resource_kind_including_replicas,
    watch_resource_kind_replica_sources, BoundChangeRequest, Checkout as ResourceCheckout, CheckoutIntegrationStatus,
    CheckoutPhase as ResourceCheckoutPhase, CheckoutSpec as ResourceCheckoutSpec, CheckoutStatus as ResourceCheckoutStatus, Clock,
    ConditionValue, Convoy as ResourceConvoy, ConvoyEnsure, ConvoyEnsureCondition, ConvoyEnsureHoldReason, ConvoyEnsureSpec,
    ConvoyEnsureStatusPatch, ConvoyIssue, ConvoyPhase, ConvoyRepositorySpec, ConvoySpec, ConvoyStatus, ConvoyStatusPatch,
    CredentialConsumer, CredentialGrant, CredentialSpec, CrewCompletionPending, CrewSource, CrewWorkPhase, Demand as ResourceDemand,
    DemandExpiry, DemandExpiryDisposition, DemandKind, DemandSpec, DemandState, Environment as ResourceEnvironment, EnvironmentPhase,
    HoldAct, Host as ResourceHost, HostStatus as ResourceHostStatus, InMemoryBackend, InputMeta, InputValue, IntegrationCondition,
    IssueSnapshot, IssueSourceResolution, IssueSourceUnavailable, LifecycleAuthority, ObservedCheckoutSpec as ResourceObservedCheckoutSpec,
    PendingBrief, PlacementPolicy, PlacementPolicySpec, Presentation as ResourcePresentation, Project, ProjectRepositoryRole,
    ProjectRepositorySpec, ProjectSpec, ReadResourceObject, Repository, RepositoryKey, RepositorySpec, Resource, ResourceBackend,
    ResourceError, ResourceObject, ResourceProvenance, SettlementMode, SystemClock, TerminalAttentionState, TerminalBrief,
    TerminalCrewContext, TerminalCrewMessage, TerminalSession as ResourceTerminalSession, TerminalSessionIdentity,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, TerminalSessionStatus, TerminalSessionStatusPatch,
    TurnDeliveryRung, UnmetSettlementExpectation, Vessel, WatchEvent, WatchStart, WorkCompletionAuthority, WorkPhase as ResourceWorkPhase,
    WorkflowTemplate, WorkflowTemplateSpec, ACTUATOR_SOURCE_ROOT_ANNOTATION, CONVOY_LABEL, CREDENTIAL_REFS_ANNOTATION,
    CREDENTIAL_SCOPES_ANNOTATION, DRIVER_ADMISSION_CONDITION_TYPE, GENERATION_LABEL, HEARTBEAT_READY_TTL_SECS, MANAGED_BY_LABEL,
    PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use futures::{FutureExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    agent_adapter::{required_agent_adapters, CapabilityTable},
    aggregator_projection::AggregatorProjectionState,
    checkout_integration::{
        checkout_path_from_status_and_spec, convoy_change_request_id_for_checkout, inspect_checkout_integration,
        inspect_convoy_checkout_integration, LANDING_EVIDENCE_TTL,
    },
    config::{ConfigStore, RemoteHostConfig, StaticEnvironmentConfig},
    daemon::{DaemonHandle, QuerySubscription},
    environment_manager::EnvironmentManager,
    executor,
    executor::checkout::{checkout_matches_scope, CheckoutResolutionScope},
    hop_chain::{
        environment::DockerEnvironmentHopResolver, remote::ssh_resolver_from_config, resolver::HopResolver,
        terminal::NoopTerminalHopResolver, Hop, HopPlan, ResolutionContext,
    },
    host_identity::{
        resolve_local_environment_state_dir, resolve_local_host_id, resolve_local_node_id, resolve_or_create_environment_id,
        resolve_or_create_remote_environment_id, resolve_or_create_remote_host_id,
    },
    host_registry::HostCounts,
    leaf_engine::{LeafSubscriptionTable, LeafWatcher},
    model::{provider_names_from_registry, repo_name, RepoModel},
    ops_entry::{
        parse_operational_entry, OperationalEntryDefinition, ENSURED_FROM_ANNOTATION, ENSURE_PROVENANCE_ANNOTATION,
        MATERIALIZED_PROJECT_ANNOTATION, PRESENTS_AS_ANNOTATION, SOURCE_COMMIT_ANNOTATION, SOURCE_ENTRY_PATH_ANNOTATION,
        SOURCE_REPOSITORY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION,
    },
    path_context::{canonical_or_original, DaemonHostPath, ExecutionEnvironmentPath},
    project_declaration::{
        parse_project_declaration, ProjectDeclaration, BOOTSTRAP_COMMIT_ANNOTATION, BOOTSTRAP_PATH_ANNOTATION,
        BOOTSTRAP_REPOSITORY_ANNOTATION, DECLARATION_FILE, DECLARATION_FILE_ANNOTATION,
    },
    providers::{
        ai_utility::{AiUtility, ConvoyNames},
        discovery::{
            discover_providers_with_host_scoped, run_host_detectors, DiscoveryResult, DiscoveryRuntime, EnvironmentAssertion,
            EnvironmentBag,
        },
        issue_tracker::{forge_issue_source, IssueProvider},
        registry::ProviderRegistry,
        ssh_runner::SshCommandRunner,
        types::RepoCriteria,
        ChannelLabel, CommandRunner,
    },
    regard_lifecycle::{RegardLifecycle, SurfaceGestureOutcome, DEFAULT_REGARD_DECAY_SECONDS, DEFAULT_REGARD_REFRESH_SECONDS},
    repo_state::{RepoRootState, RepoState},
    repository_inspection::{
        GitRepositoryInspector, OperationalEntriesInspection, ProjectDeclarationInspection, RepositoryInspection, RepositoryInspector,
    },
    step::{
        run_step_plan_with_remote_executor, RemoteStepBatchRequest, RemoteStepExecutor, RemoteStepProgressSink, StepOutcome, StepResolver,
    },
};

fn static_ssh_environment_id(config_key: &str) -> EnvironmentId {
    let mut encoded = String::with_capacity(config_key.len() * 2);
    for byte in config_key.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    let suffix = if encoded.is_empty() { "empty".to_string() } else { encoded };
    // Remote direct environments do not have a persisted remote identity yet.
    // Use a deterministic temporary id encoded directly from the daemon.toml
    // entry key bytes so distinct legal config keys remain injective in this tranche.
    EnvironmentId::new(format!("static-ssh-{suffix}"))
}

#[cfg(test)]
mod tests;

#[derive(bon::Builder)]
struct ResourceWatchCommandContext {
    backend: ResourceBackend,
    namespace: String,
    kind: String,
    name: Option<String>,
    include_replicas: bool,
    replica_sources: bool,
    cursor: Option<ResourceCursor>,
    command_id: u64,
    node_id: NodeId,
    repo_identity: RepoIdentity,
    event_tx: broadcast::Sender<DaemonEvent>,
    token: CancellationToken,
}

async fn run_resource_watch_command(context: ResourceWatchCommandContext) -> CommandValue {
    let resuming = context.cursor.is_some();
    let start = match context.cursor.as_ref().map(ResourceCursor::position).transpose() {
        Ok(position) => position.map(|(resource_version, generation)| match generation {
            Some(generation) => WatchStart::FromVersionInGeneration { generation, resource_version },
            None => WatchStart::FromVersion(resource_version),
        }),
        Err(message) => return CommandValue::Error { message },
    };
    let result = match (context.replica_sources, context.include_replicas, start) {
        (true, _, Some(_)) => Err(ResourceError::invalid("replica-source watches do not support cursor resume")),
        (true, _, None) => watch_resource_kind_replica_sources(&context.backend, &context.namespace, &context.kind).await,
        (false, true, Some(_)) => Err(ResourceError::invalid("include-replicas watches do not support cursor resume")),
        (false, true, None) => watch_resource_kind_including_replicas(&context.backend, &context.namespace, &context.kind).await,
        (false, false, Some(start)) => watch_resource_kind_from(&context.backend, &context.namespace, &context.kind, start).await,
        (false, false, None) => watch_resource_kind(&context.backend, &context.namespace, &context.kind).await,
    };
    let watch = match result {
        Ok(watch) => watch,
        Err(error) => return CommandValue::Error { message: error.to_string() },
    };

    let resource_kind = watch.kind;
    let plural = watch.plural;
    let namespace = watch.namespace;
    let initial_resource_version = watch.resource_version;
    let generation = watch.generation;
    let initial_cursor = ResourceCursor::from_position(initial_resource_version.clone(), generation.clone());
    if !resuming {
        let initial = watch
            .initial
            .into_iter()
            .filter_map(|event| resource_watch_record(event, &context.node_id).transpose())
            .collect::<Result<Vec<_>, _>>();
        let initial = match initial {
            Ok(initial) => initial.into_iter().filter(|record| resource_record_matches_name(record, context.name.as_deref())).collect(),
            Err(message) => return CommandValue::Error { message },
        };
        if context.token.is_cancelled() {
            return CommandValue::Cancelled;
        }
        send_resource_watch_event(
            &context.event_tx,
            context.command_id,
            &context.node_id,
            &context.repo_identity,
            resource_read_envelope(resource_kind.clone(), plural.clone(), namespace.clone(), initial_cursor.clone(), initial),
        );
    }
    send_resource_watch_event(
        &context.event_tx,
        context.command_id,
        &context.node_id,
        &context.repo_identity,
        resource_read_envelope(resource_kind.clone(), plural.clone(), namespace.clone(), initial_cursor, vec![ResourceReadRecord {
            record_type: ResourceRecordType::Bookmark,
            provenance: ResourceRecordProvenance::Local { node_id: context.node_id.clone() },
            object: None,
        }]),
    );

    let mut stream = watch.stream;
    loop {
        tokio::select! {
            _ = context.token.cancelled() => return CommandValue::Cancelled,
            event = stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        let resource_version = event["object"]["metadata"]["resourceVersion"]
                            .as_str()
                            .unwrap_or(&initial_resource_version)
                            .to_string();
                        let record = match resource_watch_record(event, &context.node_id) {
                            Ok(Some(record)) => record,
                            Ok(None) => continue,
                            Err(message) => return CommandValue::Error { message },
                        };
                        if resource_record_matches_name(&record, context.name.as_deref()) {
                            send_resource_watch_event(
                                &context.event_tx,
                                context.command_id,
                                &context.node_id,
                                &context.repo_identity,
                                resource_read_envelope(
                                    resource_kind.clone(),
                                    plural.clone(),
                                    namespace.clone(),
                                    ResourceCursor::from_position(resource_version, generation.clone()),
                                    vec![record],
                                ),
                            );
                        }
                    }
                    Some(Err(error)) => return CommandValue::Error { message: error.to_string() },
                    None => return CommandValue::Ok,
                }
            }
        }
    }
}

fn send_resource_watch_event(
    event_tx: &broadcast::Sender<DaemonEvent>,
    command_id: u64,
    node_id: &NodeId,
    repo_identity: &RepoIdentity,
    response: ResourceReadEnvelope,
) {
    let description = format!(
        "{} {}",
        response.records.first().map(|record| format!("{:?}", record.record_type)).unwrap_or_else(|| "CURRENT".to_string()),
        response.resource_kind
    );
    let _ = event_tx.send(DaemonEvent::CommandStepUpdate {
        command_id,
        node_id: node_id.clone(),
        repo_identity: repo_identity.clone(),
        repo: None,
        step_index: 0,
        step_count: 1,
        description,
        status: StepStatus::Produced { value: Box::new(CommandValue::ResourceWatchEvent(Box::new(response))) },
    });
}

fn resource_read_envelope(
    resource_kind: String,
    plural: String,
    namespace: String,
    cursor: ResourceCursor,
    records: Vec<ResourceReadRecord>,
) -> ResourceReadEnvelope {
    ResourceReadEnvelope { api_version: "flotilla.work/v1".to_string(), resource_kind, plural, namespace, cursor, records }
}

fn resource_record(record_type: ResourceRecordType, object: serde_json::Value, local_node_id: &NodeId) -> ResourceReadRecord {
    let annotations = object.get("metadata").and_then(|metadata| metadata.get("annotations"));
    let origin_root = annotations.and_then(|annotations| annotations.get("flotilla.work/origin-root")).and_then(|value| value.as_str());
    let last_synced_at =
        annotations.and_then(|annotations| annotations.get("flotilla.work/last-synced-at")).and_then(|value| value.as_str());
    let provenance = match (origin_root, last_synced_at) {
        (Some(origin_root), Some(last_synced_at)) => {
            ResourceRecordProvenance::Replica { origin_root: NodeId::new(origin_root), last_synced_at: last_synced_at.to_string() }
        }
        _ => ResourceRecordProvenance::Local { node_id: local_node_id.clone() },
    };
    ResourceReadRecord { record_type, provenance, object: Some(object) }
}

fn explained_provenance(provenance: &ResourceProvenance, local_node_id: &NodeId) -> ResourceRecordProvenance {
    match provenance {
        ResourceProvenance::Local => ResourceRecordProvenance::Local { node_id: local_node_id.clone() },
        ResourceProvenance::Replica { origin_root, last_synced_at } => {
            ResourceRecordProvenance::Replica { origin_root: origin_root.clone(), last_synced_at: last_synced_at.to_rfc3339() }
        }
    }
}

fn observed_freshness(observed_at: Option<DateTime<Utc>>, now: DateTime<Utc>, ttl: Duration) -> EvidenceFreshness {
    match observed_at.and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok()) {
        Some(age) if age < ttl => EvidenceFreshness::Fresh,
        Some(_) => EvidenceFreshness::Stale,
        None => EvidenceFreshness::Missing,
    }
}

fn explain_condition(condition: &IntegrationCondition, now: DateTime<Utc>, ttl: Duration) -> ExplainedCondition {
    let observed_at = condition.observed_at.as_deref().and_then(|value| DateTime::parse_from_rfc3339(value).ok()).map(|at| at.to_utc());
    ExplainedCondition {
        value: match condition.value {
            ConditionValue::True => "true",
            ConditionValue::False => "false",
            ConditionValue::Unknown => "unknown",
        }
        .to_string(),
        observed_at: condition.observed_at.clone(),
        freshness: observed_freshness(observed_at, now, ttl),
        details: condition.details.clone(),
    }
}

fn explain_unmet_expectation(expectation: UnmetSettlementExpectation) -> ExplainedUnmetExpectation {
    match expectation {
        UnmetSettlementExpectation::InvalidExpectedCheckouts { message } => {
            ExplainedUnmetExpectation { reason: "invalid_expected_checkouts".to_string(), subject: "convoy".to_string(), detail: message }
        }
        UnmetSettlementExpectation::ExitEntryAwaitingBinding { disposition, subject } => ExplainedUnmetExpectation {
            reason: "missing_binding".to_string(),
            subject: format!("exit/{disposition}"),
            detail: format!("entry awaits a bound change request for {subject}"),
        },
        UnmetSettlementExpectation::MissingCheckout { checkout } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "expected checkout has no federated record".to_string(),
        },
        UnmetSettlementExpectation::MissingCheckoutStatus { checkout } => ExplainedUnmetExpectation {
            reason: "missing_status".to_string(),
            subject: format!("checkout/{checkout}"),
            detail: "observed checkout has no status".to_string(),
        },
        UnmetSettlementExpectation::CheckoutConditionFalse { checkout, condition } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is false"),
        },
        UnmetSettlementExpectation::CheckoutConditionUnknown { checkout, condition } => ExplainedUnmetExpectation {
            reason: "unknown_condition".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: format!("{condition} is unknown"),
        },
        UnmetSettlementExpectation::StaleCheckoutEvidence { checkout, condition, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("checkout/{checkout}.{condition}"),
            detail: observed_at.map_or_else(|| "evidence has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::MissingChangeRequest { record } => ExplainedUnmetExpectation {
            reason: "missing_record".to_string(),
            subject: format!("change_request/{record}"),
            detail: "expected change request has no federated observation".to_string(),
        },
        UnmetSettlementExpectation::StaleChangeRequest { record, observed_at } => ExplainedUnmetExpectation {
            reason: "stale_evidence".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: observed_at.map_or_else(|| "state has no observation time".to_string(), |at| format!("observed at {at}")),
        },
        UnmetSettlementExpectation::ChangeRequestConditionFalse { record, value } => ExplainedUnmetExpectation {
            reason: "false_condition".to_string(),
            subject: format!("change_request/{record}.state"),
            detail: value.map_or_else(|| "state is unknown".to_string(), |value| format!("state is {value}")),
        },
        UnmetSettlementExpectation::InvalidCondition { subject, message } => {
            ExplainedUnmetExpectation { reason: "invalid_condition".to_string(), subject, detail: message }
        }
    }
}

fn resource_watch_record(event: serde_json::Value, local_node_id: &NodeId) -> Result<Option<ResourceReadRecord>, String> {
    let Some(event_type) = event.get("type").and_then(|value| value.as_str()) else {
        return Err("resource watch event is missing type".to_string());
    };
    if event_type == "BOOKMARK" {
        return Ok(None);
    }
    let record_type = match event_type {
        "ADDED" => ResourceRecordType::Added,
        "MODIFIED" => ResourceRecordType::Modified,
        "DELETED" => ResourceRecordType::Deleted,
        other => return Err(format!("unknown resource watch event type '{other}'")),
    };
    let object = event.get("object").cloned().ok_or_else(|| "resource watch event is missing object".to_string())?;
    Ok(Some(resource_record(record_type, object, local_node_id)))
}

fn resource_record_matches_name(record: &ResourceReadRecord, name: Option<&str>) -> bool {
    name.is_none_or(|name| {
        record
            .object
            .as_ref()
            .and_then(|object| object.get("metadata"))
            .and_then(|metadata| metadata.get("name"))
            .and_then(|value| value.as_str())
            == Some(name)
    })
}

#[derive(Default)]
struct StaticEnvVars {
    vars: HashMap<String, String>,
}

impl StaticEnvVars {
    fn from_bag(bag: &EnvironmentBag) -> Self {
        let mut vars = HashMap::new();
        for assertion in bag.assertions() {
            if let crate::providers::discovery::EnvironmentAssertion::EnvVarSet { key, value } = assertion {
                vars.insert(key.clone(), value.clone());
            }
        }
        Self { vars }
    }
}

impl crate::providers::discovery::EnvVars for StaticEnvVars {
    fn get(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
}

async fn load_env_vars(runner: &dyn CommandRunner, cwd: &Path) -> HashMap<String, String> {
    let Ok(output) = runner.run("env", &[], cwd, &ChannelLabel::Default).await else {
        return HashMap::new();
    };

    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

const STATIC_SSH_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

async fn register_static_ssh_direct_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config_key: &str,
    environment: &StaticEnvironmentConfig,
) -> Result<(), String> {
    let fallback_env_id = static_ssh_environment_id(config_key);
    let runner = Arc::new(SshCommandRunner::new(environment.hostname.clone(), true, Arc::clone(&discovery.runner)));
    tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, runner.run("true", &[], Path::new("/"), &ChannelLabel::Default))
        .await
        .map_err(|_| format!("ssh preflight timed out for {}", environment.hostname))?
        .map_err(|err| format!("ssh preflight failed for {}: {err}", environment.hostname))?;
    let remote_env_vars =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, load_env_vars(&*runner, Path::new("/"))).await.unwrap_or_default();
    let remote_env = StaticEnvVars { vars: remote_env_vars };
    let env_id = resolve_or_create_remote_environment_id(&*runner, &remote_env, fallback_env_id).await?;
    let host_id = resolve_or_create_remote_host_id(&*runner, &remote_env).await?;
    let mut env_bag =
        tokio::time::timeout(STATIC_SSH_REGISTRATION_TIMEOUT, run_host_detectors(&discovery.host_detectors, &*runner, &remote_env))
            .await
            .map_err(|_| format!("host detector execution timed out for {}", environment.hostname))?;
    if let Some(display_name) = environment.display_name.as_ref() {
        env_bag = env_bag.with(EnvironmentAssertion::env_var("DISPLAY_NAME", display_name));
    }
    environment_manager.register_direct_environment(env_id, runner, env_bag, host_id)
}

async fn register_static_ssh_direct_environments(
    config: &ConfigStore,
    discovery: &DiscoveryRuntime,
    environment_manager: &EnvironmentManager,
) {
    let daemon_config = match config.load_daemon_config() {
        Ok(config) => config,
        Err(err) => {
            warn!(%err, "failed to load daemon config for static SSH environments; continuing with local startup only");
            return;
        }
    };

    for (config_key, environment) in &daemon_config.environments {
        if let Err(err) = register_static_ssh_direct_environment(environment_manager, discovery, config_key, environment).await {
            warn!(
                environment = %config_key,
                hostname = %environment.hostname,
                %err,
                "failed to register static SSH direct environment; continuing startup"
            );
        }
    }
}

fn fallback_repo_identity(path: &Path) -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: "local".into(), path: path.to_string_lossy().into_owned() }
}

fn empty_repo_identity() -> flotilla_protocol::RepoIdentity {
    flotilla_protocol::RepoIdentity { authority: String::new(), path: String::new() }
}

/// An attach resolution: the plan the CLI should execute, plus the
/// structured binding it stamps onto its enclosing PM pane (#708).
#[derive(Debug, Clone)]
pub struct ResolvedAttach {
    pub plan: ResolvedAttachPlan,
    pub binding: Option<AttachBinding>,
}

fn attach_reference_keys(session_name: &str, labels: &BTreeMap<String, String>, convoy_address: Option<&str>) -> Vec<String> {
    let mut refs = vec![session_name.to_string()];

    let convoy = labels.get(CONVOY_LABEL);
    let task = labels.get(VESSEL_LABEL);
    let role = labels.get(ROLE_LABEL);
    let vessel = labels.get(VESSEL_REF_LABEL);

    if let Some(convoy) = convoy {
        refs.push(convoy.clone());
    }
    if let Some(address) = convoy_address {
        refs.push(address.to_string());
        if let Some(task) = task {
            refs.push(format!("{address}/{task}"));
        }
        if let (Some(task), Some(role)) = (task, role) {
            refs.push(format!("{address}/{task}/{role}"));
        }
    }
    if let Some(vessel) = vessel {
        refs.push(vessel.clone());
    }
    if let (Some(convoy), Some(task)) = (convoy, task) {
        refs.push(format!("{convoy}/{task}"));
    }
    if let (Some(convoy), Some(task), Some(role)) = (convoy, task, role) {
        refs.push(format!("{convoy}/{task}/{role}"));
    }
    if let (Some(vessel), Some(role)) = (vessel, role) {
        refs.push(format!("{vessel}/{role}"));
    }
    if let Some(role) = role {
        refs.push(role.clone());
    }

    refs.sort();
    refs.dedup();
    refs
}

fn attach_reference_label(session_name: &str, labels: &BTreeMap<String, String>, convoy_address: Option<&str>) -> String {
    if let Some(address) = convoy_address {
        return format!("{} ({session_name})", address.replace('@', " @ "));
    }
    match (labels.get(CONVOY_LABEL), labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) {
        (Some(convoy), Some(task), Some(role)) => format!("{convoy}/{task}/{role} ({session_name})"),
        (Some(convoy), Some(task), None) => format!("{convoy}/{task} ({session_name})"),
        (Some(convoy), None, Some(role)) => format!("{convoy}/{role} ({session_name})"),
        (Some(convoy), None, None) => format!("{convoy} ({session_name})"),
        _ => session_name.to_string(),
    }
}

fn fleet_row_attach_reference_keys(row: &FleetListRow) -> Vec<String> {
    let address = row.convoy.replace(" @ ", "@");
    let mut refs = vec![address.clone(), row.vessel.clone(), row.crew.clone()];
    if let Some(convoy_ref) = &row.convoy_ref {
        refs.push(convoy_ref.clone());
    }
    if let Some(session) = &row.session {
        refs.push(session.clone());
    }
    if row.crew != "-" {
        refs.push(format!("{address}/{}", row.crew));
        if let Some(convoy_ref) = &row.convoy_ref {
            refs.push(format!("{convoy_ref}/{}", row.crew));
        }
        if let Some((_task, role)) = row.crew.rsplit_once('/') {
            refs.push(role.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn fleet_row_attach_reference_label(row: &FleetListRow) -> String {
    if row.crew == "-" {
        format!("{} ({})", row.convoy, row.host)
    } else {
        format!("{}/{} ({})", row.convoy, row.crew, row.host)
    }
}

enum AttachTarget {
    Local(Box<flotilla_resources::ResourceObject<ResourceTerminalSession>>),
    Replica { row: Box<FleetListRow> },
    Checkout(Box<CheckoutRow>),
}

impl AttachTarget {
    async fn resolve(
        &self,
        daemon: &InProcessDaemon,
        reference: &str,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        match self {
            Self::Local(session) => {
                let (plan, host) = daemon.attach_plan_for_session(reference, session, seat).await?;
                let labels = &session.metadata.labels;
                let binding = AttachBinding::builder()
                    .host(host)
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .maybe_convoy(labels.get(CONVOY_LABEL).cloned())
                    .maybe_vessel(labels.get(VESSEL_LABEL).cloned())
                    .role(labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone()))
                    .build();
                Ok(ResolvedAttach { plan, binding: Some(binding) })
            }
            Self::Replica { row } => {
                let plan = daemon.recursive_attach_plan_for_remote(&row.host, reference, seat).await?;
                // Replica rows carry crew as "vessel/role" (or a bare role)
                // and the owning host's namespace + session name, so
                // cross-host panes stamp the full join key.
                let (vessel, role) = match row.crew.split_once('/') {
                    Some((vessel, role)) => (Some(vessel.to_owned()), Some(role.to_owned())),
                    None => (None, Some(row.crew.clone()).filter(|role| !role.is_empty() && role != "-")),
                };
                let binding = AttachBinding::builder()
                    .host(row.host.clone())
                    .namespace(row.namespace.clone())
                    .maybe_session(row.session.clone())
                    .maybe_convoy(row.convoy_ref.clone().or_else(|| Some(row.convoy.clone()).filter(|convoy| convoy != "-")))
                    .maybe_vessel(vessel)
                    .maybe_role(role)
                    .build();
                Ok(ResolvedAttach { plan, binding: Some(binding) })
            }
            Self::Checkout(row) => {
                if !transient {
                    return Err(format!("checkout '{}' is only available as a transient attach target", row.path));
                }
                let plan = if row.host == daemon.host_name {
                    daemon.local_checkout_terminal_plan(row, seat).await?
                } else {
                    daemon.recursive_attach_plan_for_remote(&row.host, reference, seat).await?
                };
                Ok(ResolvedAttach { plan, binding: None })
            }
        }
    }
}

struct AttachCandidate {
    label: String,
    references: Vec<String>,
    host: HostName,
    target: AttachTarget,
}

struct AttachCandidateIndex {
    candidates: Vec<AttachCandidate>,
    exact: HashMap<String, Vec<usize>>,
}

impl AttachCandidateIndex {
    fn new(candidates: Vec<AttachCandidate>) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            for reference in &candidate.references {
                exact.entry(reference.clone()).or_default().push(index);
            }
        }
        Self { candidates, exact }
    }

    async fn resolve(
        &self,
        daemon: &InProcessDaemon,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }

        let mut matches = self.exact.get(reference).cloned().unwrap_or_else(|| {
            self.candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.references.iter().any(|candidate_reference| candidate_reference.starts_with(reference)))
                .map(|(index, _)| index)
                .collect()
        });
        if let Some(host) = host {
            matches.retain(|index| &self.candidates[*index].host == host);
        }
        match matches.as_slice() {
            [] => match host {
                Some(host) => Err(format!("no attach target matching '{reference}' on host '{host}'")),
                None => Err(format!("no attach target matching '{reference}'")),
            },
            [index] => self.candidates[*index].target.resolve(daemon, reference, transient, seat).await,
            _ => {
                let mut labels: Vec<_> = matches.iter().map(|index| self.candidates[*index].label.clone()).collect();
                labels.sort();
                labels.dedup();
                Err(format!("attach reference '{reference}' is ambiguous: {}", labels.join(", ")))
            }
        }
    }
}

fn session_status_label(phase: Option<ResourceTerminalSessionPhase>) -> String {
    match phase {
        Some(ResourceTerminalSessionPhase::Starting) | None => "starting".to_string(),
        Some(ResourceTerminalSessionPhase::Running) => "running".to_string(),
        Some(ResourceTerminalSessionPhase::Stopped) => "stopped".to_string(),
        Some(ResourceTerminalSessionPhase::Failed) => "failed".to_string(),
    }
}

fn crew_attention(status: Option<&TerminalSessionStatus>, work_unsettled: bool, now: DateTime<Utc>) -> Option<CrewAttention> {
    let status = status.filter(|status| status.phase == ResourceTerminalSessionPhase::Running)?;
    if status.degraded.as_ref().is_some_and(|condition| condition.reason == "DeliveryUnconfirmed") {
        return Some(CrewAttention::DeliveryUnconfirmed);
    }
    let attention = status.attention.as_ref()?;
    if attention.is_stale_at(now) {
        return Some(CrewAttention::Unobservable);
    }
    Some(match attention.state {
        TerminalAttentionState::Working => CrewAttention::Working,
        TerminalAttentionState::NeedsInput => CrewAttention::NeedsInput,
        TerminalAttentionState::Idle if work_unsettled => CrewAttention::Stalled,
        TerminalAttentionState::Idle => CrewAttention::Idle,
        TerminalAttentionState::Unobservable => CrewAttention::Unobservable,
    })
}

fn crew_work_unsettled(phase: CrewWorkPhase) -> bool {
    !matches!(phase, CrewWorkPhase::Done | CrewWorkPhase::HandedBack | CrewWorkPhase::Failed)
}

fn convoy_state_label(row: &ConvoyRow) -> String {
    match row.message.as_deref().filter(|message| !message.trim().is_empty()) {
        Some(message) => format!("{}: {message}", row.phase),
        None => row.phase.to_string(),
    }
}

fn append_crewless_convoy_rows(
    rows: &mut Vec<FleetListRow>,
    target_namespace: &str,
    result_sets: &[ResultSet],
    host: &HostName,
    staleness: FleetStaleness,
) {
    let mut convoys_with_crew: HashSet<String> = rows.iter().filter_map(|row| row.convoy_ref.clone()).collect();
    for result_set in result_sets {
        let Rows::Convoys { rows: convoys, .. } = &result_set.rows else { continue };
        for row in convoys {
            if row.resource.namespace != target_namespace {
                continue;
            }
            if !convoys_with_crew.insert(row.resource.name.clone()) {
                continue;
            }
            let display = row.project_ref.as_ref().map_or_else(|| row.name.clone(), |project| format!("{} @ {project}", row.name));
            rows.push(
                FleetListRow::builder()
                    .convoy(display)
                    .convoy_ref(row.resource.name.clone())
                    .vessel("-")
                    .crew("-")
                    .crew_state(convoy_state_label(row))
                    .host(host.clone())
                    .maybe_placement_decision(row.placement_decision.clone())
                    .namespace(target_namespace)
                    .staleness(staleness.clone())
                    .build(),
            );
        }
    }
}

fn resource_environment_host_ref(environment: &flotilla_resources::ResourceObject<ResourceEnvironment>) -> Option<&str> {
    environment
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
}

fn ssh_destination(remote: &RemoteHostConfig) -> String {
    match remote.user.as_deref() {
        Some(user) if !user.is_empty() => format!("{user}@{}", remote.hostname),
        _ => remote.hostname.clone(),
    }
}

fn fleet_replica_ssh_args(remote: &RemoteHostConfig, multiplex: bool) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()),
        "-o".to_string(),
        "ConnectionAttempts=1".to_string(),
    ];
    if multiplex {
        args.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPath=/tmp/flotilla-ssh-%C".to_string(),
            "-o".to_string(),
            "ControlPersist=60".to_string(),
        ]);
    }
    args.push(ssh_destination(remote));
    let snapshot_command = vec![
        Arg::Literal("cd".to_string()),
        Arg::Quoted("/".to_string()),
        Arg::Literal("&&".to_string()),
        Arg::Literal("exec".to_string()),
        Arg::Literal("flotilla".to_string()),
        Arg::Literal("--json".to_string()),
        Arg::Quoted("replica-snapshot".to_string()),
    ];
    let login_wrapper = vec![
        Arg::Literal("${SHELL:-/bin/sh}".to_string()),
        Arg::Literal("-l".to_string()),
        Arg::Literal("-c".to_string()),
        Arg::NestedCommand(snapshot_command),
    ];
    args.push(flatten(&login_wrapper, 0));
    args
}

fn replica_staleness(entry: &FleetReplicaCacheEntry, now: DateTime<Utc>) -> FleetStaleness {
    if let Some(message) = &entry.last_error {
        return FleetStaleness::Unreachable { last_sync: entry.last_sync, message: message.clone() };
    }
    let Some(last_sync) = entry.last_sync else {
        return FleetStaleness::Unreachable { last_sync: None, message: "replica has never synced".to_string() };
    };
    if now.signed_duration_since(last_sync).num_seconds() > FLEET_REPLICA_FRESH_SECS {
        FleetStaleness::Stale { last_sync }
    } else {
        FleetStaleness::Fresh { last_sync }
    }
}

fn accumulate_fleet_health_counts(counts: &mut HashMap<HostName, (usize, HashSet<String>)>, rows: &[FleetListRow]) {
    for row in rows {
        let (crew_count, convoys) = counts.entry(row.host.clone()).or_default();
        if row.crew != "-" {
            *crew_count += 1;
        }
        if row.convoy != "-" {
            convoys.insert(row.convoy.clone());
        }
    }
}

/// One attention entry per expired or near-expiry credential scope on a host,
/// derived from the `credential_expiry` capability its heartbeat publishes.
fn host_credential_attention(
    status: &ResourceHostStatus,
    now: DateTime<Utc>,
    warning_window: chrono::Duration,
) -> Vec<CredentialAttention> {
    let Ok(expiry) = status.credential_expiry() else {
        return vec![CredentialAttention {
            severity: CredentialAttentionSeverity::Unreadable,
            message: "credential expiry capability is unreadable".to_string(),
        }];
    };
    let mut attention = Vec::new();
    for (scope, entry) in expiry {
        let label = if scope == flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE {
            "ambient claude login".to_string()
        } else {
            format!("credential `{scope}`")
        };
        if let Some(expired_at) = entry.expired_at(now) {
            attention.push(CredentialAttention {
                severity: CredentialAttentionSeverity::Expired,
                message: format!("{label} expired on {}", expired_at.format("%Y-%m-%d")),
            });
        } else if let Some(expires_at) = entry.expires_within(now, warning_window) {
            attention.push(CredentialAttention {
                severity: CredentialAttentionSeverity::Expiring,
                message: format!("{label} expires on {}", expires_at.format("%Y-%m-%d")),
            });
        }
    }
    attention
}

fn fleet_observation_agreement(
    link: &PeerConnectionState,
    heartbeat_at: Option<DateTime<Utc>>,
    heartbeat_fresh: bool,
    daemon_generation: Option<&str>,
    replica_generation: Option<&str>,
    is_local: bool,
) -> FleetObservationAgreement {
    if is_local {
        return FleetObservationAgreement::Agree;
    }
    let generation_disagrees = matches!((daemon_generation, replica_generation), (Some(daemon), Some(replica)) if daemon != replica);
    let link_disagrees = match link {
        PeerConnectionState::Connected => !heartbeat_fresh,
        PeerConnectionState::Disconnected | PeerConnectionState::Rejected { .. } => heartbeat_fresh,
        PeerConnectionState::Connecting | PeerConnectionState::Reconnecting => false,
    };
    if generation_disagrees || link_disagrees {
        FleetObservationAgreement::Disagree
    } else if heartbeat_at.is_none()
        || matches!(link, PeerConnectionState::Connecting | PeerConnectionState::Reconnecting)
        || daemon_generation.is_none()
        || replica_generation.is_none()
    {
        FleetObservationAgreement::Unknown
    } else {
        FleetObservationAgreement::Agree
    }
}

fn format_resource_replication_failures(failures: &[ResourceReplicationFailure]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    Some(format!(
        "resource replication failed: {}",
        failures.iter().map(|failure| format!("{}: {}", failure.kind, failure.message)).collect::<Vec<_>>().join("; ")
    ))
}

fn join_replica_errors(first: Option<&str>, second: Option<&str>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(message), None) | (None, Some(message)) => Some(message.to_string()),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
struct ReplicaParseDiagnostics {
    skipped_records: usize,
    first_error: Option<String>,
}

impl ReplicaParseDiagnostics {
    fn record_skip(&mut self, path: impl std::fmt::Display, error: impl std::fmt::Display) {
        self.record_skips(1, path, error);
    }

    fn record_skips(&mut self, count: usize, path: impl std::fmt::Display, error: impl std::fmt::Display) {
        self.skipped_records += count;
        self.first_error.get_or_insert_with(|| format!("{path}: {error}"));
    }
}

#[derive(Debug)]
struct ParsedFleetReplicaSnapshot {
    snapshot: FleetReplicaSnapshot,
    diagnostics: ReplicaParseDiagnostics,
}

fn result_set_records_mut(result_set: &mut serde_json::Value) -> Option<&mut Vec<serde_json::Value>> {
    result_set.get_mut("rows")?.get_mut("rows")?.get_mut("rows")?.as_array_mut()
}

fn retain_parseable_result_set_records(
    result_set: &mut serde_json::Value,
    result_set_index: usize,
    diagnostics: &mut ReplicaParseDiagnostics,
) -> bool {
    let Some(records) = result_set_records_mut(result_set) else {
        return match serde_json::from_value::<ResultSet>(result_set.clone()) {
            Ok(_) => true,
            Err(error) => {
                diagnostics.record_skip(format_args!("result_sets[{result_set_index}]"), error);
                false
            }
        };
    };
    let records = std::mem::take(records);

    let envelope = result_set.clone();
    if let Err(error) = serde_json::from_value::<ResultSet>(envelope.clone()) {
        diagnostics.record_skips(records.len().max(1), format_args!("result_sets[{result_set_index}]"), error);
        return false;
    }

    let mut retained = Vec::with_capacity(records.len());
    for (record_index, record) in records.into_iter().enumerate() {
        let mut candidate = envelope.clone();
        result_set_records_mut(&mut candidate).expect("validated result set has a row array").push(record.clone());
        match serde_json::from_value::<ResultSet>(candidate) {
            Ok(_) => retained.push(record),
            Err(error) => {
                diagnostics.record_skip(format_args!("result_sets[{result_set_index}].rows[{record_index}]"), error);
            }
        }
    }
    *result_set_records_mut(result_set).expect("validated result set has a row array") = retained;
    true
}

fn parse_fleet_replica_snapshot(input: &str) -> Result<ParsedFleetReplicaSnapshot, String> {
    let mut value: serde_json::Value = serde_json::from_str(input).map_err(|error| format!("replica snapshot parse failed: {error}"))?;
    let mut diagnostics = ReplicaParseDiagnostics::default();

    if let Some(rows) = value.get_mut("rows").and_then(serde_json::Value::as_array_mut) {
        let records = std::mem::take(rows);
        for (index, record) in records.into_iter().enumerate() {
            match serde_json::from_value::<FleetListRow>(record.clone()) {
                Ok(_) => rows.push(record),
                Err(error) => diagnostics.record_skip(format_args!("rows[{index}]"), error),
            }
        }
    }

    if let Some(result_sets) = value.get_mut("result_sets").and_then(serde_json::Value::as_array_mut) {
        let records = std::mem::take(result_sets);
        for (index, mut record) in records.into_iter().enumerate() {
            if retain_parseable_result_set_records(&mut record, index, &mut diagnostics) {
                result_sets.push(record);
            }
        }
    }

    let snapshot = serde_json::from_value(value).map_err(|error| format!("replica snapshot parse failed outside a record: {error}"))?;
    Ok(ParsedFleetReplicaSnapshot { snapshot, diagnostics })
}

fn parse_and_validate_workflow_template_yaml(yaml: &str) -> Result<WorkflowTemplateSpec, String> {
    let spec: WorkflowTemplateSpec = serde_yml::from_str(yaml).map_err(|err| format!("invalid workflow template YAML: {err}"))?;
    flotilla_resources::validate(&spec).map_err(|errors| {
        let joined = errors.iter().map(|e| format!("{e}")).collect::<Vec<_>>().join("; ");
        format!("workflow template validation failed: {joined}")
    })?;
    Ok(spec)
}

fn parse_project_yaml(yaml: &str) -> Result<ProjectSpec, String> {
    serde_yml::from_str(yaml).map_err(|err| format!("invalid project YAML: {err}"))
}

fn adopted_checkout_name(convoy_name: &str) -> String {
    format!("adopted-checkout-{convoy_name}")
}

#[derive(bon::Builder)]
struct AdoptedCheckoutRequest<'a> {
    namespace: &'a str,
    convoy_name: &'a str,
    checkout_path: &'a Path,
    repository_spec: &'a RepositorySpec,
    repository_url: &'a str,
    git_ref: &'a str,
    host_ref: &'a str,
}

async fn create_adopted_checkout_resource(
    durable_backend: &ResourceBackend,
    observed_backend: &ResourceBackend,
    request: AdoptedCheckoutRequest<'_>,
) -> Result<(String, String, String), String> {
    let AdoptedCheckoutRequest { namespace, convoy_name, checkout_path, repository_spec, repository_url, git_ref, host_ref } = request;
    let path = std::fs::canonicalize(checkout_path)
        .map_err(|err| format!("adopted checkout path {} cannot be resolved: {err}", checkout_path.display()))?;
    let path_str = path.to_string_lossy().to_string();
    let checkout_ref = adopted_checkout_name(convoy_name);
    let repository_key = repository_spec.key();
    flotilla_resources::ensure_repository(&durable_backend.clone().using::<Repository>(namespace), &repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    let meta = InputMeta::builder()
        .name(checkout_ref.clone())
        .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), convoy_name.to_string())]))
        .build()
        .with_lifecycle_authority(LifecycleAuthority::Adopted);
    let spec = ResourceCheckoutSpec::Observed(
        ResourceObservedCheckoutSpec::builder()
            .r#ref(git_ref.to_string())
            .path(path_str.clone())
            .repo_ref(repository_key)
            .host_ref(host_ref.to_string())
            .is_main(matches!(git_ref, "main" | "master" | "trunk"))
            .build(),
    );
    let status = ResourceCheckoutStatus::builder().phase(ResourceCheckoutPhase::Ready).path(path_str).build();

    let durable = persist_adopted_checkout(durable_backend, namespace, &checkout_ref, &meta, &spec, &status).await?;
    match crate::observed_resources::project_adopted_checkout(observed_backend, namespace, &durable).await {
        Ok(()) => {}
        Err(ResourceError::Invalid { message }) => return Err(message),
        Err(error) => {
            warn!(checkout = %checkout_ref, %error, "adopted checkout committed durably but observed publication failed; reconciliation will retry");
        }
    }

    Ok((checkout_ref, repository_url.to_string(), git_ref.to_string()))
}

async fn persist_adopted_checkout(
    backend: &ResourceBackend,
    namespace: &str,
    checkout_ref: &str,
    meta: &InputMeta,
    spec: &ResourceCheckoutSpec,
    status: &ResourceCheckoutStatus,
) -> Result<ResourceObject<ResourceCheckout>, String> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let checkout = match checkouts.create(meta, spec).await {
        Ok(checkout) => checkout,
        Err(ResourceError::Conflict { .. }) => {
            let existing = checkouts.get(checkout_ref).await.map_err(|err| err.to_string())?;
            if existing.metadata.lifecycle_authority().map_err(|err| err.to_string())? != Some(LifecycleAuthority::Adopted) {
                return Err(format!("checkout {checkout_ref} already exists but is not adopted"));
            }
            if &existing.spec != spec {
                return Err(format!("checkout {checkout_ref} already exists with different adopted checkout details"));
            }
            existing
        }
        Err(err) => return Err(err.to_string()),
    };
    if checkout.status.is_some() {
        Ok(checkout)
    } else {
        checkouts.update_status(checkout_ref, &checkout.metadata.resource_version, status).await.map_err(|err| err.to_string())
    }
}

#[derive(Debug)]
struct PlacementResolution {
    selected: Option<ResourceObject<PlacementPolicy>>,
    refused_candidates: Vec<PlacementRefusal>,
    viable_not_selected: Vec<PlacementViableCandidate>,
}

fn home_copy_wins_by_name<T: Resource>(sources: impl IntoIterator<Item = ReadResourceObject<T>>) -> Vec<ResourceObject<T>> {
    let mut resolved = BTreeMap::<String, ReadResourceObject<T>>::new();
    for source in sources {
        let name = source.object.metadata.name.clone();
        match resolved.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(mut entry)
                if matches!(source.provenance, ResourceProvenance::Local)
                    && matches!(entry.get().provenance, ResourceProvenance::Replica { .. }) =>
            {
                entry.insert(source);
            }
            std::collections::btree_map::Entry::Occupied(_) => {}
        }
    }
    resolved.into_values().map(|source| source.object).collect()
}

fn placement_host_ref(policy: &ResourceObject<PlacementPolicy>) -> Option<&str> {
    policy
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| policy.spec.docker_per_vessel.as_ref().map(|spec| spec.host_ref.as_str()))
}

async fn placement_target_host(
    backend: &ResourceBackend,
    namespace: &str,
    policy: &ResourceObject<PlacementPolicy>,
) -> Result<PlacementTargetHost, String> {
    let host_ref = placement_host_ref(policy).ok_or_else(|| format!("placement `{}` has no target host", policy.metadata.name))?;
    canonical_placement_host_ref(backend, namespace, host_ref)
        .await
        .and_then(|target| target.ok_or_else(|| format!("references unknown host `{host_ref}`")))
        .map_err(|error| format!("placement `{}` {error}", policy.metadata.name))
}

async fn canonical_placement_host_ref(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let hosts = backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
    canonical_placement_host_ref_from_sources(&hosts.items, host_ref)
}

fn canonical_placement_host_ref_from_sources(
    hosts: &[ReadResourceObject<ResourceHost>],
    host_ref: &str,
) -> Result<Option<PlacementTargetHost>, String> {
    let canonical = flotilla_resources::canonical_host_id(hosts.iter().map(|host| &host.object), host_ref)?;
    let Some(canonical) = canonical else {
        return Ok(None);
    };
    let resolved = hosts
        .iter()
        .find(|host| host.object.metadata.name == canonical.as_str())
        .expect("canonical host resolver selected an existing host");
    let display_name = if resolved.object.spec.display_name.is_empty() {
        resolved.object.metadata.name.clone()
    } else {
        resolved.object.spec.display_name.clone()
    };
    Ok(Some(PlacementTargetHost { reference: canonical, display_name }))
}

async fn authoritative_placement_host(
    backend: &ResourceBackend,
    namespace: &str,
    target_host: &PlacementTargetHost,
    placement_name: &str,
) -> Result<ResourceObject<ResourceHost>, String> {
    backend
        .including_replicas::<ResourceHost>(namespace)
        .get(target_host.reference.as_str())
        .await
        .map(|source| source.object)
        .map_err(|error| format!("placement `{placement_name}` target host is not ready: {error}"))
}

fn host_generation(status: Option<&ResourceHostStatus>) -> &str {
    status.and_then(|status| status.daemon_generation.as_deref()).unwrap_or("unknown")
}

fn placement_host_not_ready_reason(placement_name: &str, host_label: &str, generation: &str, status: &ResourceHostStatus) -> String {
    let mut failing_conditions = status
        .conditions
        .iter()
        .filter(|condition| condition.blocks_readiness())
        .map(|condition| format!("{}: {}", condition.reason, condition.message))
        .collect::<Vec<_>>();
    failing_conditions.sort();
    let detail = if failing_conditions.is_empty() { String::new() } else { format!(": {}", failing_conditions.join("; ")) };
    format!("placement `{placement_name}` host `{host_label}` generation `{generation}` is not ready{detail}")
}

fn check_placement_capacity(target_host: &PlacementTargetHost, capacity: Option<(u64, Option<u64>)>) -> Result<(), String> {
    let Some((floor_bytes, free_bytes)) = capacity else {
        return Err(format!("placement refused on host `{}`: admission free-space floor is unavailable", target_host.display_name));
    };
    if floor_bytes == 0 {
        return Ok(());
    }
    let free_bytes =
        free_bytes.ok_or_else(|| format!("placement refused on host `{}`: free space is unavailable", target_host.display_name))?;
    crate::admission::check_measured_free_space(&target_host.display_name, free_bytes, floor_bytes)
}

async fn default_convoy_placement_policy(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    local_host_ref: Option<&CanonicalHostId>,
) -> Result<PlacementResolution, String> {
    let contained = workflow.vessels.iter().any(|vessel| vessel.stance == flotilla_resources::Stance::Contained);
    let mut policies = match backend.including_replicas::<PlacementPolicy>(namespace).list().await {
        Ok(list) => home_copy_wins_by_name(list.items),
        Err(err) => {
            warn!(%namespace, error = %err, "failed to list placement policies; convoy will remain Pending until one is registered");
            return Ok(PlacementResolution { selected: None, refused_candidates: Vec::new(), viable_not_selected: Vec::new() });
        }
    };
    policies.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
    let candidate_names = policies.iter().map(|policy| policy.metadata.name.clone()).collect::<Vec<_>>();
    let mut viable = Vec::new();
    let mut refused_candidates = Vec::new();
    for policy in policies {
        let refusal = if contained && policy.spec.docker_per_vessel.is_none() {
            Some(format!("contained workflow requires a docker placement policy, but {} is not contained", policy.metadata.name))
        } else if let Err(reason) = validate_workflow_agent_adapters(backend, namespace, workflow, Some(&policy)).await {
            Some(reason)
        } else {
            validate_workflow_credentials(backend, namespace, workflow, Some(&policy)).await.err()
        };
        if let Some(reason) = refusal {
            let target_host = placement_target_host(backend, namespace, &policy).await.unwrap_or_else(|_| PlacementTargetHost {
                reference: CanonicalHostId::resolved(String::new()),
                display_name: "no target host".to_string(),
            });
            refused_candidates.push(PlacementRefusal { policy_name: policy.metadata.name.clone(), target_host, reason });
        } else {
            viable.push(policy);
        }
    }
    let mut viable_targets = HashMap::new();
    let mut resolved_viable = Vec::with_capacity(viable.len());
    for policy in viable {
        match placement_target_host(backend, namespace, &policy).await {
            Ok(target_host) => {
                viable_targets.insert(policy.metadata.name.clone(), target_host);
                resolved_viable.push(policy);
            }
            Err(reason) => refused_candidates.push(PlacementRefusal {
                policy_name: policy.metadata.name.clone(),
                target_host: PlacementTargetHost {
                    reference: CanonicalHostId::resolved(String::new()),
                    display_name: "no target host".to_string(),
                },
                reason,
            }),
        }
    }
    viable = resolved_viable;
    viable.sort_by_key(|policy| {
        let target_host = &viable_targets[&policy.metadata.name].reference;
        let is_local = local_host_ref.is_some_and(|local| target_host == local);
        let is_host_direct = policy.spec.host_direct.is_some();
        (Reverse(policy.spec.priority), !is_local, !is_host_direct, policy.metadata.name.clone())
    });
    if !viable.is_empty() {
        let selected = viable.remove(0);
        let selected_target = viable_targets.remove(&selected.metadata.name).expect("viable placement target was resolved");
        let mut viable_not_selected = Vec::with_capacity(viable.len());
        for policy in viable {
            let target_host = viable_targets.remove(&policy.metadata.name).expect("viable placement target was resolved");
            let reason = placement_ordering_reason(&selected, &selected_target, &policy, &target_host, local_host_ref);
            viable_not_selected.push(PlacementViableCandidate { policy_name: policy.metadata.name.clone(), target_host, reason });
        }
        return Ok(PlacementResolution { selected: Some(selected), refused_candidates, viable_not_selected });
    }

    let required_adapters = required_workflow_agent_adapters(workflow)?;
    if !required_adapters.is_empty() {
        let requirement = if required_adapters.len() == 1 {
            format!("adapter `{}`", required_adapters.first().expect("one required adapter"))
        } else {
            format!("adapters {}", required_adapters.iter().map(|adapter| format!("`{adapter}`")).collect::<Vec<_>>().join(", "))
        };
        if refused_candidates.is_empty() {
            return Err(format!("no placement policy satisfies {requirement}; candidates: (none)"));
        }
        refused_candidates.sort_by(|left, right| left.policy_name.cmp(&right.policy_name));
        let candidates = refused_candidates
            .iter()
            .map(|candidate| format!("- `{}`: {}", candidate.policy_name, candidate.reason))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("no placement policy satisfies {requirement}; candidates:\n{candidates}"));
    }

    if candidate_names.is_empty() {
        warn!(%namespace, "no placement policy found; convoy will remain Pending until one is registered");
    }
    Ok(PlacementResolution { selected: None, refused_candidates, viable_not_selected: Vec::new() })
}

fn placement_ordering_reason(
    selected: &ResourceObject<PlacementPolicy>,
    selected_target: &PlacementTargetHost,
    candidate: &ResourceObject<PlacementPolicy>,
    candidate_target: &PlacementTargetHost,
    local_host_ref: Option<&CanonicalHostId>,
) -> String {
    if candidate.spec.priority != selected.spec.priority {
        return format!(
            "priority {} is lower than selected policy `{}` priority {}",
            candidate.spec.priority, selected.metadata.name, selected.spec.priority
        );
    }

    let selected_is_local = local_host_ref.is_some_and(|local| &selected_target.reference == local);
    let candidate_is_local = local_host_ref.is_some_and(|local| &candidate_target.reference == local);
    if selected_is_local && !candidate_is_local {
        return format!("fallback ordering preferred local policy `{}`", selected.metadata.name);
    }
    if selected.spec.host_direct.is_some() && candidate.spec.host_direct.is_none() {
        return format!("fallback ordering preferred host-direct policy `{}`", selected.metadata.name);
    }
    format!("fallback ordering preferred policy `{}` by name", selected.metadata.name)
}

fn repo_identity_from_bag_or_path(path: &Path, bag: &EnvironmentBag) -> flotilla_protocol::RepoIdentity {
    bag.repo_identity().unwrap_or_else(|| fallback_repo_identity(path))
}

fn configured_repo_identity_or_bag_or_path(config: &ConfigStore, path: &Path, bag: &EnvironmentBag) -> flotilla_protocol::RepoIdentity {
    let repo_root = ExecutionEnvironmentPath::new(path);
    config
        .resolve_repo_issue_source(&repo_root)
        .map(|source| flotilla_protocol::RepoIdentity { authority: source.service, path: source.scope })
        .unwrap_or_else(|| repo_identity_from_bag_or_path(path, bag))
}

async fn discover_repo_for_environment(
    environment_manager: &EnvironmentManager,
    discovery: &DiscoveryRuntime,
    config: &ConfigStore,
    local_environment_id: &EnvironmentId,
    environment_id: &EnvironmentId,
    repo_path: &Path,
) -> Result<DiscoveryResult, String> {
    let host_bag = environment_manager.environment_bag(environment_id).ok_or_else(|| format!("environment not found: {environment_id}"))?;
    let runner =
        environment_manager.environment_runner(environment_id).ok_or_else(|| format!("environment runner not found: {environment_id}"))?;
    let ee_path = ExecutionEnvironmentPath::new(repo_path);
    let remote_env = StaticEnvVars::from_bag(&host_bag);
    let env: &dyn crate::providers::discovery::EnvVars = if environment_id == local_environment_id { &*discovery.env } else { &remote_env };

    let host_scoped = discovery
        .host_scoped_providers
        .discover_for_environment(environment_id, &host_bag, &discovery.factories, config, &ee_path, Arc::clone(&runner))
        .await;
    Ok(discover_providers_with_host_scoped(
        &host_bag,
        &ee_path,
        &discovery.repo_detectors,
        &discovery.factories,
        config,
        runner,
        env,
        &host_scoped,
    )
    .await)
}

#[derive(Debug, Clone)]
struct FleetReplicaCacheEntry {
    rows: Vec<FleetListRow>,
    result_sets: Vec<ResultSet>,
    last_sync: Option<DateTime<Utc>>,
    generation: Option<String>,
    skipped_records: usize,
    first_parse_error: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct ResourceReplicationFailure {
    kind: String,
    message: String,
}

const SUPERSEDED_BY_ANNOTATION: &str = "flotilla.work/superseded-by";

#[derive(bon::Builder)]
struct ResolvedCrewContext {
    namespace: String,
    convoy: String,
    vessel_ref: String,
    vessel: String,
    caller_role: String,
    caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
}

struct DaemonTurnDeliveryActuator {
    daemon: Weak<InProcessDaemon>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDeliverySessionPlan {
    QueueWarm,
    QueueFresh,
    RestartFresh,
}

fn turn_delivery_session_plan(
    phase: Option<ResourceTerminalSessionPhase>,
    vessel: &str,
    role: &str,
) -> Result<TurnDeliverySessionPlan, String> {
    match phase {
        Some(ResourceTerminalSessionPhase::Running) => Ok(TurnDeliverySessionPlan::QueueWarm),
        Some(ResourceTerminalSessionPhase::Starting) | None => Ok(TurnDeliverySessionPlan::QueueFresh),
        Some(ResourceTerminalSessionPhase::Stopped) => Ok(TurnDeliverySessionPlan::RestartFresh),
        Some(ResourceTerminalSessionPhase::Failed) => {
            Err(format!("turn-delivery target {vessel}/{role} failed provisioning and cannot be restarted"))
        }
    }
}

#[async_trait]
impl crate::leaf_engine::TurnDeliveryActuator for DaemonTurnDeliveryActuator {
    async fn deliver(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
        self.daemon.upgrade().ok_or_else(|| "daemon stopped before turn delivery".to_string())?.deliver_standing_turn(request).await
    }

    async fn hold(&self, request: &crate::leaf_engine::TurnDeliveryRequest, act: &HoldAct, reason: &str) -> Result<(), String> {
        self.daemon
            .upgrade()
            .ok_or_else(|| "daemon stopped before turn-delivery hold".to_string())?
            .execute_turn_delivery_hold(request, act, reason)
            .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewRoutingContext {
    pub command_context: CrewCommandContext,
    pub session_name: Option<String>,
    pub convoy: String,
}

fn input_meta_from_resource<T: Resource>(resource: &flotilla_resources::ResourceObject<T>) -> InputMeta {
    InputMeta::builder()
        .name(resource.metadata.name.clone())
        .labels(resource.metadata.labels.clone())
        .annotations(resource.metadata.annotations.clone())
        .owner_references(resource.metadata.owner_references.clone())
        .finalizers(resource.metadata.finalizers.clone())
        .maybe_deletion_timestamp(resource.metadata.deletion_timestamp)
        .build()
}

fn handoff_crew_brief(
    context: &ResolvedCrewContext,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    target: &str,
    prompt: Option<&str>,
    members: &[CrewListMember],
    repository_refs: &[RepositoryKey],
    render_options: &crate::agent_adapter::CrewBriefRenderOptions,
) -> Result<TerminalBrief, String> {
    let assignment = match prompt {
        Some(prompt) => crate::agent_adapter::CrewAssignment::Prompt(prompt),
        None if !convoy.spec.issues.is_empty() => crate::agent_adapter::CrewAssignment::CarriedIssue,
        None if convoy.spec.change_request.is_some() => crate::agent_adapter::CrewAssignment::CarriedChangeRequest,
        None => crate::agent_adapter::CrewAssignment::Unassigned,
    };
    let brief = crate::agent_adapter::build_crew_brief_with_options(
        &TerminalCrewContext {
            namespace: context.namespace.clone(),
            convoy: context.convoy.clone(),
            vessel_ref: context.vessel_ref.clone(),
        },
        &context.vessel,
        target,
        assignment,
        &members
            .iter()
            .map(|member| crate::agent_adapter::CrewBriefMember {
                role: member.role.clone(),
                state: if member.role == target { "active".to_string() } else { member.state.clone() },
                is_agent: member.kind == "agent",
            })
            .collect::<Vec<_>>(),
        render_options,
    );
    let mut brief = brief?;
    crate::agent_adapter::append_convoy_work_context(&mut brief.content, convoy, repository_refs);
    Ok(brief)
}

async fn crew_brief_repo_roots(
    backend: &ResourceBackend,
    namespace: &str,
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    repository_refs: &[RepositoryKey],
) -> Vec<PathBuf> {
    let checkouts = backend.clone().using::<ResourceCheckout>(namespace);
    let mut roots = Vec::new();
    for repository_ref in repository_refs {
        let Some(checkout_ref) = convoy.spec.adopted_checkout_refs.get(repository_ref) else {
            continue;
        };
        let Ok(checkout) = checkouts.get(checkout_ref).await else {
            continue;
        };
        let Some(path) =
            checkout.status.as_ref().and_then(|status| status.path.clone()).or_else(|| checkout.spec.target_path().map(str::to_string))
        else {
            continue;
        };
        roots.push(PathBuf::from(path));
    }
    roots
}

fn pending_crew_message(text: &str) -> TerminalCrewMessage {
    TerminalCrewMessage { id: uuid::Uuid::new_v4().to_string(), text: text.to_string() }
}

fn ensure_crew_work_is_defined(
    convoy: &flotilla_resources::ResourceObject<ResourceConvoy>,
    context: &ResolvedCrewContext,
) -> Result<(), String> {
    let known_agent = convoy
        .status
        .as_ref()
        .and_then(|status| status.crew_work.get(&context.vessel))
        .is_some_and(|crew| crew.contains_key(&context.caller_role));
    if known_agent {
        Ok(())
    } else {
        Err(format!("crew work for role `{}` is not defined on vessel `{}`", context.caller_role, context.vessel))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvoyResumeOutcome {
    Delivered { displaced: Option<String> },
    Queued { displaced: Option<String> },
}

type ConvoyMessageKey = (String, String);
type ConvoyMessageLock = Arc<Mutex<()>>;
type WeakConvoyMessageLock = Weak<Mutex<()>>;

fn crew_handoff_address_error(target: &str, vessel: &str) -> String {
    format!(
        "no such crew member in your vessel; crew messaging is intra-vessel and requires a different crew member (target `{target}`, vessel `{vessel}`)"
    )
}

fn crew_handoff_message(context: &ResolvedCrewContext, message: &str) -> String {
    format!("handoff from {}@{}\n\n{message}", context.caller_role, context.vessel)
}

fn terminal_meta_with_vessel_credentials(mut meta: InputMeta, requirement: &flotilla_resources::VesselRequirement) -> InputMeta {
    if !requirement.credential_refs.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_REFS_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_refs).expect("credential names serialize"),
        );
    }
    if !requirement.credential_scopes.is_empty() {
        meta.annotations.insert(
            CREDENTIAL_SCOPES_ANNOTATION.to_string(),
            serde_json::to_string(&requirement.credential_scopes).expect("credential scopes serialize"),
        );
    }
    meta
}

async fn queue_pending_crew_message(
    sessions: &flotilla_resources::TypedResolver<ResourceTerminalSession>,
    existing: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    message: &str,
) -> Result<(), String> {
    let mut spec = existing.spec.clone();
    let TerminalSessionSource::Agent { message: pending, .. } = &mut spec.source else {
        return Err(format!("crew target `{}` is not an agent session", existing.spec.role));
    };
    *pending = Some(pending_crew_message(message));
    sessions
        .update(&input_meta_from_resource(existing), &existing.metadata.resource_version, &spec)
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[derive(bon::Builder)]
struct ConvoyStartTask {
    command_id: u64,
    intent: flotilla_protocol::ConvoyStartIntent,
    key: ConvoyStartKey,
    dispatching_principal_ref: PrincipalRef,
}

#[derive(bon::Builder)]
struct ConvoyAdmission {
    name: String,
    spec: ConvoySpec,
    workflow: WorkflowTemplateSpec,
    placement_policy: Option<PlacementPolicySpec>,
    placement_decision: Option<PlacementDecision>,
}

fn convoy_record_name() -> String {
    format!("convoy-{}", uuid::Uuid::new_v4().simple())
}

fn convoy_ensure_name(project: &str, role: &str) -> String {
    let digest = Sha256::digest(format!("{project}\0{role}").as_bytes());
    format!("ensure-{digest:x}")
}

fn convoy_address(role: &str, project: Option<&str>) -> String {
    project.map_or_else(|| role.to_string(), |project| format!("{role}@{project}"))
}

fn convoy_disambiguation_address(role: &str, project: Option<&str>) -> String {
    format!("{role}@{}", project.unwrap_or_default())
}

/// The stable human-facing address of a convoy role, independent of any one
/// generation's resource record name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleAddress {
    pub project: String,
    pub role: String,
}

impl FromStr for RoleAddress {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((role, project)) = value.split_once('@') else {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        };
        if role.is_empty() || project.is_empty() || project.contains('@') {
            return Err(format!("invalid role address `{value}`: expected role@project"));
        }
        Ok(Self { project: project.to_string(), role: role.to_string() })
    }
}

impl fmt::Display for RoleAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.role, self.project)
    }
}

/// A resolved, currently-live convoy generation. Callers route by owner and
/// select sessions by `record_name`; neither operation accepts a raw role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConvoyRecord {
    pub address: RoleAddress,
    pub record_name: String,
    pub owner_host: HostName,
}

async fn allocate_convoy_generation(backend: &ResourceBackend, namespace: &str, project: Option<&str>, role: &str) -> Result<u64, String> {
    let generations = backend.clone().using::<ResourceConvoy>(namespace).list().await.map_err(|error| error.to_string())?;
    let mut maximum = 0;
    for convoy in generations.items.into_iter().filter(|convoy| convoy.spec.project_ref.as_deref() == project && convoy.spec.role == role) {
        let generation =
            convoy.metadata.labels.get(GENERATION_LABEL).and_then(|value| value.parse::<u64>().ok()).unwrap_or(convoy.spec.generation);
        maximum = maximum.max(generation);
        let live = convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal());
        if live {
            return Err(format!("live convoy {} generation {generation} already exists", convoy_address(role, project)));
        }
    }
    let generation =
        maximum.checked_add(1).ok_or_else(|| format!("convoy {} exhausted its generation counter", convoy_address(role, project)))?;
    Ok(generation)
}

fn parse_role_address(value: &str) -> Result<(&str, Option<&str>), String> {
    match value.split_once('@') {
        Some((role, project)) if !role.is_empty() && !project.contains('@') => Ok((role, Some(project))),
        Some(_) => Err(format!("invalid convoy address `{value}`: expected role@project")),
        None if value.is_empty() => Err("convoy role cannot be empty".to_string()),
        None => Ok((value, None)),
    }
}

struct ConvoyAddressIdentity<'a> {
    record_name: &'a str,
    role: Option<&'a str>,
    project: Option<&'a str>,
    terminal: bool,
}

fn resolve_convoy_candidate_indices(identities: &[ConvoyAddressIdentity<'_>], address: &str) -> Result<Vec<usize>, String> {
    let exact = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| (identity.record_name == address).then_some(index))
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return Ok(exact);
    }

    let (role, project) = parse_role_address(address)?;
    let matching = identities
        .iter()
        .enumerate()
        .filter_map(|(index, identity)| {
            (identity.role == Some(role) && project.is_none_or(|project| identity.project.unwrap_or_default() == project)).then_some(index)
        })
        .collect::<Vec<_>>();
    let (live, terminal): (Vec<_>, Vec<_>) = matching.into_iter().partition(|index| !identities[*index].terminal);
    let candidates = if live.is_empty() { terminal } else { live };
    let record_names = candidates.iter().map(|index| identities[*index].record_name).collect::<BTreeSet<_>>();
    if record_names.len() <= 1 {
        return Ok(candidates);
    }

    let address_options = candidates
        .iter()
        .filter_map(|index| identities[*index].role.map(|role| convoy_disambiguation_address(role, identities[*index].project)))
        .collect::<BTreeSet<_>>();
    if candidates.iter().all(|index| identities[*index].terminal) && address_options.len() == 1 {
        return Err(format!(
            "convoy address `{address}` matches multiple terminal records; use an exact record name: {}",
            record_names.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if address_options.len() > 1 {
        return Err(format!(
            "convoy role `{role}` is ambiguous; use one of: {}",
            address_options.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Err(format!(
        "convoy address `{address}` matches multiple records; use an exact record name: {}",
        record_names.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

async fn resolve_local_convoy_name(backend: &ResourceBackend, namespace: &str, address: &str) -> Result<String, String> {
    let convoys = backend.clone().using::<ResourceConvoy>(namespace);
    let candidates = convoys.list().await.map_err(|error| error.to_string())?.items;
    let identities = candidates
        .iter()
        .map(|convoy| ConvoyAddressIdentity {
            record_name: &convoy.metadata.name,
            role: convoy.metadata.labels.get(ROLE_LABEL).map(String::as_str),
            project: convoy.metadata.labels.get(PROJECT_LABEL).map(String::as_str),
            terminal: convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
        })
        .collect::<Vec<_>>();
    let selected = resolve_convoy_candidate_indices(&identities, address)?;
    match selected.as_slice() {
        [index] => Ok(candidates[*index].metadata.name.clone()),
        [] => Err(format!("no convoy matches `{address}`")),
        _ => Err(format!("convoy record `{address}` is present from multiple sources")),
    }
}

#[derive(bon::Builder)]
struct ConvoySnapshotBundle<'a> {
    spec: &'a ConvoySpec,
    workflow: &'a WorkflowTemplateSpec,
    placement: Option<&'a PlacementPolicySpec>,
    placement_decision: Option<PlacementDecision>,
}

/// An issue body is the crew's contract, so admission may only reuse a
/// recently observed snapshot. Keep this deliberately fixed until an
/// operational need establishes that it should be configurable.
const ISSUE_SNAPSHOT_FRESHNESS: ChronoDuration = ChronoDuration::minutes(5);
fn issue_snapshot_is_fresh(issue: &flotilla_protocol::Issue) -> bool {
    let Some(observed_at) = issue.observed_at else { return false };
    let age = Utc::now().signed_duration_since(observed_at);
    (ChronoDuration::zero()..=ISSUE_SNAPSHOT_FRESHNESS).contains(&age)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ConvoyStartKey {
    namespace: String,
    project_ref: String,
    subject: ConvoyStartSubject,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ConvoyStartSubject {
    ChangeRequest(String),
    Issues(Vec<flotilla_protocol::IssueSelector>),
    Name(String),
    Anonymous {
        branch: Option<String>,
        workflow_ref: Option<String>,
        inputs: Vec<(String, String)>,
        instruction: Option<String>,
        placement_policy: Option<String>,
    },
}

impl ConvoyStartKey {
    fn new(namespace: String, intent: &flotilla_protocol::ConvoyStartIntent) -> Self {
        let subject = if let Some(change_request) = &intent.change_request {
            ConvoyStartSubject::ChangeRequest(change_request.clone())
        } else if intent.issues.is_empty() {
            match &intent.name {
                Some(name) => ConvoyStartSubject::Name(name.clone()),
                None => ConvoyStartSubject::Anonymous {
                    branch: intent.branch.clone(),
                    workflow_ref: intent.workflow_ref.clone(),
                    inputs: intent.inputs.clone(),
                    instruction: intent.instruction.clone(),
                    placement_policy: intent.placement_policy.clone(),
                },
            }
        } else {
            ConvoyStartSubject::Issues(intent.issues.clone())
        };
        Self { namespace, project_ref: intent.project_ref.clone(), subject }
    }
}

struct ResolvedConvoyChangeRequestAdmission {
    binding: BoundChangeRequest,
    branch: String,
    base_ref: String,
}

fn convoy_start_failure(convoy: &ResourceObject<ResourceConvoy>) -> Option<String> {
    let role = if convoy.spec.role.is_empty() { &convoy.metadata.name } else { &convoy.spec.role };
    let identity = convoy.spec.project_ref.as_ref().map_or_else(|| role.clone(), |project| format!("{role}@{project}"));
    let status = convoy.status.as_ref()?;
    if let Some((work, state)) = status.work.iter().find(|(_, state)| state.phase == ResourceWorkPhase::Failed) {
        let detail = state.message.as_deref().filter(|message| !message.trim().is_empty()).unwrap_or("work failed without a message");
        return Some(format!("convoy {identity} failed while starting work {work}: {detail}"));
    }
    match status.phase {
        flotilla_resources::ConvoyPhase::Failed => Some(match status.message.as_deref().filter(|message| !message.trim().is_empty()) {
            Some(message) => format!("convoy {identity} failed while starting: {message}"),
            None => format!("convoy {identity} failed while starting"),
        }),
        flotilla_resources::ConvoyPhase::Cancelled => Some(format!("convoy {identity} was cancelled while starting")),
        flotilla_resources::ConvoyPhase::Pending
        | flotilla_resources::ConvoyPhase::Active
        | flotilla_resources::ConvoyPhase::Interrupted
        | flotilla_resources::ConvoyPhase::Anchored
        | flotilla_resources::ConvoyPhase::Landing
        | flotilla_resources::ConvoyPhase::Landed
        | flotilla_resources::ConvoyPhase::Abandoned => None,
    }
}

fn checkout_path(checkout: &ResourceObject<ResourceCheckout>) -> Option<&str> {
    checkout_path_from_status_and_spec(checkout.status.as_ref(), &checkout.spec)
}

fn condition_is_true(condition: &IntegrationCondition) -> bool {
    condition.value == ConditionValue::True
}

fn integration_condition_is_fresh(condition: &IntegrationCondition, now: chrono::DateTime<Utc>) -> bool {
    condition
        .observed_at
        .as_deref()
        .and_then(|observed_at| chrono::DateTime::parse_from_rfc3339(observed_at).ok())
        .and_then(|observed_at| now.signed_duration_since(observed_at).to_std().ok())
        .is_some_and(|age| age < LANDING_EVIDENCE_TTL)
}

fn condition_problem(label: &str, condition: &IntegrationCondition) -> Option<String> {
    match condition.value {
        ConditionValue::True => None,
        ConditionValue::False => Some(format!("{label}=False{}", condition_detail_suffix(condition))),
        ConditionValue::Unknown => Some(format!("{label}=Unknown{}", condition_detail_suffix(condition))),
    }
}

fn condition_detail_suffix(condition: &IntegrationCondition) -> String {
    if condition.details.is_empty() {
        String::new()
    } else {
        format!(" ({})", condition.details.join(", "))
    }
}

fn checkout_integration_summary(checkout: &ResourceObject<ResourceCheckout>, integration: &CheckoutIntegrationStatus) -> Option<String> {
    let problems = [
        condition_problem("Clean", &integration.clean),
        condition_problem("Pushed", &integration.pushed),
        condition_problem("Landed", &integration.landed),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if problems.is_empty() {
        None
    } else {
        Some(format!("{} [{}]: {}", checkout.metadata.name, checkout_path(checkout).unwrap_or("<unknown path>"), problems.join("; ")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingConvoyTarget {
    pub home: HostName,
    pub node_id: NodeId,
    pub namespace: String,
    pub record_name: String,
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ExistingConvoyTarget {
    pub fn unreachable_message(&self, cause: &str) -> String {
        convoy_home_unreachable_message(&self.namespace, &self.record_name, &self.home, self.last_seen_at, cause)
    }
}

fn convoy_home_unreachable_message(
    namespace: &str,
    record_name: &str,
    home: &HostName,
    last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    cause: &str,
) -> String {
    let last_seen = last_seen_at.map_or_else(|| "unknown".to_string(), |timestamp| timestamp.to_rfc3339());
    format!(
        "convoy {namespace}/{record_name} is homed at {home}, last seen {last_seen}; home is unreachable: {cause}. \
         Break glass: flotilla resource delete convoys {record_name} --namespace {namespace} --host {home}"
    )
}

fn managed_terminal_changes(
    previous: Option<&HashMap<flotilla_protocol::AttachableId, ManagedTerminal>>,
    current: &HashMap<flotilla_protocol::AttachableId, ManagedTerminal>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, terminal) in current {
        let op = match previous.and_then(|terminals| terminals.get(key)) {
            Some(previous) if previous == terminal => continue,
            Some(_) => EntryOp::Updated(terminal.clone()),
            None => EntryOp::Added(terminal.clone()),
        };
        changes.push(Change::ManagedTerminal { key: key.clone(), op });
    }
    if let Some(previous) = previous {
        for key in previous.keys().filter(|key| !current.contains_key(*key)) {
            changes.push(Change::ManagedTerminal { key: key.clone(), op: EntryOp::Removed });
        }
    }
    changes
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<flotilla_protocol::RepoIdentity, RepoState>>,
    repo_order: RwLock<Vec<flotilla_protocol::RepoIdentity>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    config: Arc<ConfigStore>,
    next_command_id: AtomicU64,
    node_id: NodeId,
    host_name: HostName,
    /// Maps local tracked paths (including virtual synthetic paths) to RepoIdentity.
    // Lock ordering: do not hold path_identities across awaits that later take
    // repos/repo_order; add_repo intentionally takes it last while already
    // holding those write locks.
    path_identities: RwLock<HashMap<PathBuf, flotilla_protocol::RepoIdentity>>,
    /// Repository identity last projected for each local tracked path.
    /// Mutated under `observed_checkout_reconciliation` so removal deletes
    /// observations using the identity that originally created them.
    repository_keys_by_path: RwLock<HashMap<PathBuf, RepositoryKey>>,
    host_registry: crate::host_registry::HostRegistry,
    local_environment_id: EnvironmentId,
    environment_manager: Arc<EnvironmentManager>,
    /// Discovery dependencies and configuration used for all daemon-side
    /// provider detection, both at startup and for later repo additions.
    discovery: DiscoveryRuntime,
    /// Running commands, keyed by command ID, for cancellation.
    active_commands: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    self_weak: Weak<InProcessDaemon>,
    pending_convoy_starts: Mutex<HashSet<ConvoyStartKey>>,
    /// Serializes pending-brief state with its terminal-session delivery side effect.
    convoy_message_locks: Mutex<HashMap<ConvoyMessageKey, WeakConvoyMessageLock>>,
    /// Serializes the identity selector check with Convoy creation. The owner
    /// host is the admission authority, so this is the local transaction that
    /// enforces one live generation per `{project, role}`.
    convoy_admission: Mutex<()>,
    /// Unique identity for this daemon instance, generated at startup.
    /// Used in peer Hello handshake to detect remote daemon restarts.
    session_id: uuid::Uuid,
    agent_state_store: crate::agents::SharedAgentStateStore,
    /// Socket path for the daemon server — set by the daemon after startup.
    /// Used to inject FLOTILLA_DAEMON_SOCKET into managed terminal sessions.
    daemon_socket_path: RwLock<Option<PathBuf>>,
    resource_backend: ResourceBackend,
    clock: Arc<dyn Clock>,
    regard_lifecycle: RegardLifecycle,
    observed_resource_backend: ResourceBackend,
    /// Serializes observed Checkout publication with repository removal so a
    /// refresh captured before untracking cannot recreate deleted resources.
    observed_checkout_reconciliation: Mutex<()>,
    aggregator_projection_state: AggregatorProjectionState,
    /// Provisioning namespace used by daemon-side resource operations (e.g.
    /// looking up the Convoy whose task is being marked complete). Set by the
    /// daemon runtime at startup; defaults to [`DEFAULT_PROVISIONING_NAMESPACE`].
    provisioning_namespace: std::sync::RwLock<String>,
    fleet_replica_cache: RwLock<HashMap<HostName, FleetReplicaCacheEntry>>,
    fleet_replica_tx: broadcast::Sender<Vec<FleetReplicaSnapshot>>,
    resource_replication_failures: RwLock<HashMap<NodeId, BTreeMap<String, String>>>,
    repository_inspector: RwLock<Option<Arc<dyn RepositoryInspector>>>,
    local_placement_provider_statuses: RwLock<Vec<HostProviderStatus>>,
    /// Last terminal state published per repository, used to emit field-scoped
    /// deltas without disturbing unrelated provider snapshot state.
    managed_terminals_by_repo: RwLock<HashMap<RepoIdentity, HashMap<flotilla_protocol::AttachableId, ManagedTerminal>>>,
    /// Filesystem path whose capacity governs convoy admission on this host.
    ///
    /// The daemon runtime sets this to the host-direct checkout root. Keeping
    /// the path here makes the local gate and the capacity published to peers
    /// use the same measurement basis even when daemon state is on another
    /// mount.
    admission_free_space_path: std::sync::RwLock<PathBuf>,
    leaf_subscriptions: LeafSubscriptionTable,
}

/// Default provisioning namespace used until [`InProcessDaemon::set_provisioning_namespace`]
/// is called. Matches `RuntimeOptions::namespace`'s default so tests that construct
/// the daemon directly hit the same namespace the runtime uses.
pub const DEFAULT_PROVISIONING_NAMESPACE: &str = "flotilla";
const FLEET_REPLICA_FRESH_SECS: i64 = 90;
const FLEET_REPLICA_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
const ENSURE_BACKOFF_RESET_AFTER: ChronoDuration = ChronoDuration::minutes(10);
const ENSURE_MAX_CONSECUTIVE_FAILURES: u32 = 3;
const ENSURE_ESCALATION_AFTER: ChronoDuration = ChronoDuration::minutes(15);
const ENSURE_HOLD_ATTENTION_PREFIX: &str = "ensure-attention-";
const RECLAIM_REFUSAL_REASON_ANNOTATION: &str = "flotilla.work/reclaim-refusal-reason";

fn ensure_retry_delay(restart_count: u32) -> ChronoDuration {
    let exponent = restart_count.min(5);
    ChronoDuration::seconds((30_i64.saturating_mul(1_i64 << exponent)).min(15 * 60))
}

fn ensure_config_hash(spec: &ConvoyEnsureSpec) -> Result<String, String> {
    let encoded = serde_json::to_vec(spec).map_err(|error| format!("serialize ensure config: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

/// Verifies the provider backing of a terminal standing convoy before the
/// ensure controller may reclaim it. Implementations must fail closed: `Ok`
/// means the backing was positively observed dead, while any live, unknown,
/// or uninspectable state is an error that holds teardown.
#[async_trait]
pub trait StandingConvoyBackingInspector: Send + Sync {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String>;
}

#[async_trait]
impl StandingConvoyBackingInspector for InProcessDaemon {
    async fn verify_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        self.verify_standing_convoy_resource_backing_dead(convoy).await
    }
}

impl InProcessDaemon {
    async fn resolve_convoy_issue_snapshot(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        let issue = self.fetch_issue_by_ref(reference).await?;
        if issue_snapshot_is_fresh(&issue) {
            Ok(issue)
        } else {
            Err(format!("issue {} snapshot is too stale to admit", reference.id))
        }
    }

    async fn admission_ai_utility(&self) -> Option<Arc<dyn AiUtility>> {
        let environment = self.environment_manager.environment_bag(&self.local_environment_id)?;
        let runner = self.environment_manager.environment_runner(&self.local_environment_id)?;
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        for factory in &self.discovery.factories.ai_utilities {
            if let Ok(utility) = factory.probe(&environment, &self.config, &probe_root, Arc::clone(&runner)).await {
                return Some(utility);
            }
        }
        None
    }

    /// Create a new in-process daemon tracking the given repo paths.
    ///
    /// Returns `Arc<Self>` because daemon-owned background controllers retain
    /// weak references to the process state.
    pub async fn new(repo_paths: Vec<PathBuf>, config: Arc<ConfigStore>, discovery: DiscoveryRuntime, host_name: HostName) -> Arc<Self> {
        Self::new_with_resource_backend(repo_paths, config, discovery, host_name, ResourceBackend::InMemory(Default::default())).await
    }

    pub async fn new_with_resource_backend(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
    ) -> Arc<Self> {
        Self::new_with_resource_backend_and_clock(repo_paths, config, discovery, host_name, resource_backend, Arc::new(SystemClock)).await
    }

    pub async fn new_with_resource_backend_and_clock(
        repo_paths: Vec<PathBuf>,
        config: Arc<ConfigStore>,
        discovery: DiscoveryRuntime,
        host_name: HostName,
        resource_backend: ResourceBackend,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        use crate::providers::discovery::DiscoveryResult;

        let (event_tx, _) = broadcast::channel(256);
        let mut repos: HashMap<flotilla_protocol::RepoIdentity, RepoState> = HashMap::new();
        let mut order = Vec::new();
        let mut path_identities = HashMap::new();
        let mut repository_keys_by_path = HashMap::new();

        let daemon_config = config.load_daemon_config().expect("failed to load daemon config");
        let config_machine_id = daemon_config.machine_id.as_deref();
        let local_environment_state_dir =
            resolve_local_environment_state_dir(config.state_dir().as_path(), config_machine_id, &*discovery.runner).await;
        let local_node_id = resolve_local_node_id(config.base_path().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local node id");
        let resource_backend = resource_backend.with_local_root(local_node_id.clone());
        let local_environment_id =
            resolve_or_create_environment_id(&local_environment_state_dir).expect("failed to resolve local direct environment id");
        let local_host_id = resolve_local_host_id(config.state_dir().as_path(), config_machine_id, &*discovery.runner)
            .await
            .expect("failed to resolve local host id");
        let environment_manager =
            Arc::new(EnvironmentManager::new_local(&discovery, local_environment_id.clone(), local_host_id.clone()).await);
        register_static_ssh_direct_environments(&config, &discovery, &environment_manager).await;
        let agent_state_store = crate::agents::shared_file_backed_agent_state_store(config.base_path());
        let startup_repository_inspector = GitRepositoryInspector::new(discovery.runner.clone(), local_host_id.to_string());

        for path in repo_paths {
            if path_identities.contains_key(&path) {
                continue;
            }
            let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
                &environment_manager,
                &discovery,
                &config,
                &local_environment_id,
                &local_environment_id,
                &path,
            )
            .await
            .expect("local direct environment discovery should always be available");
            if !unmet.is_empty() {
                debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
            }

            let identity = configured_repo_identity_or_bag_or_path(&config, &path, &host_repo_bag);
            match startup_repository_inspector.inspect_path(&path, None).await {
                Ok(inspection) => {
                    repository_keys_by_path.insert(path.clone(), inspection.key());
                }
                Err(error) => {
                    warn!(repo = %path.display(), %error, "repository key is unavailable during daemon startup");
                }
            }
            let slug = repo_slug.clone();
            let model = RepoModel::new(registry, Some(local_environment_id.clone()));
            let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                order.push(identity.clone());
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
            }
            path_identities.insert(path.clone(), identity);
        }

        let local_provider_statuses = crate::host_summary::provider_statuses_from_registries(
            repos.values().map(|state| state.preferred_root().model.registry.as_ref()),
        );
        let local_host_summary = crate::host_summary::build_local_host_summary(
            &local_node_id,
            &host_name,
            EnvironmentId::host(environment_manager.local_host_id().clone()),
            &environment_manager,
            local_provider_statuses,
            &*discovery.env,
        )
        .await;

        let (fleet_replica_tx, _) = broadcast::channel(32);
        let change_request_refresher = crate::change_request_observer::ChangeRequestRefresher::new(
            resource_backend.clone(),
            local_node_id.to_string(),
            Arc::new(crate::change_request_observer::GhChangeRequestObservationSource::new(discovery.runner.clone())),
            crate::change_request_observer::ChangeRequestRefreshCadence::default(),
        );
        if let Err(error) = change_request_refresher.garbage_collect_orphans().await {
            tracing::warn!(%error, "garbage collect orphaned change request observations at startup failed");
        }
        let leaf_subscriptions = LeafSubscriptionTable::new(resource_backend.clone(), event_tx.clone(), change_request_refresher);
        let admission_free_space_path = config.state_dir().as_path().to_path_buf();
        let daemon = Arc::new_cyclic(|self_weak| Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx: event_tx.clone(),
            config,
            next_command_id: AtomicU64::new(1),
            node_id: local_node_id.clone(),
            host_name: host_name.clone(),
            path_identities: RwLock::new(path_identities),
            repository_keys_by_path: RwLock::new(repository_keys_by_path),
            host_registry: crate::host_registry::HostRegistry::new(
                NodeInfo::new(local_node_id.clone(), host_name.to_string()),
                local_host_summary,
            ),
            local_environment_id,
            environment_manager,
            discovery,
            active_commands: Arc::new(Mutex::new(HashMap::new())),
            self_weak: self_weak.clone(),
            pending_convoy_starts: Mutex::new(HashSet::new()),
            convoy_message_locks: Mutex::new(HashMap::new()),
            convoy_admission: Mutex::new(()),
            session_id: uuid::Uuid::new_v4(),
            agent_state_store,
            daemon_socket_path: RwLock::new(None),
            clock: Arc::clone(&clock),
            regard_lifecycle: RegardLifecycle::new(resource_backend.clone(), clock, ChronoDuration::seconds(DEFAULT_REGARD_DECAY_SECONDS)),
            resource_backend,
            observed_resource_backend: ResourceBackend::InMemory(InMemoryBackend::observed()),
            observed_checkout_reconciliation: Mutex::new(()),
            aggregator_projection_state: AggregatorProjectionState::new(),
            provisioning_namespace: std::sync::RwLock::new(DEFAULT_PROVISIONING_NAMESPACE.to_string()),
            fleet_replica_cache: RwLock::new(HashMap::new()),
            fleet_replica_tx,
            resource_replication_failures: RwLock::new(HashMap::new()),
            repository_inspector: RwLock::new(None),
            local_placement_provider_statuses: RwLock::new(Vec::new()),
            managed_terminals_by_repo: RwLock::new(HashMap::new()),
            admission_free_space_path: std::sync::RwLock::new(admission_free_space_path),
            leaf_subscriptions: leaf_subscriptions.clone(),
        });
        leaf_subscriptions.set_turn_delivery_actuator(Arc::new(DaemonTurnDeliveryActuator { daemon: Arc::downgrade(&daemon) })).await;

        let weak = Arc::downgrade(&daemon);
        tokio::spawn(async move {
            let mut expiry = tokio::time::interval(Duration::from_secs(1));
            expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut refresh = tokio::time::interval(Duration::from_secs(DEFAULT_REGARD_REFRESH_SECONDS));
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = expiry.tick() => {
                        let Some(daemon) = weak.upgrade() else { break };
                        let namespace = daemon.provisioning_namespace().await;
                        if let Err(error) = daemon.regard_lifecycle.expire_due(&namespace).await {
                            warn!(%error, "failed to expire due regards");
                        }
                    }
                    _ = refresh.tick() => {
                        let Some(daemon) = weak.upgrade() else { break };
                        if let Err(error) = daemon.regard_lifecycle.refresh_focused().await {
                            warn!(%error, "failed to refresh focused regards");
                        }
                    }
                }
            }
        });

        daemon
    }

    /// Returns the host name for this daemon.
    pub fn host_name(&self) -> &HostName {
        &self.host_name
    }

    pub fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    /// Returns the session ID for this daemon instance.
    ///
    /// Generated once at startup via `Uuid::new_v4()`. Used in peer Hello
    /// handshake so peers can detect daemon restarts.
    pub fn session_id(&self) -> uuid::Uuid {
        self.session_id
    }

    pub async fn local_host_summary(&self) -> HostSummary {
        self.refresh_local_host_summary().await
    }

    pub async fn set_local_placement_capabilities(&self, agent_adapters: &BTreeSet<String>, terminal_pools: &[String]) {
        let mut statuses = agent_adapters
            .iter()
            .map(|adapter| HostProviderStatus::available(AGENT_ADAPTER_PROVIDER_CATEGORY, adapter))
            .chain(terminal_pools.iter().map(|pool| HostProviderStatus::available(TERMINAL_POOL_PROVIDER_CATEGORY, pool)))
            .collect::<Vec<_>>();
        statuses.sort_by(|left, right| (&left.category, &left.implementation).cmp(&(&right.category, &right.implementation)));
        *self.local_placement_provider_statuses.write().await = statuses;
        let _ = self.refresh_local_host_summary().await;
    }

    /// Use `path` as the canonical capacity source for both local and
    /// federated convoy admission.
    pub fn set_admission_free_space_path(&self, path: PathBuf) {
        *self.admission_free_space_path.write().expect("admission free-space path lock poisoned") = path;
    }

    pub async fn admission_free_space_bytes(&self) -> Result<Option<u64>, String> {
        let path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let probe = Arc::clone(&self.discovery.available_space_probe);
        tokio::task::spawn_blocking(move || probe.measure(&path)).await.map_err(|error| format!("measure available disk space: {error}"))
    }

    pub fn local_environment_id(&self) -> &EnvironmentId {
        &self.local_environment_id
    }

    pub fn local_command_runner(&self) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(&self.local_environment_id)
    }

    pub async fn set_repository_inspector(&self, inspector: Arc<dyn RepositoryInspector>) {
        *self.repository_inspector.write().await = Some(inspector);
    }

    async fn repository_inspector(&self) -> Result<Arc<dyn RepositoryInspector>, String> {
        if let Some(inspector) = self.repository_inspector.read().await.clone() {
            return Ok(inspector);
        }
        let runner = self.local_command_runner().ok_or_else(|| "local repository inspector is unavailable".to_string())?;
        let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?;
        Ok(Arc::new(GitRepositoryInspector::new(runner, host_ref.to_string())))
    }

    pub async fn inspect_repository_path(&self, path: &Path, remote: Option<&str>) -> Result<RepositoryInspection, String> {
        let mut inspection = self.repository_inspector().await?.inspect_path(path, remote).await?;
        inspection.spec = self.configure_inspected_repository(&inspection.checkout.path, inspection.spec).await?;
        Ok(inspection)
    }

    async fn configure_inspected_repository(&self, path: &Path, spec: RepositorySpec) -> Result<RepositorySpec, String> {
        let resolved_spec = self.resolve_declared_repository(spec).await?;
        let configured_spec = self.config.configure_repository_spec(&ExecutionEnvironmentPath::new(path), resolved_spec.clone())?;
        if resolved_spec.remotes().len() > 1 && configured_spec.key() != resolved_spec.key() {
            return Err(format!("repository config for {} changes the canonical remote of an existing declaration", path.display()));
        }
        Ok(configured_spec)
    }

    pub async fn repository_key_for_path(&self, path: &Path) -> Option<RepositoryKey> {
        self.repository_keys_by_path.read().await.get(path).cloned()
    }

    async fn resolve_repository_remote(&self, remote: &str) -> Result<RepositorySpec, String> {
        let spec = self.repository_inspector().await?.resolve_remote(remote).await?;
        self.resolve_declared_repository(spec).await
    }

    async fn resolve_declared_repository(&self, observed: RepositorySpec) -> Result<RepositorySpec, String> {
        let flotilla_resources::RepositoryIdentity::Remote { canonical_remote } = observed.identity() else {
            return Ok(observed);
        };
        let namespace = self.provisioning_namespace().await;
        let matching_sources = self
            .resource_backend
            .clone()
            .including_replicas::<Repository>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .map(|repository| repository.object)
            .filter(|repository| repository.spec.declares_remote(canonical_remote))
            .collect::<Vec<_>>();
        let mut matches_by_name = BTreeMap::<String, Vec<ResourceObject<Repository>>>::new();
        for repository in matching_sources {
            matches_by_name.entry(repository.metadata.name.clone()).or_default().push(repository);
        }
        let mut matches = Vec::with_capacity(matches_by_name.len());
        for sources in matches_by_name.into_values() {
            let declared_remotes = sources
                .iter()
                .filter(|repository| repository.spec.remotes().len() > 1)
                .map(|repository| repository.spec.remotes())
                .collect::<BTreeSet<_>>();
            if declared_remotes.len() > 1 {
                return Err(format!("remote `{canonical_remote}` has conflicting declarations for one Repository"));
            }
            matches.push(
                sources
                    .iter()
                    .find(|repository| repository.spec.remotes().len() > 1)
                    .unwrap_or_else(|| sources.first().expect("Repository source group cannot be empty"))
                    .clone(),
            );
        }
        let declared = matches.iter().filter(|repository| repository.spec.remotes().len() > 1).collect::<Vec<_>>();
        match declared.as_slice() {
            [repository] => return observed.with_remotes(repository.spec.remotes().iter().cloned()),
            [_, _, ..] => return Err(format!("remote `{canonical_remote}` is declared by multiple Repositories")),
            [] => {}
        }
        match matches.as_slice() {
            [] => Ok(observed),
            [repository] => observed.with_remotes(repository.spec.remotes().iter().cloned()),
            _ => Err(format!("remote `{canonical_remote}` is declared by multiple Repositories")),
        }
    }

    async fn inspect_adopted_checkout(
        &self,
        path: &Path,
        repository_url: Option<&str>,
        git_ref: Option<&str>,
    ) -> Result<RepositoryInspection, String> {
        if let (Some(repository_url), Some(git_ref)) = (repository_url, git_ref) {
            if let Ok(spec) = RepositorySpec::remote(repository_url) {
                let path = std::fs::canonicalize(path)
                    .map_err(|error| format!("adopted checkout path {} cannot be resolved: {error}", path.display()))?;
                let spec = self.configure_inspected_repository(&path, spec).await?;
                let host_ref = self.local_host_id().ok_or_else(|| "local Host identity is unavailable".to_string())?.to_string();
                return Ok(RepositoryInspection {
                    spec,
                    checkout: crate::repository_inspection::LocalCheckoutInspection {
                        path,
                        host_ref,
                        git_ref: git_ref.to_string(),
                        is_main: matches!(git_ref, "main" | "master" | "trunk"),
                    },
                    transport_url: Some(repository_url.to_string()),
                });
            }
        }
        self.inspect_repository_path(path, repository_url).await
    }

    pub fn local_environment_bag(&self) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(&self.local_environment_id)
    }

    pub async fn fetch_issue_by_ref(&self, reference: &flotilla_protocol::IssueRef) -> Result<flotilla_protocol::Issue, String> {
        self.issue_provider_for_source(&reference.source).await?.fetch_by_id(reference).await
    }

    /// Resolve a portable issue source to a provider capability installed on
    /// this host. Provider names and credentials remain local.
    pub async fn issue_provider_for_source(&self, source: &flotilla_protocol::IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        let host_bag = self
            .environment_manager
            .environment_bag(&self.local_environment_id)
            .ok_or_else(|| format!("environment not found: {}", self.local_environment_id))?;
        let runner = self
            .environment_manager
            .environment_runner(&self.local_environment_id)
            .ok_or_else(|| format!("environment runner not found: {}", self.local_environment_id))?;
        let probe_root = ExecutionEnvironmentPath::new(self.config.base_path().as_ref());
        let host_scoped = self
            .discovery
            .host_scoped_providers
            .discover_for_environment(&self.local_environment_id, &host_bag, &self.discovery.factories, &self.config, &probe_root, runner)
            .await;
        let provider = host_scoped
            .issue_provider_for(source)
            .ok_or_else(|| format!("no issue provider available for {} {}", source.service, source.scope))?;
        Ok(provider)
    }

    /// Resolve a curated query scope to external issue sources. Repository
    /// keys live in the daemon provisioning namespace; Project scopes carry
    /// their namespace explicitly.
    pub async fn resolve_issue_sources(
        &self,
        scope: &flotilla_protocol::QueryScope,
    ) -> Result<Vec<flotilla_protocol::IssueSource>, String> {
        let project = self
            .resource_backend
            .clone()
            .definitions::<Project>(&scope.namespace)
            .get(&scope.name)
            .await
            .map_err(|error| format!("project {}/{}: {error}", scope.namespace, scope.name))?;
        match resolve_project_issue_sources(&self.resource_backend.including_replicas::<Repository>(&scope.namespace), &project.spec).await
        {
            IssueSourceResolution::Available { sources } => Ok(sources),
            IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { repository, message }) => {
                Err(format!("repository {repository}: {message}"))
            }
            IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource) => {
                Err(format!("project {}/{} has no issue source", scope.namespace, scope.name))
            }
        }
    }

    pub fn command_runner_for_environment(&self, env_id: &EnvironmentId) -> Option<Arc<dyn CommandRunner>> {
        self.environment_manager.environment_runner(env_id)
    }

    pub fn environment_bag_for_environment(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(env_id)
    }

    pub fn environment_registry_for_environment(
        &self,
        env_id: &EnvironmentId,
    ) -> Option<Arc<crate::providers::registry::ProviderRegistry>> {
        self.environment_manager.environment_registry(env_id)
    }

    pub fn environment_container_name(&self, env_id: &EnvironmentId) -> Option<String> {
        self.environment_manager.environment_container_name(env_id)
    }

    pub fn register_provisioned_environment(
        &self,
        env_id: EnvironmentId,
        handle: crate::providers::environment::EnvironmentHandle,
        env_bag: EnvironmentBag,
        registry: Option<Arc<crate::providers::registry::ProviderRegistry>>,
    ) -> Result<(), String> {
        self.environment_manager.register_provisioned_environment(env_id, handle, env_bag, registry)
    }

    pub fn remove_provisioned_environment(&self, env_id: &EnvironmentId) -> bool {
        self.environment_manager.remove_provisioned_environment(env_id).is_some()
    }

    pub fn discovery_runtime(&self) -> &DiscoveryRuntime {
        &self.discovery
    }

    pub fn local_host_id(&self) -> Option<flotilla_protocol::qualified_path::HostId> {
        self.environment_manager.host_id_for_environment(&self.local_environment_id)
    }

    pub fn host_id_for_environment(&self, env_id: &EnvironmentId) -> Option<flotilla_protocol::qualified_path::HostId> {
        self.environment_manager.host_id_for_environment(env_id)
    }

    pub fn agent_state_store(&self) -> &crate::agents::SharedAgentStateStore {
        &self.agent_state_store
    }

    pub async fn set_daemon_socket_path(&self, path: PathBuf) {
        *self.daemon_socket_path.write().await = Some(path);
    }

    pub async fn daemon_socket_path(&self) -> Option<PathBuf> {
        self.daemon_socket_path.read().await.clone()
    }

    /// Override the provisioning namespace used for daemon-side resource lookups
    /// (e.g. `ConvoyWorkForceComplete`). Called by the daemon runtime at startup with
    /// `RuntimeOptions::namespace`.
    pub async fn set_provisioning_namespace(&self, namespace: String) {
        *self.provisioning_namespace.write().expect("provisioning namespace lock poisoned") = namespace;
    }

    pub async fn provisioning_namespace(&self) -> String {
        self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    fn start_context_free_command(&self, command_id: u64, description: String) -> flotilla_protocol::RepoIdentity {
        let repo_identity = empty_repo_identity();
        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id,
            node_id: self.node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: None,
            description,
        });
        repo_identity
    }

    fn finish_context_free_command(
        &self,
        command_id: u64,
        repo_identity: flotilla_protocol::RepoIdentity,
        result: flotilla_protocol::CommandValue,
    ) {
        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
            command_id,
            node_id: self.node_id.clone(),
            repo_identity,
            repo: None,
            result,
        });
    }

    pub async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.clone()
    }

    pub fn subscribe_fleet_replicas(&self) -> broadcast::Receiver<Vec<FleetReplicaSnapshot>> {
        self.fleet_replica_tx.subscribe()
    }

    pub async fn cached_fleet_replica_snapshots(&self) -> Vec<FleetReplicaSnapshot> {
        self.fleet_replica_cache
            .read()
            .await
            .iter()
            .map(|(host, entry)| FleetReplicaSnapshot {
                host: host.clone(),
                generation: entry.generation.clone(),
                rows: entry.rows.clone(),
                result_sets: entry.result_sets.clone(),
            })
            .collect()
    }

    pub fn resource_backend(&self) -> ResourceBackend {
        self.resource_backend.clone()
    }

    pub async fn subscribe_wait(
        &self,
        connection_id: uuid::Uuid,
        request: flotilla_protocol::WaitSubscriptionRequest,
    ) -> Result<uuid::Uuid, String> {
        self.leaf_subscriptions.subscribe_wait(connection_id, request).await
    }

    pub async fn unsubscribe_waits(&self, connection_id: uuid::Uuid) {
        self.leaf_subscriptions.unsubscribe_connection(connection_id).await;
    }

    pub fn reconciler_wake_watch(&self) -> Box<dyn flotilla_resources::controller::SecondaryWatch<Primary = flotilla_resources::Convoy>> {
        self.leaf_subscriptions.reconciler_wake_watch()
    }

    pub fn change_request_stale_after(&self) -> Duration {
        self.leaf_subscriptions.change_request_stale_after()
    }

    pub fn connect_surface(&self, surface_id: uuid::Uuid, declaration: SurfaceDeclaration) {
        self.regard_lifecycle.connect_surface(surface_id, declaration);
    }

    pub fn principal_for_surface(&self, surface_id: uuid::Uuid) -> Result<Option<PrincipalRef>, String> {
        self.regard_lifecycle.principal_for_surface(surface_id)
    }

    fn should_auto_attach(&self, requested: flotilla_protocol::ConvoyAutoAttach) -> bool {
        match requested {
            flotilla_protocol::ConvoyAutoAttach::Always => true,
            flotilla_protocol::ConvoyAutoAttach::Never => false,
            flotilla_protocol::ConvoyAutoAttach::Default => {
                self.config.load_config().convoy.auto_attach.unwrap_or_else(|| !self.regard_lifecycle.has_ambient_surface())
            }
        }
    }

    pub async fn disconnect_surface(&self, surface_id: uuid::Uuid) -> Result<(), String> {
        self.regard_lifecycle.disconnect_surface(surface_id).await
    }

    pub async fn observe_surface_focus(&self, surface_id: uuid::Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        self.regard_lifecycle.observe_focus(surface_id, targets).await
    }

    pub fn observed_resource_backend(&self) -> ResourceBackend {
        self.observed_resource_backend.clone()
    }

    /// Refresh durable integration observations for adopted checkouts, then
    /// restore their ephemeral query-facing projection.
    pub async fn reconcile_adopted_checkouts(&self, namespace: &str) -> Result<(), String> {
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        crate::observed_resources::reconcile_adopted_checkouts(&self.resource_backend, &self.observed_resource_backend, namespace)
            .await
            .map_err(|error| error.to_string())?;
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        for checkout in checkouts.list().await.map_err(|error| error.to_string())?.items {
            if checkout.metadata.lifecycle_authority().map_err(|error| error.to_string())? != Some(LifecycleAuthority::Adopted) {
                continue;
            }
            let Some(path) = checkout_path(&checkout) else {
                continue;
            };
            let runner = self.runner_for_resource_checkout(&checkout).await?;
            let convoy_ref = checkout.metadata.labels.get(CONVOY_LABEL).map(String::as_str);
            let source_root = checkout.metadata.annotations.get(ACTUATOR_SOURCE_ROOT_ANNOTATION).map(String::as_str);
            let convoy = self
                .resource_backend
                .clone()
                .including_replicas::<ResourceConvoy>(namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .find_map(|source| {
                    let origin_matches = match (source_root, &source.provenance) {
                        (Some(expected), ResourceProvenance::Replica { origin_root, .. }) => origin_root.as_str() == expected,
                        (Some(_), ResourceProvenance::Local) => false,
                        (None, _) => true,
                    };
                    let association_matches = convoy_ref.map_or_else(
                        || {
                            flotilla_resources::expected_checkout_refs(&source.object)
                                .is_ok_and(|expected| expected.contains(&checkout.metadata.name))
                        },
                        |expected| source.object.metadata.name == expected,
                    );
                    (origin_matches && association_matches).then_some(source.object)
                });
            let integration = if let Some(convoy) = convoy.as_ref() {
                let change_request_id = convoy_change_request_id_for_checkout(convoy, &checkout);
                inspect_convoy_checkout_integration(&*runner, Path::new(path), &checkout.spec, change_request_id.as_deref(), None).await
            } else {
                inspect_checkout_integration(
                    &*runner,
                    Path::new(path),
                    &checkout.spec,
                    checkout.metadata.labels.get(flotilla_resources::CHANGE_REQUEST_ID_LABEL).map(String::as_str),
                )
                .await
            };
            apply_resource_status_patch(&checkouts, &checkout.metadata.name, &flotilla_resources::CheckoutStatusPatch::UpdateIntegration {
                integration: Box::new(integration),
            })
            .await
            .map_err(|error| error.to_string())?;
        }
        crate::observed_resources::reconcile_adopted_checkouts(&self.resource_backend, &self.observed_resource_backend, namespace)
            .await
            .map_err(|error| error.to_string())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn register_direct_environment_for_test(
        &self,
        env_id: EnvironmentId,
        runner: Arc<dyn CommandRunner>,
        env_bag: EnvironmentBag,
        host_id: Option<flotilla_protocol::qualified_path::HostId>,
    ) -> Result<(), String> {
        self.environment_manager.register_direct_environment(env_id, runner, env_bag, host_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn register_provisioned_environment_for_test(
        &self,
        env_id: EnvironmentId,
        handle: crate::providers::environment::EnvironmentHandle,
        env_bag: EnvironmentBag,
    ) -> Result<(), String> {
        self.environment_manager.register_provisioned_environment(env_id, handle, env_bag, None)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_local_environment_bag_for_test(&self, env_bag: EnvironmentBag) -> Result<(), String> {
        self.environment_manager.replace_local_environment_bag_for_test(env_bag)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn managed_environment_ids_for_test(&self) -> Vec<EnvironmentId> {
        self.environment_manager.managed_environments().into_iter().map(|(env_id, _)| env_id).collect()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn environment_bag_for_test(&self, env_id: &EnvironmentId) -> Option<EnvironmentBag> {
        self.environment_manager.environment_bag(env_id)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn discover_repo_for_environment_for_test(
        &self,
        repo_path: &Path,
        environment_id: &EnvironmentId,
    ) -> Result<DiscoveryResult, String> {
        discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            environment_id,
            repo_path,
        )
        .await
    }

    /// Returns the current connection status for a peer host.
    pub async fn peer_connection_status(&self, node_id: &NodeId) -> PeerConnectionState {
        self.host_registry.peer_connection_status(node_id).await
    }

    pub async fn connected_peer_node_ids(&self) -> Vec<NodeId> {
        let mut peers =
            self.host_registry.connected_peer_summaries().await.into_iter().map(|summary| summary.node.node_id).collect::<Vec<_>>();
        peers.sort();
        peers.dedup();
        peers
    }

    pub async fn set_configured_peers(&self, peers: Vec<NodeInfo>) {
        let remote_counts = HashMap::new();
        self.host_registry
            .set_configured_peers(peers, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn set_peer_host_summaries(&self, summaries: HashMap<EnvironmentId, HostSummary>) {
        let remote_counts = HashMap::new();
        self.host_registry
            .set_peer_host_summaries(summaries, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn publish_peer_connection_status(&self, node: &NodeInfo, status: PeerConnectionState) {
        let remote_counts = HashMap::new();
        self.host_registry
            .publish_peer_connection_status(node, status, &remote_counts, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn begin_peer_resource_replication(&self, peer: &NodeId) {
        self.resource_replication_failures.write().await.remove(peer);
    }

    pub async fn report_resource_replication_failure(&self, peer: &NodeId, kind: &str, message: &str) {
        self.resource_replication_failures.write().await.entry(peer.clone()).or_default().insert(kind.to_string(), message.to_string());
    }

    pub async fn report_resource_replication_healthy(&self, peer: &NodeId, kind: &str) {
        let mut failures = self.resource_replication_failures.write().await;
        let Some(peer_failures) = failures.get_mut(peer) else {
            return;
        };
        peer_failures.remove(kind);
        if peer_failures.is_empty() {
            failures.remove(peer);
        }
    }

    pub async fn publish_peer_summary(&self, summary: HostSummary) {
        self.host_registry
            .publish_peer_summary(summary, &|e| {
                let _ = self.event_tx.send(e);
            })
            .await;
    }

    pub async fn remote_placement_host(
        &self,
        namespace: &str,
        policy_name: Option<&str>,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        let Some(policy_name) = policy_name else {
            return Ok(None);
        };
        let policy = self
            .resource_backend
            .clone()
            .including_replicas::<PlacementPolicy>(namespace)
            .get(policy_name)
            .await
            .map(|source| source.object)
            .map_err(|error| format!("placement policy {policy_name}: {error}"))?;
        let target_host = placement_target_host(&self.resource_backend, namespace, &policy).await?;
        if self.canonical_local_host_id().as_ref().is_some_and(|host_id| host_id == &target_host.reference) {
            return Ok(None);
        }
        Ok(Some(flotilla_protocol::qualified_path::HostId::new(target_host.reference.as_str())))
    }

    pub async fn convoy_start_placement_host(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
    ) -> Result<Option<flotilla_protocol::qualified_path::HostId>, String> {
        if intent.placement_policy.is_some() {
            return self.remote_placement_host(namespace, intent.placement_policy.as_deref()).await;
        }

        let (project_namespace, project_ref) = resolve_project_ref(namespace, &intent.project_ref)?;
        let project = self
            .resource_backend
            .clone()
            .including_replicas::<Project>(&project_namespace)
            .get(&project_ref)
            .await
            .map(|source| source.object)
            .map_err(|error| project_not_ready_error(&project_namespace, &project_ref, error))?;
        let repositories = self.snapshot_project_repositories(&project_namespace, &project_ref).await?;
        let (_, workflow) =
            self.resolve_convoy_admission_workflow(&project_namespace, &project_ref, &project.spec, &repositories, intent, None).await?;
        let placement = self.resolve_and_validate_convoy_placement(&project_namespace, &workflow, None).await?;
        let Some(policy) = placement.selected else {
            return Ok(None);
        };
        let target_host = placement_target_host(&self.resource_backend, &project_namespace, &policy).await?;
        if self.canonical_local_host_id().as_ref().is_some_and(|host_id| host_id == &target_host.reference) {
            return Ok(None);
        }
        Ok(Some(flotilla_protocol::qualified_path::HostId::new(target_host.reference.as_str())))
    }

    pub async fn resolve_existing_convoy_target(
        &self,
        action: &flotilla_protocol::CommandAction,
    ) -> Result<Option<ExistingConvoyTarget>, String> {
        let (namespace, name) = match action {
            flotilla_protocol::CommandAction::ConvoyDelete { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyAbandon { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyResume { namespace, name, .. }
            | flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { namespace, name }
            | flotilla_protocol::CommandAction::QueryExplainConvoy { namespace, name } => {
                (namespace.clone().unwrap_or(self.provisioning_namespace().await), name.as_str())
            }
            flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, .. } => {
                (self.provisioning_namespace().await, convoy.as_str())
            }
            flotilla_protocol::CommandAction::CrewComplete { context, .. }
            | flotilla_protocol::CommandAction::CrewFail { context, .. }
            | flotilla_protocol::CommandAction::CrewHandoff { context, .. }
            | flotilla_protocol::CommandAction::QueryCrewList { context } => {
                let namespace = context.namespace.clone().unwrap_or(self.provisioning_namespace().await);
                let name = context.convoy.as_deref().ok_or_else(|| "crew command was not resolved to a convoy".to_string())?;
                (namespace, name)
            }
            _ => return Ok(None),
        };

        let result_set = self.aggregator_projection_state().await.result_set().await;
        let Rows::Convoys { rows, .. } = result_set.rows else {
            return Ok(None);
        };
        let candidates = rows.into_iter().filter(|row| row.resource.namespace == namespace).collect::<Vec<_>>();
        let identities = candidates
            .iter()
            .map(|row| ConvoyAddressIdentity {
                record_name: &row.resource.name,
                role: row.address_role.as_deref(),
                project: row.project_ref.as_deref(),
                terminal: row.phase.is_terminal(),
            })
            .collect::<Vec<_>>();
        let selected = resolve_convoy_candidate_indices(&identities, name)?;
        let record_name = selected.first().map(|index| candidates[*index].resource.name.clone()).unwrap_or_else(|| name.to_string());
        let mut hosts = selected.into_iter().filter_map(|index| candidates[index].resource.host.clone()).collect::<Vec<_>>();
        hosts.sort();
        hosts.dedup();

        let home = match hosts.as_slice() {
            [] => return Ok(None),
            [host] if host == &self.host_name => {
                return Ok(Some(ExistingConvoyTarget {
                    home: host.clone(),
                    node_id: self.node_id.clone(),
                    namespace,
                    record_name,
                    last_seen_at: None,
                }));
            }
            [host] => host.clone(),
            _ => {
                let homes = hosts.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(format!("convoy {name} is present on multiple home hosts: {homes}"));
            }
        };

        let last_seen_at =
            self.resource_backend.including_replicas::<ResourceConvoy>(&namespace).get(&record_name).await.ok().and_then(|source| {
                match source.provenance {
                    flotilla_resources::ResourceProvenance::Replica { last_synced_at, .. } => Some(last_synced_at),
                    flotilla_resources::ResourceProvenance::Local => None,
                }
            });

        let node_id = self.host_registry.node_id_for_host_name(&home).await?.ok_or_else(|| {
            convoy_home_unreachable_message(&namespace, &record_name, &home, last_seen_at, "no routed node address found for host")
        })?;
        Ok(Some(ExistingConvoyTarget { home, node_id, namespace, record_name, last_seen_at }))
    }

    pub async fn has_authoritative_convoy(&self, namespace: &str, name: &str) -> Result<bool, String> {
        match self.resource_backend.clone().using::<ResourceConvoy>(namespace).get(name).await {
            Ok(_) => Ok(true),
            Err(ResourceError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    pub async fn set_topology_routes(&self, routes: Vec<TopologyRoute>) {
        self.host_registry.set_topology_routes(routes).await;
    }

    async fn local_host_counts(&self) -> HashMap<EnvironmentId, HostCounts> {
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut counts: HashMap<EnvironmentId, HostCounts> = HashMap::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            let Some(environment_id) = state.preferred_environment_id().cloned() else {
                continue;
            };
            let entry = counts.entry(environment_id).or_default();
            entry.repo_count += 1;
        }

        counts
    }

    /// Resolve a repo identity to the preferred local path for execution or overlay updates.
    pub async fn preferred_local_path_for_identity(&self, identity: &flotilla_protocol::RepoIdentity) -> Option<PathBuf> {
        self.repos.read().await.get(identity).map(|state| state.preferred_path().to_path_buf())
    }

    /// Resolve a tracked local or synthetic repo path to its stable repo identity.
    pub async fn tracked_repo_identity_for_path(&self, repo_path: &Path) -> Option<flotilla_protocol::RepoIdentity> {
        self.path_identities.read().await.get(repo_path).cloned()
    }

    async fn detect_repo_identity(&self, repo_path: &Path) -> flotilla_protocol::RepoIdentity {
        let repo_root = ExecutionEnvironmentPath::new(repo_path);
        if let Some(source) = self.config.resolve_repo_issue_source(&repo_root) {
            return flotilla_protocol::RepoIdentity { authority: source.service, path: source.scope };
        }
        match discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &self.local_environment_id,
            repo_path,
        )
        .await
        {
            Ok(result) => repo_identity_from_bag_or_path(repo_path, &result.host_repo_bag),
            Err(_) => fallback_repo_identity(repo_path),
        }
    }

    /// Returns the paths of all locally tracked repos.
    ///
    /// Only local repo paths, not remote/virtual ones. Used by the outbound
    /// task to send local state to a newly connected peer.
    pub async fn tracked_repo_paths(&self) -> Vec<PathBuf> {
        self.repos.read().await.values().flat_map(RepoState::local_paths).collect()
    }

    async fn resolve_repo_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        match selector {
            flotilla_protocol::RepoSelector::Path(path) => {
                let identities = self.path_identities.read().await;
                if identities.contains_key(path) {
                    Ok(path.clone())
                } else {
                    Err(format!("repo not tracked: {}", path.display()))
                }
            }
            flotilla_protocol::RepoSelector::Query(query) => {
                let repos = self.repos.read().await;
                let entries: Vec<_> = repos.values().map(|state| (state.preferred_path(), state.slug())).collect();
                crate::resolve::resolve_repo(query, entries.into_iter()).map_err(|e| e.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => self
                .repos
                .read()
                .await
                .get(identity)
                .map(|state| state.preferred_path().to_path_buf())
                .ok_or_else(|| format!("repo not tracked: {identity}")),
        }
    }

    async fn resolve_checkout_selector(
        &self,
        selector: &flotilla_protocol::CheckoutSelector,
        scope: &CheckoutResolutionScope,
    ) -> Result<(PathBuf, String), String> {
        let repos = self.repos.read().await;
        let mut matches = Vec::new();
        for state in repos.values() {
            let root = state.preferred_root();
            let repo_root = ExecutionEnvironmentPath::new(&root.path);
            let Some((_, manager)) = root.model.registry.checkout_managers.preferred_with_desc() else { continue };
            let checkouts = manager.list_checkouts(&repo_root).await.map_err(|error| format!("checkout discovery failed: {error}"))?;
            for (checkout_path, checkout) in checkouts {
                let host_path =
                    QualifiedPath::host(self.environment_manager.local_host_id().clone(), checkout_path.as_path().to_path_buf());
                if !checkout_matches_scope(&host_path, &checkout, &self.host_name, scope) {
                    continue;
                }
                let matched = match selector {
                    flotilla_protocol::CheckoutSelector::Path(path) => host_path.path == *path,
                    flotilla_protocol::CheckoutSelector::Query(query) => {
                        checkout.branch == *query || checkout.branch.contains(query) || host_path.path.to_string_lossy().contains(query)
                    }
                };
                if matched {
                    matches.push((state.preferred_path().to_path_buf(), checkout.branch));
                }
            }
        }
        match matches.len() {
            0 => Err("checkout not found".into()),
            1 => Ok(matches.remove(0)),
            _ => Err("checkout selector is ambiguous".into()),
        }
    }

    async fn resolve_repo_for_command(&self, command: &Command) -> Result<PathBuf, String> {
        use flotilla_protocol::CommandAction;

        let checkout_scope = match (&command.provisioning_target, command.node_id.as_ref()) {
            (Some(flotilla_protocol::ProvisioningTarget::Host { host }), _) => CheckoutResolutionScope::Host(host.clone()),
            (_, Some(node_id)) if *node_id != self.node_id => CheckoutResolutionScope::RemoteAny,
            _ => CheckoutResolutionScope::Any,
        };

        match &command.action {
            CommandAction::Checkout { repo, .. } => self.resolve_repo_selector(repo).await,
            CommandAction::RemoveCheckout { checkout, .. } => {
                if let Some(selector) = command.context_repo.as_ref() {
                    self.resolve_repo_selector(selector).await
                } else {
                    self.resolve_checkout_selector(checkout, &checkout_scope).await.map(|(repo, _)| repo)
                }
            }
            CommandAction::Refresh { repo: Some(selector) } => self.resolve_repo_selector(selector).await,
            CommandAction::FetchCheckoutStatus { .. }
            | CommandAction::OpenChangeRequest { .. }
            | CommandAction::CloseChangeRequest { .. }
            | CommandAction::MergeChangeRequest { .. }
            | CommandAction::OpenIssue { .. }
            | CommandAction::LinkIssuesToChangeRequest { .. }
            | CommandAction::ArchiveSession { .. }
            | CommandAction::GenerateBranchName { .. }
            | CommandAction::TeleportSession { .. }
            | CommandAction::CreateWorkspaceForCheckout { .. }
            | CommandAction::CreateWorkspaceFromPreparedTerminal { .. }
            | CommandAction::PrepareTerminalForCheckout { .. }
            | CommandAction::SelectWorkspace { .. } => {
                let selector = command.context_repo.as_ref().ok_or_else(|| "command requires repo context".to_string())?;
                self.resolve_repo_selector(selector).await
            }
            _ => Err("command does not resolve to a single repo".to_string()),
        }
    }

    async fn repository_action_policy_error(&self, command: &Command, repo: &Path) -> Option<String> {
        let CommandAction::MergeChangeRequest { id, .. } = &command.action else {
            return None;
        };
        let repository_key = match self.repository_keys_by_path.read().await.get(repo).cloned() {
            Some(key) => key,
            None => {
                return Some(format!(
                    "cannot determine whether merging change request {id} is permitted: repository policy is unavailable"
                ));
            }
        };
        let namespace = self.provisioning_namespace().await;
        let repository = match self.resource_backend.clone().using::<Repository>(&namespace).get(&repository_key.to_string()).await {
            Ok(repository) => repository,
            Err(error) => {
                return Some(format!(
                    "cannot determine whether merging change request {id} is permitted: repository policy is unavailable: {error}"
                ));
            }
        };
        repository
            .spec
            .is_fork()
            .then(|| format!("merging change request {id} is forbidden for fork-stance repository; landing is human-only"))
    }

    /// Resolve an explicitly requested change request across the project's
    /// snapshotted repositories and capture its admission identity.
    async fn resolve_convoy_change_request_admission(
        &self,
        repository_keys: &[RepositoryKey],
        requested_id: &str,
    ) -> Result<ResolvedConvoyChangeRequestAdmission, String> {
        let candidates = {
            let keys_by_path = self.repository_keys_by_path.read().await;
            let repos = self.repos.read().await;
            let order = self.repo_order.read().await;
            let mut seen = HashSet::new();
            let mut candidates = Vec::new();
            for identity in order.iter() {
                let Some(state) = repos.get(identity) else { continue };
                for root in &state.roots {
                    let Some(repository) = keys_by_path.get(&root.path).filter(|repository| repository_keys.contains(repository)) else {
                        continue;
                    };
                    if seen.contains(repository) {
                        continue;
                    }
                    let providers =
                        root.model.registry.change_requests.iter().map(|(_, provider)| Arc::clone(provider)).collect::<Vec<_>>();
                    if !providers.is_empty() {
                        seen.insert(repository.clone());
                        candidates.push((repository.clone(), root.path.clone(), providers));
                    }
                }
            }
            candidates
        };

        let mut matches = Vec::new();
        let mut failures = Vec::new();
        for (repository, path, providers) in candidates {
            let mut matched = None;
            for provider in providers {
                match provider.get_change_request_for_admission(&path, requested_id).await {
                    Ok(admission) => {
                        let Some(base_ref) = admission.base_ref else {
                            failures.push(format!("repository {repository}: change request {} did not report a base ref", admission.id));
                            continue;
                        };
                        matched = Some(ResolvedConvoyChangeRequestAdmission {
                            binding: BoundChangeRequest {
                                id: admission.id,
                                repository_ref: repository.clone(),
                                title: admission.change_request.title,
                            },
                            branch: admission.change_request.branch,
                            base_ref,
                        });
                        break;
                    }
                    Err(error) => failures.push(format!("repository {repository}: {error}")),
                }
            }
            if let Some(matched) = matched {
                matches.push(matched);
            }
        }

        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(format!(
                "change request {requested_id} was not found in project repositories{}",
                if failures.is_empty() { String::new() } else { format!(": {}", failures.join("; ")) }
            )),
            count => Err(format!("change request {requested_id} is ambiguous across {count} project repositories")),
        }
    }

    /// Resolve the first change request whose head matches a convoy branch
    /// across the convoy's snapshotted repositories.
    pub async fn resolve_convoy_change_request(
        &self,
        repository_keys: &[RepositoryKey],
        branch: &str,
        change_request_id: Option<&str>,
    ) -> Result<Option<ConvoyChangeRequest>, String> {
        let live_candidates = {
            let keys_by_path = self.repository_keys_by_path.read().await;
            let repos = self.repos.read().await;
            let order = self.repo_order.read().await;
            let mut live_by_key = HashMap::new();

            for identity in order.iter() {
                let Some(state) = repos.get(identity) else { continue };
                let Some(repository) = state
                    .roots
                    .iter()
                    .filter_map(|root| keys_by_path.get(&root.path))
                    .find(|repository| repository_keys.contains(repository))
                    .cloned()
                else {
                    continue;
                };

                if !live_by_key.contains_key(&repository) {
                    for root in &state.roots {
                        if keys_by_path.get(&root.path) != Some(&repository) {
                            continue;
                        }
                        let providers =
                            root.model.registry.change_requests.iter().map(|(_, provider)| Arc::clone(provider)).collect::<Vec<_>>();
                        if !providers.is_empty() {
                            live_by_key.insert(repository.clone(), (root.path.clone(), providers));
                            break;
                        }
                    }
                }
            }

            let live_candidates = repository_keys
                .iter()
                .filter_map(|repository| live_by_key.remove(repository).map(|(path, providers)| (repository.clone(), path, providers)))
                .collect::<Vec<_>>();
            live_candidates
        };

        let mut first_error = None;
        for (repository, path, providers) in live_candidates {
            for provider in providers {
                let resolved = match change_request_id {
                    Some(id) => provider.get_change_request(&path, id).await.map(Some),
                    None => provider.find_change_request_by_branch(&path, branch).await,
                };
                match resolved {
                    Ok(Some((id, request))) => {
                        return Ok(Some(ConvoyChangeRequest { id, status: request.status, repository_key: repository }));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }

    /// Add a virtual repo (no local filesystem path) for a remote-only repo.
    ///
    /// Unlike `add_repo`, this skips provider discovery entirely — there is
    /// no local path to scan. Instead it creates a dormant `RepoState` with
    /// an empty provider registry and an idle refresh handle.
    ///
    /// The `synthetic_path` serves as a stable key for tab identity (e.g.
    /// `<remote>/desktop/home/dev/repo`).
    ///
    /// Emits `DaemonEvent::RepoTracked`.
    pub async fn add_virtual_repo(
        &self,
        identity: flotilla_protocol::RepoIdentity,
        repository_key: Option<RepositoryKey>,
        synthetic_path: PathBuf,
    ) -> Result<(), String> {
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        let existing_path = self.repos.read().await.get(&identity).map(|state| state.preferred_path().to_path_buf());
        if let Some(existing_path) = existing_path {
            let key_became_available = if let Some(repository_key) = repository_key {
                self.repository_keys_by_path.write().await.insert(existing_path, repository_key.clone()).as_ref() != Some(&repository_key)
            } else {
                false
            };
            drop(_reconciliation);
            if key_became_available {
                self.publish_repo_info_update(&identity).await;
            }
            return Ok(());
        }

        let model = RepoModel::new_virtual();

        let repo_info = RepoInfo {
            identity: identity.clone(),
            repository_key: repository_key.clone(),
            path: Some(synthetic_path.clone()),
            name: repo_name(&synthetic_path),
            labels: model.labels.clone(),
            provider_names: provider_names_from_registry(&model.registry)
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if repos.contains_key(&identity) {
                return Ok(());
            }
            repos.insert(
                identity.clone(),
                RepoState::new(identity.clone(), RepoRootState {
                    path: synthetic_path.clone(),
                    model,
                    slug: None,
                    repo_bag: EnvironmentBag::new(),
                    unmet: Vec::new(),
                    is_local: false,
                }),
            );
            order.push(identity.clone());
        }

        self.path_identities.write().await.insert(synthetic_path.clone(), identity);
        if let Some(repository_key) = repository_key {
            self.repository_keys_by_path.write().await.insert(synthetic_path.clone(), repository_key);
        }

        // Virtual repos are not persisted to config — they come and go
        // with peer connections.

        info!(repo = %synthetic_path.display(), "added virtual repo");
        let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));

        Ok(())
    }

    /// Send an arbitrary event to all subscribers.
    ///
    /// Mirrors host events into daemon-owned host state so replay/query paths
    /// can use a single authoritative source of truth.
    ///
    /// For peer status changes, prefer [`publish_peer_connection_status`](Self::publish_peer_connection_status)
    /// which emits both a `PeerStatusChanged` and a `HostSnapshot` for live subscribers.
    /// Calling `send_event(PeerStatusChanged)` directly only updates replay state.
    pub fn send_event(&self, event: DaemonEvent) {
        self.host_registry.apply_event(&event);
        let _ = self.event_tx.send(event);
    }

    /// Return a clone of the broadcast sender so background tasks (e.g.
    /// the Aggregator) can emit events into the daemon-wide event bus.
    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }
}

/// Non-trait methods that are called directly on the concrete `InProcessDaemon`
/// type by the daemon server peer-overlay code and by the `execute()` implementation.
fn repository_matches_target(repository: &ResourceObject<Repository>, target: &str) -> bool {
    repository.metadata.name == target || repository.spec.matches_catalog_target(target)
}

async fn ensure_default_workflows(backend: &ResourceBackend, namespace: &str) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    for (name, spec) in [
        ("single-agent-contained", flotilla_resources::single_agent_contained_workflow_spec()),
        ("single-agent-shepherd", flotilla_resources::single_agent_shepherd_workflow_spec()),
        ("single-agent-trusted", flotilla_resources::single_agent_trusted_workflow_spec()),
        ("implement-review", flotilla_resources::implement_review_workflow_spec()),
    ] {
        let meta = InputMeta::builder().name(name.to_string()).build();
        match templates.create(&meta, &spec).await {
            Ok(_) | Err(ResourceError::Conflict { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn prepared_snapshot_name(kind: &str, spec: &serde_json::Value) -> Result<String, String> {
    let suffix = flotilla_resources::content_hash(spec).map_err(|error| error.to_string())?;
    Ok(format!("{kind}-snapshot-{suffix}"))
}

async fn ensure_prepared_workflow_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &WorkflowTemplateSpec,
) -> Result<(), String> {
    let templates = backend.clone().using::<WorkflowTemplate>(namespace);
    match templates.get(name).await {
        Ok(existing) if existing.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared workflow snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => templates
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::WORKFLOW_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

async fn ensure_prepared_placement_snapshot(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    spec: &PlacementPolicySpec,
) -> Result<(), String> {
    let policies = backend.clone().using::<PlacementPolicy>(namespace);
    match policies.get(name).await {
        Ok(existing) if existing.spec == *spec => Ok(()),
        Ok(_) => Err(format!("prepared placement snapshot {name} already exists with different contents")),
        Err(ResourceError::NotFound { .. }) => policies
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([(
                        flotilla_resources::PREPARED_SNAPSHOT_LABEL.to_string(),
                        flotilla_resources::PLACEMENT_SNAPSHOT_KIND.to_string(),
                    )]))
                    .build(),
                spec,
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_project_name(name: &str) -> Result<(), String> {
    let normalized = normalize_project_name(name)?;
    if normalized != name {
        return Err(format!("project name `{name}` is invalid; use `{normalized}`"));
    }
    Ok(())
}

fn normalize_project_name(name: &str) -> Result<String, String> {
    let normalized = name
        .trim()
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        return Err("project name must contain an alphanumeric character".to_string());
    }
    Ok(normalized)
}

async fn ensure_repository_and_default_project_workflow(
    backend: &ResourceBackend,
    namespace: &str,
    repository_key: &RepositoryKey,
    repository_spec: &RepositorySpec,
) -> Result<(), String> {
    flotilla_resources::ensure_repository(&backend.clone().using::<Repository>(namespace), repository_key, repository_spec)
        .await
        .map_err(|error| error.to_string())?;
    ensure_default_workflows(backend, namespace).await?;
    flotilla_resources::PreparedSnapshotGarbageCollector::new(backend.clone(), namespace)
        .collect(None)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn whole_repository_project_spec(repository_key: RepositoryKey, display_name: String) -> Result<ProjectSpec, String> {
    normalize_project_spec(ProjectSpec {
        display_name,
        default_workflow_ref: "single-agent-trusted".to_string(),
        issue_source: None,
        repositories: vec![ProjectRepositorySpec {
            repo: repository_key,
            alias: None,
            roles: Default::default(),
            subpath: None,
            default_branch: None,
        }],
        dispatch_policy: None,
    })
}

const WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE: &str = "whole-repository-project";

fn whole_repository_project_meta(name: impl Into<String>) -> InputMeta {
    InputMeta::builder()
        .name(name.into())
        .labels(BTreeMap::from([(MANAGED_BY_LABEL.to_string(), WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE.to_string())]))
        .build()
}

/// Marks an existing whole-repository Project as generator-materialized without
/// changing its user-owned definition.
///
/// Materialization is intentionally one-way: it fills a missing Project, but a
/// later refresh must not reinterpret any part of an existing spec as
/// generator-owned. Explicit Project operations are the only way to refresh a
/// materialized definition.
async fn reconcile_whole_repository_project_definition(
    projects: &flotilla_resources::DefinitionResolver<Project>,
    existing: ResourceObject<Project>,
) -> Result<ResourceObject<Project>, String> {
    if is_declaration_backed_project(&existing) {
        return Ok(existing);
    }
    let managed_by_generator =
        existing.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE);
    if managed_by_generator {
        return Ok(existing);
    }

    let mut meta = InputMeta::from(&existing.metadata);
    meta.labels.insert(MANAGED_BY_LABEL.to_string(), WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE.to_string());
    let reconciled = projects
        .apply(&meta, &existing.spec)
        .await
        .map_err(|error| format!("reconcile generated whole-repository Project {}: {error}", existing.metadata.name))?;
    Ok(reconciled)
}

fn is_declaration_backed_project(project: &ResourceObject<Project>) -> bool {
    project.metadata.annotations.contains_key(BOOTSTRAP_REPOSITORY_ANNOTATION)
}

fn is_whole_repository_project(spec: &ProjectSpec, repository_key: &RepositoryKey) -> bool {
    matches!(
        spec.repositories.as_slice(),
        [entry] if &entry.repo == repository_key && entry.subpath.is_none()
    )
}

fn whole_repository_project_names(repository_spec: &RepositorySpec) -> Result<Vec<String>, String> {
    let repository_key = repository_spec.key();
    let display_name = normalize_project_name(&repository_spec.leaf_slug())?;
    let catalog_name = normalize_project_name(&repository_spec.catalog_slug())?;
    let key_suffix = repository_key.0.chars().take(8).collect::<String>();
    let disambiguated_name = format!("{catalog_name}-{key_suffix}");
    let mut candidates = Vec::new();
    for candidate in [display_name, catalog_name, disambiguated_name] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn repository_identity_display(spec: &RepositorySpec) -> String {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
        flotilla_resources::RepositoryIdentity::Local { .. } => "local".to_string(),
    }
}

fn local_repository_matches_checkout(spec: &RepositorySpec, checkout: &crate::repository_inspection::LocalCheckoutInspection) -> bool {
    match spec.identity() {
        flotilla_resources::RepositoryIdentity::Local { host_ref, git_common_dir } => {
            host_ref == &checkout.host_ref && Path::new(git_common_dir).parent() == Some(checkout.path.as_path())
        }
        flotilla_resources::RepositoryIdentity::Remote { .. } => false,
    }
}

#[derive(Debug)]
pub struct AddRepoOutcome {
    pub tracked_path: PathBuf,
    pub resolved_from: Option<PathBuf>,
    pub identity_change: Option<RepositoryIdentityChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectTargetSyntax {
    ExplicitPath,
    QualifiedSlug,
    Ambiguous,
}

fn project_target_syntax(target: &str) -> ProjectTargetSyntax {
    let path = Path::new(target);
    if path.is_absolute() || target.starts_with("./") || target.starts_with("../") {
        ProjectTargetSyntax::ExplicitPath
    } else if target.contains('/') {
        ProjectTargetSyntax::QualifiedSlug
    } else {
        ProjectTargetSyntax::Ambiguous
    }
}

fn required_admission_value<'a>(value: &'a str, field: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(value)
    }
}

fn resolve_project_ref(default_namespace: &str, value: &str) -> Result<(String, String), String> {
    let value = required_admission_value(value, "project")?;
    let address_value = value.strip_prefix(flotilla_protocol::view_address::SCHEME_PREFIX).unwrap_or(value);
    let has_scheme = address_value != value;
    if has_scheme || (address_value.starts_with("project/") && address_value.split('/').count() != 2) {
        return match value.parse::<ViewAddress>() {
            Ok(ViewAddress::Project { namespace, name }) => Ok((namespace, name)),
            Ok(address) => Err(format!("invalid project reference {value}: expected a project address, got {}", address.kind_name())),
            Err(error) => Err(format!("invalid project reference {value}: {error}")),
        };
    }
    match value.split('/').collect::<Vec<_>>().as_slice() {
        [name] => Ok((default_namespace.to_string(), (*name).to_string())),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => Ok(((*namespace).to_string(), (*name).to_string())),
        _ => Err(format!("invalid project reference {value}: expected <name>, <namespace>/<name>, or project/<namespace>/<name>")),
    }
}

fn normalize_convoy_start_intent(
    default_namespace: &str,
    intent: &flotilla_protocol::ConvoyStartIntent,
) -> Result<(String, flotilla_protocol::ConvoyStartIntent), String> {
    let (namespace, project_ref) = resolve_project_ref(default_namespace, &intent.project_ref)?;
    let mut intent = intent.clone();
    intent.namespace = Some(namespace.clone());
    intent.project_ref = project_ref;
    Ok((namespace, intent))
}

fn project_not_ready_error(namespace: &str, project_ref: &str, error: ResourceError) -> String {
    match error {
        ResourceError::NotFound { name } => {
            format!("project {namespace}/{project_ref} is not ready: resource not found: {name} (tried {namespace}/{project_ref})")
        }
        error => format!("project {project_ref} is not ready: {error}"),
    }
}

fn workflow_has_in_crew_review(workflow: &WorkflowTemplateSpec) -> bool {
    workflow.vessels.iter().any(|vessel| {
        let agent_count = vessel.crew.iter().filter(|crew| matches!(crew.source, CrewSource::Agent { .. })).count();
        agent_count > 1
            && vessel.crew.iter().any(|crew| {
                matches!(
                    &crew.source,
                    CrewSource::Agent { selector, .. } if matches!(selector.capability.as_str(), "review" | "code-review")
                )
            })
    })
}

async fn validate_fork_workflow_admission(
    backend: &ResourceBackend,
    namespace: &str,
    repositories: &[ConvoyRepositorySpec],
    workflow_ref: &str,
    workflow: &WorkflowTemplateSpec,
) -> Result<(), String> {
    if workflow_has_in_crew_review(workflow) {
        return Ok(());
    }
    let resolver = backend.including_replicas::<Repository>(namespace);
    for repository in repositories {
        let repository =
            resolver.get(&repository.repo_ref.to_string()).await.map_err(|error| format!("repository {}: {error}", repository.repo_ref))?;
        if repository.object.spec.is_fork() && !repository.object.spec.allows_reviewless_workflows() {
            return Err(format!("workflow {workflow_ref} not permitted for fork-stance repository — use implement-review"));
        }
    }
    Ok(())
}

async fn validate_workflow_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
) -> Result<(), String> {
    let required_adapters = required_workflow_agent_adapters(workflow)?;

    for adapter in required_adapters {
        let Some(placement) = placement else {
            return Err(format!("workflow requires agent adapter `{adapter}`, but no placement is available"));
        };
        let (available_adapters, detail) = placement_agent_adapters(backend, namespace, placement).await?;
        if available_adapters.contains(&adapter) {
            continue;
        }
        return Err(format!(
            "workflow requires agent adapter `{adapter}`, which is not available in placement `{}` ({detail})",
            placement.metadata.name
        ));
    }

    Ok(())
}

async fn resolve_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    project_ref: Option<&str>,
    repositories: &[ConvoyRepositorySpec],
    workflow: &mut WorkflowTemplateSpec,
) -> Result<(), String> {
    let grants = backend
        .including_replicas::<CredentialGrant>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential grants: {error}"))?
        .items;
    let specs = backend
        .including_replicas::<CredentialSpec>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .map(|source| source.object.metadata.name)
        .collect::<BTreeSet<_>>();
    let all_repositories = repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<BTreeSet<_>>();

    for vessel in &mut workflow.vessels {
        let vessel_repositories = vessel
            .repository_refs
            .as_ref()
            .map(|repositories| repositories.iter().cloned().collect())
            .unwrap_or_else(|| all_repositories.clone());
        let matching_grants = grants
            .iter()
            .filter(|source| source.object.spec.selector.matches(vessel.stance, project_ref, &vessel_repositories))
            .collect::<Vec<_>>();
        let granted = matching_grants.iter().flat_map(|grant| grant.object.spec.credentials.iter().cloned()).collect::<BTreeSet<_>>();
        if let Some(missing) = granted.iter().find(|name| !specs.contains(*name)) {
            return Err(format!("credential grant references missing credential `{missing}`"));
        }
        let mut credential_scopes = BTreeMap::<String, BTreeSet<_>>::new();
        for grant in matching_grants {
            let covered_repositories = if grant.object.spec.selector.repositories.is_empty() {
                vessel_repositories.clone()
            } else {
                grant.object.spec.selector.repositories.intersection(&vessel_repositories).cloned().collect()
            };
            for credential in &grant.object.spec.credentials {
                credential_scopes.entry(credential.clone()).or_default().extend(covered_repositories.iter().cloned());
            }
        }
        vessel.credential_refs = granted;
        vessel.credential_scopes = credential_scopes;
    }
    Ok(())
}

async fn validate_workflow_credentials(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
) -> Result<(), String> {
    validate_workflow_credentials_with_capabilities(backend, namespace, workflow, placement, &CapabilityTable::seeded()).await
}

async fn validate_workflow_credentials_with_capabilities(
    backend: &ResourceBackend,
    namespace: &str,
    workflow: &WorkflowTemplateSpec,
    placement: Option<&ResourceObject<PlacementPolicy>>,
    capabilities: &CapabilityTable,
) -> Result<(), String> {
    let specs = backend
        .including_replicas::<CredentialSpec>(namespace)
        .list()
        .await
        .map_err(|error| format!("list credential specs: {error}"))?
        .items
        .into_iter()
        .map(|source| (source.object.metadata.name, source.object.spec.consumer))
        .collect::<BTreeMap<_, _>>();
    for vessel in &workflow.vessels {
        for crew in &vessel.crew {
            let CrewSource::Agent { selector, .. } = &crew.source else {
                continue;
            };
            let requirement = capabilities.resolve_selector(selector)?;
            let Some(delivery_slot) = requirement.credential_delivery_slot() else {
                continue;
            };
            let has_granted_credential =
                vessel.credential_refs.iter().any(|name| specs.get(name).is_some_and(|consumer| consumer.delivery_slot() == delivery_slot));
            if has_granted_credential {
                continue;
            }
            let compatible = specs
                .iter()
                .filter(|(_, consumer)| consumer.delivery_slot() == delivery_slot)
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            let credential = match compatible.as_slice() {
                [name] => format!("credential `{name}`"),
                [] => format!("a `{delivery_slot}` credential"),
                names => format!("one of credentials `{}`", names.join("`, `")),
            };
            return Err(format!(
                "{} agent adapter `{}` requires {credential}, but no matching CredentialGrant selected it",
                vessel.stance, requirement.adapter
            ));
        }
    }

    let required = workflow.vessels.iter().flat_map(|vessel| vessel.credential_refs.iter().cloned()).collect::<BTreeSet<_>>();
    let ambient_dependent_vessels = ambient_credential_dependent_vessels(capabilities, &specs, workflow)?;
    if required.is_empty() && ambient_dependent_vessels.is_empty() {
        return Ok(());
    }
    let Some(placement) = placement else {
        if let Some(first) = required.first() {
            return Err(format!("workflow requires credential `{first}`, but no placement is available"));
        }
        // Ambient-dependent vessels without a placement have no target host to
        // check expiry against; admission proceeds as before.
        return Ok(());
    };
    let target_host = placement_target_host(backend, namespace, placement).await?;
    let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
    let host_label = target_host.display_name;
    let generation = host_generation(host.status.as_ref()).to_string();
    let Some(mut status) = host.status else {
        if required.is_empty() {
            return Ok(());
        }
        return Err(format!(
            "placement `{}` host `{host_label}` generation `{generation}` has no observed status",
            placement.metadata.name
        ));
    };
    status.apply_heartbeat_readiness(Utc::now());
    if !required.is_empty() {
        if !status.ready {
            return Err(placement_host_not_ready_reason(&placement.metadata.name, &host_label, &generation, &status));
        }
        let held = status.held_credentials().map_err(|error| {
            format!(
                "placement `{}` host `{host_label}` generation `{generation}` has invalid held-credential capability: {error}",
                placement.metadata.name
            )
        })?;
        if let Some(missing) = required.iter().find(|credential| !held.contains(*credential)) {
            return Err(format!(
                "workflow requires credential `{missing}`, which placement `{}` host `{host_label}` generation `{generation}` does not hold",
                placement.metadata.name
            ));
        }
    }
    let expiry = status.credential_expiry().map_err(|error| {
        format!("placement `{}` host `{host_label}` has invalid credential expiry capability: {error}", placement.metadata.name)
    })?;
    let now = Utc::now();
    for credential in &required {
        if let Some(expired_at) = expiry.get(credential).and_then(|entry| entry.expired_at(now)) {
            return Err(format!(
                "credential `{credential}` expired on host `{host_label}` on {} — refresh its material before dispatching",
                expired_at.format("%Y-%m-%d")
            ));
        }
    }
    for (vessel, scope) in &ambient_dependent_vessels {
        if let Some(expired_at) = expiry.get(*scope).and_then(|entry| entry.expired_at(now)) {
            return Err(format!(
                "vessel `{vessel}` depends on the ambient claude login on host `{host_label}`, which expired on {} — \
                 log in again on that host or grant a delivered claude credential",
                expired_at.format("%Y-%m-%d")
            ));
        }
    }
    Ok(())
}

/// Vessels whose agent crews will authenticate through a host's ambient login
/// rather than delivered material: non-contained vessels with a crew on an
/// ambient-capable adapter and no granted credential covering that adapter's
/// delivery slot. Returns `(vessel name, ambient scope)` pairs, the scope
/// being the entry name under the Host `credential_expiry` capability.
/// The seeded adapters currently pair ambient Claude scope with a delivery
/// slot, so this is forward-provisioned for an ambient-only adapter.
fn ambient_credential_dependent_vessels<'workflow>(
    capabilities: &CapabilityTable,
    specs: &BTreeMap<String, CredentialConsumer>,
    workflow: &'workflow WorkflowTemplateSpec,
) -> Result<Vec<(&'workflow str, &'static str)>, String> {
    let mut vessels = Vec::new();
    for vessel in workflow.vessels.iter().filter(|vessel| vessel.stance != flotilla_resources::Stance::Contained) {
        for crew in &vessel.crew {
            let CrewSource::Agent { selector, .. } = &crew.source else {
                continue;
            };
            let requirement = capabilities.resolve_selector(selector)?;
            let Some(scope) = requirement.ambient_credential_scope() else {
                continue;
            };
            let delivery_slot = requirement.credential_delivery_slot();
            let has_delivered_credential = delivery_slot.is_some_and(|slot| {
                vessel.credential_refs.iter().any(|name| specs.get(name).is_some_and(|consumer| consumer.delivery_slot() == slot))
            });
            if !has_delivered_credential {
                vessels.push((vessel.name.as_str(), scope));
                break;
            }
        }
    }
    Ok(vessels)
}

/// Write dispatch-time agent choices into the workflow spec that is about to
/// be snapshotted, so every downstream consumer — placement validation, the
/// vessel reconciler, terminal launch — reads the effective requirement from
/// the selector itself. Loud on anything that cannot take effect: a
/// capability named twice, or one no agent selector in the workflow carries.
/// Dispatch overrides cross the protocol boundary from arbitrary clients, but
/// adapter ids and model names land in fields the launch layer treats as
/// resolver-trusted (`Arg`'s safety invariant). Constrain them to the token
/// charset real harness and model names use before they enter the snapshot.
fn valid_agent_override_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn apply_agent_overrides(workflow: &mut WorkflowTemplateSpec, overrides: &[flotilla_protocol::AgentOverride]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for choice in overrides {
        if !seen.insert(choice.capability.as_str()) {
            return Err(format!("duplicate --agent override for capability `{}`", choice.capability));
        }
        if !valid_agent_override_token(&choice.adapter) {
            return Err(format!("agent adapter `{}` may only contain alphanumerics, `.`, `_`, and `-`", choice.adapter));
        }
        if let Some(model) = &choice.model {
            if !valid_agent_override_token(model) {
                return Err(format!("agent model `{model}` may only contain alphanumerics, `.`, `_`, and `-`"));
            }
        }
        let mut matched = false;
        for crew in workflow.vessels.iter_mut().flat_map(|vessel| &mut vessel.crew) {
            if let CrewSource::Agent { selector, .. } = &mut crew.source {
                if selector.capability == choice.capability {
                    selector.adapter = Some(choice.adapter.clone());
                    selector.model = choice.model.clone();
                    matched = true;
                }
            }
        }
        if !matched {
            let available = workflow
                .vessels
                .iter()
                .flat_map(|vessel| &vessel.crew)
                .filter_map(|crew| match &crew.source {
                    CrewSource::Agent { selector, .. } => Some(selector.capability.as_str()),
                    CrewSource::Tool { .. } => None,
                })
                .collect::<BTreeSet<_>>();
            if available.is_empty() {
                return Err(format!(
                    "--agent override names capability `{}`, but this workflow has no agent crew to override",
                    choice.capability
                ));
            }
            return Err(format!(
                "--agent override names capability `{}`, but this workflow's agent capabilities are: {}",
                choice.capability,
                available.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    Ok(())
}

fn required_workflow_agent_adapters(workflow: &WorkflowTemplateSpec) -> Result<BTreeSet<String>, String> {
    required_agent_adapters(workflow.vessels.iter().flat_map(|vessel| &vessel.crew))
}

async fn placement_agent_adapters(
    backend: &ResourceBackend,
    namespace: &str,
    placement: &ResourceObject<PlacementPolicy>,
) -> Result<(BTreeSet<String>, String), String> {
    if let Some(docker) = &placement.spec.docker_per_vessel {
        Ok((docker.agent_adapters.clone(), format!("image `{}`", docker.image)))
    } else if placement.spec.host_direct.is_some() {
        let target_host = placement_target_host(backend, namespace, placement).await?;
        let host = authoritative_placement_host(backend, namespace, &target_host, &placement.metadata.name).await?;
        let host_label = target_host.display_name;
        let generation = host_generation(host.status.as_ref()).to_string();
        let mut status = host.status.ok_or_else(|| {
            format!("placement `{}` host `{host_label}` generation `{generation}` has no observed status", placement.metadata.name)
        })?;
        status.apply_heartbeat_readiness(Utc::now());
        if !status.ready {
            return Err(placement_host_not_ready_reason(&placement.metadata.name, &host_label, &generation, &status));
        }
        let available_adapters = status.agent_adapters().map_err(|error| {
            format!(
                "placement `{}` host `{}` generation `{generation}` has invalid agent adapter capabilities: {error}",
                placement.metadata.name, host_label
            )
        })?;
        Ok((available_adapters, format!("host `{host_label}`")))
    } else {
        Ok((BTreeSet::new(), "unknown target environment".to_string()))
    }
}

fn convoy_fallback_slug(title: &str, id: &str) -> String {
    let slug = format!("{title}-{id}")
        .chars()
        .fold((String::new(), false), |(mut output, pending_separator), character| {
            if character.is_ascii_alphanumeric() {
                if pending_separator && !output.is_empty() {
                    output.push('-');
                }
                output.push(character.to_ascii_lowercase());
                (output, false)
            } else {
                (output, true)
            }
        })
        .0;
    let slug = if slug.is_empty() { "convoy".to_string() } else { slug };
    const MAX_CONVOY_NAME_LEN: usize = 63;
    if slug.len() <= MAX_CONVOY_NAME_LEN {
        return slug;
    }
    let digest = format!("{:x}", Sha256::digest(slug.as_bytes()));
    let suffix = &digest[..8];
    let max_base_len = MAX_CONVOY_NAME_LEN - suffix.len() - 1;
    let base = slug.chars().take(max_base_len).collect::<String>().trim_matches('-').to_string();
    format!("{base}-{suffix}")
}

fn convoy_issues_fallback_slug(issues: &[ConvoyIssue], project_display_name: &str, project_ref: &str) -> String {
    match issues {
        [] => convoy_fallback_slug(project_display_name, project_ref),
        [issue] => convoy_fallback_slug(&issue.snapshot.title, &issue.reference.id),
        issues => {
            let issue_ids = issues.iter().map(|issue| issue.reference.id.as_str()).collect::<Vec<_>>().join("-");
            convoy_fallback_slug("batch-issues", &issue_ids)
        }
    }
}

fn convoy_issue_name_context(issue: &ConvoyIssue) -> String {
    format!("Issue {}: {}\n{}", issue.reference.id, issue.snapshot.title, issue.snapshot.body.as_deref().unwrap_or_default())
}

fn validate_convoy_name(name: &str) -> Result<(), String> {
    if name.len() > 63
        || !name.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !name.bytes().last().is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("convoy name `{name}` must be a lowercase DNS label of at most 63 characters"));
    }
    Ok(())
}

fn validate_convoy_branch(branch: &str) -> Result<(), String> {
    let invalid_character =
        branch.bytes().any(|byte| byte <= b' ' || byte == 0x7f || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'));
    let invalid_component =
        branch.split('/').any(|component| component.is_empty() || component.starts_with('.') || component.ends_with(".lock"));
    if branch.len() > 1024
        || branch == "@"
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || invalid_character
        || invalid_component
    {
        return Err(format!("branch `{branch}` is not a valid git branch name"));
    }
    Ok(())
}

impl InProcessDaemon {
    async fn resolve_convoy_issue(
        &self,
        namespace: &str,
        project: &ResourceObject<Project>,
        selector: &flotilla_protocol::IssueSelector,
    ) -> Result<ConvoyIssue, String> {
        let sources =
            match resolve_project_issue_sources(&self.resource_backend.including_replicas::<Repository>(namespace), &project.spec).await {
                IssueSourceResolution::Available { sources } => sources,
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::RepositoryUnavailable { repository, message }) => {
                    return Err(format!("repository {repository}: {message}"));
                }
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource) => {
                    return Err(format!("project {} has no issue source", project.metadata.name));
                }
            };
        let issue = match selector {
            flotilla_protocol::IssueSelector::Reference(reference) => {
                if !sources.contains(&reference.source) {
                    return Err(format!(
                        "issue source {} {} is not part of project {}",
                        reference.source.service, reference.source.scope, project.metadata.name
                    ));
                }
                self.resolve_convoy_issue_snapshot(reference).await?
            }
            flotilla_protocol::IssueSelector::Id(id) => {
                let mut matches = Vec::new();
                let mut failures = Vec::new();
                for source in sources {
                    let reference = flotilla_protocol::IssueRef { source, id: id.clone() };
                    match self.resolve_convoy_issue_snapshot(&reference).await {
                        Ok(issue) => matches.push(issue),
                        Err(error) => failures.push(error),
                    }
                }
                match matches.len() {
                    1 => matches.remove(0),
                    0 => return Err(format!("issue {id} was not found for project {}: {}", project.metadata.name, failures.join("; "))),
                    count => return Err(format!("issue {id} is ambiguous across {count} project issue sources")),
                }
            }
        };

        let repositories = self.resource_backend.including_replicas::<Repository>(namespace);
        let mut matching_repositories = Vec::new();
        for project_repository in &project.spec.repositories {
            let repository = repositories
                .get(&project_repository.repo.to_string())
                .await
                .map_err(|error| format!("repository {}: {error}", project_repository.repo))?;
            if repository.object.spec.forge().is_some_and(|forge| {
                forge.service_url == issue.reference.source.service && forge.repository == issue.reference.source.scope
            }) {
                matching_repositories.push(project_repository.repo.clone());
            }
        }
        let repository_ref = match matching_repositories.as_slice() {
            [repository] => Some(repository.clone()),
            [] if project.spec.repositories.len() == 1 => Some(project.spec.repositories[0].repo.clone()),
            _ => None,
        };

        Ok(ConvoyIssue {
            reference: issue.reference,
            repository_ref,
            snapshot: IssueSnapshot {
                title: issue.title,
                body: issue.body,
                state: issue.state,
                labels: issue.labels,
                as_of: issue.observed_at.expect("admission only accepts observed issue snapshots"),
            },
        })
    }

    async fn prepare_convoy_admission(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<ConvoyAdmission, String> {
        self.prepare_convoy_admission_with_preferences(namespace, intent, dispatching_principal_ref, None, None).await
    }

    async fn resolve_convoy_admission_workflow(
        &self,
        namespace: &str,
        project_ref: &str,
        project: &ProjectSpec,
        repositories: &[ConvoyRepositorySpec],
        intent: &flotilla_protocol::ConvoyStartIntent,
        stance: Option<flotilla_resources::Stance>,
    ) -> Result<(String, WorkflowTemplateSpec), String> {
        let workflow_ref = match intent.workflow_ref.as_deref() {
            Some(workflow_ref) => required_admission_value(workflow_ref, "workflow")?.to_string(),
            None if intent.change_request.is_some() => "single-agent-shepherd".to_string(),
            None => project.default_workflow_ref.clone(),
        };
        let mut workflow = self
            .resource_backend
            .clone()
            .including_replicas::<WorkflowTemplate>(namespace)
            .get(&workflow_ref)
            .await
            .map(|source| source.object)
            .map_err(|error| format!("workflow template {workflow_ref}: {error}"))?;
        if let Some(stance) = stance {
            for vessel in &mut workflow.spec.vessels {
                vessel.stance = stance;
            }
        }
        apply_agent_overrides(&mut workflow.spec, &intent.agent_overrides)?;
        validate_fork_workflow_admission(&self.resource_backend, namespace, repositories, &workflow_ref, &workflow.spec).await?;
        resolve_workflow_credentials(&self.resource_backend, namespace, Some(project_ref), repositories, &mut workflow.spec).await?;
        Ok((workflow_ref, workflow.spec))
    }

    async fn prepare_convoy_admission_with_preferences(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
        repositories: Option<&[RepositoryKey]>,
        stance: Option<flotilla_resources::Stance>,
    ) -> Result<ConvoyAdmission, String> {
        let project_ref = required_admission_value(&intent.project_ref, "project")?;
        let project = self
            .resource_backend
            .clone()
            .including_replicas::<Project>(namespace)
            .get(project_ref)
            .await
            .map(|project| project.object)
            .map_err(|error| project_not_ready_error(namespace, project_ref, error))?;
        let mut repositories_snapshot = self.snapshot_project_repositories(namespace, project_ref).await?;
        if let Some(selected) = repositories {
            let available = repositories_snapshot.iter().map(|repository| &repository.repo_ref).collect::<BTreeSet<_>>();
            if let Some(missing) = selected.iter().find(|repository| !available.contains(repository)) {
                return Err(format!("standing convoy selects repository {missing} outside project {project_ref}"));
            }
            repositories_snapshot.retain(|repository| selected.contains(&repository.repo_ref));
            if repositories_snapshot.is_empty() {
                return Err("standing convoy must select at least one project repository".to_string());
            }
        }
        if intent.change_request.is_some() && intent.branch.is_some() {
            return Err("change request adoption derives the branch from --pr; do not also provide a branch".to_string());
        }
        if intent.change_request.is_some() && !intent.issues.is_empty() {
            return Err("change request adoption is PR-first; do not also provide issues".to_string());
        }
        let change_request = match intent.change_request.as_deref() {
            Some(id) => {
                let id = required_admission_value(id, "change request")?;
                let resolved = self
                    .resolve_convoy_change_request_admission(
                        &repositories_snapshot.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>(),
                        id,
                    )
                    .await?;
                let repository = repositories_snapshot
                    .iter_mut()
                    .find(|repository| repository.repo_ref == resolved.binding.repository_ref)
                    .expect("admission resolution only returns project repositories");
                repository.source_ref = resolved.base_ref.clone();
                repository.target_ref = resolved.base_ref.clone();
                Some(resolved)
            }
            None => None,
        };
        let mut seen_issue_selectors = HashSet::new();
        let mut issues = Vec::with_capacity(intent.issues.len());
        for selector in &intent.issues {
            if seen_issue_selectors.insert(selector.clone()) {
                issues.push(self.resolve_convoy_issue(namespace, &project, selector).await?);
            }
        }
        let (workflow_ref, workflow) =
            self.resolve_convoy_admission_workflow(namespace, project_ref, &project.spec, &repositories_snapshot, intent, stance).await?;

        let fallback_slug = change_request
            .as_ref()
            .map(|change_request| convoy_fallback_slug(&change_request.binding.title, &change_request.binding.id))
            .unwrap_or_else(|| convoy_issues_fallback_slug(&issues, &project.spec.display_name, project_ref));
        let generated = if change_request.is_none() && (intent.name.is_none() || intent.branch.is_none()) {
            let issue_context = (!issues.is_empty()).then(|| issues.iter().map(convoy_issue_name_context).collect::<Vec<_>>().join("\n\n"));
            let context = [
                Some(format!("Project: {}", project.spec.display_name)),
                issue_context,
                intent.instruction.as_ref().map(|instruction| format!("Instruction: {instruction}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            match self.admission_ai_utility().await {
                Some(utility) => utility.generate_convoy_names(&context).await.ok(),
                None => None,
            }
        } else {
            None
        };
        let generated = generated.unwrap_or_else(|| ConvoyNames { name: fallback_slug.clone(), branch: fallback_slug.clone() });
        let role = intent
            .name
            .as_deref()
            .map(|name| required_admission_value(name, "name").map(str::to_string))
            .transpose()?
            .unwrap_or_else(|| convoy_fallback_slug(&generated.name, "").trim_end_matches('-').to_string());
        validate_convoy_name(&role)?;
        let branch = match (change_request.as_ref(), intent.branch.as_deref()) {
            (Some(change_request), None) => change_request.branch.clone(),
            (Some(_), Some(_)) => unreachable!("change request plus branch was rejected"),
            (None, Some(branch)) => required_admission_value(branch, "branch")?.to_string(),
            (None, None) => required_admission_value(&generated.branch, "generated branch")?.to_string(),
        };
        validate_convoy_branch(&branch)?;
        let placement = self.resolve_and_validate_convoy_placement(namespace, &workflow, intent.placement_policy.as_deref()).await?;
        if intent.workflow_ref.is_none()
            && workflow.vessels.iter().any(|vessel| vessel.stance == flotilla_resources::Stance::Trusted)
            && placement.selected.as_ref().is_some_and(|policy| policy.spec.host_direct.is_some())
        {
            let policy = placement.selected.as_ref().expect("trusted host-direct placement was selected");
            let target = placement_target_host(&self.resource_backend, namespace, policy).await?;
            return Err(format!(
                "implicit workflow `{workflow_ref}` resolves to trusted host-direct placement `{}` on `{}`; the crew will inherit ambient human credentials and may act under the operator's forge identity. Repeat with `--workflow {workflow_ref}` to acknowledge this identity implication",
                policy.metadata.name, target.display_name
            ));
        }
        let placement_policy = placement.selected.as_ref().map(|placement| placement.metadata.name.clone());
        let placement_decision = match placement.selected.as_ref() {
            Some(selected) => Some(PlacementDecision {
                policy_name: selected.metadata.name.clone(),
                target_host: placement_target_host(&self.resource_backend, namespace, selected).await?,
                refused_candidates: placement.refused_candidates,
                viable_not_selected: placement.viable_not_selected,
            }),
            None => None,
        };
        let spec = ConvoySpec {
            workflow_ref,
            role,
            generation: 0,
            dispatching_principal_ref: dispatching_principal_ref.clone(),
            inputs: intent.inputs.iter().map(|(key, value)| (key.clone(), InputValue::String(value.clone()))).collect(),
            placement_policy,
            repositories: repositories_snapshot,
            r#ref: Some(branch),
            project_ref: Some(project_ref.to_string()),
            adopted_checkout_refs: BTreeMap::new(),
            issues,
            change_request: change_request.map(|change_request| change_request.binding),
            instruction: intent.instruction.clone(),
        };
        Ok(ConvoyAdmission::builder()
            .name(String::new())
            .spec(spec)
            .workflow(workflow)
            .maybe_placement_policy(placement.selected.map(|placement| placement.spec))
            .maybe_placement_decision(placement_decision)
            .build())
    }

    async fn admit_convoy_start(
        &self,
        namespace: &str,
        intent: &flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: &PrincipalRef,
    ) -> Result<(String, String), String> {
        self.check_local_free_space_floor().await?;
        let mut admission = self.prepare_convoy_admission(namespace, intent, dispatching_principal_ref).await?;
        self.check_remote_placement_free_space_floor(namespace, admission.placement_decision.as_ref()).await?;
        let _admission_guard = self.convoy_admission.lock().await;
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation(&self.resource_backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role)
                .await?;
        self.create_convoy_with_workflow_snapshot(
            namespace,
            &admission.name,
            ConvoySnapshotBundle::builder()
                .spec(&admission.spec)
                .workflow(&admission.workflow)
                .maybe_placement(admission.placement_policy.as_ref())
                .maybe_placement_decision(admission.placement_decision)
                .build(),
            intent.auto_attach.into(),
        )
        .await?;
        let address = convoy_address(&admission.spec.role, admission.spec.project_ref.as_deref());
        Ok((admission.name, address))
    }

    /// Drive one deterministic pass of the standing-convoy ensure loop.
    ///
    /// Tests call this directly with an in-memory backend and virtual clock;
    /// the daemon runtime invokes the same pass on its resync cadence.
    pub async fn reconcile_convoy_ensures_once(&self, namespace: &str) -> Result<Vec<String>, String> {
        self.reconcile_convoy_ensures_once_with_backing_inspector(namespace, self).await
    }

    pub async fn reconcile_convoy_ensures_once_with_backing_inspector(
        &self,
        namespace: &str,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Vec<String>, String> {
        let ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(namespace).list().await.map_err(|e| e.to_string())?;
        let local_projects = self.resource_backend.clone().using::<Project>(namespace);
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        for ensure in ensures {
            if let Some(driver_ref) = &ensure.spec.driver_ref {
                let target = match canonical_placement_host_ref(&self.resource_backend, namespace, driver_ref).await {
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
                if self.canonical_local_host_id().as_ref() != Some(&target.reference) {
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
                match self.reconcile_driver_convoy_ensure(namespace, &ensure, backing_inspector).await {
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
            match self.reconcile_convoy_ensure(namespace, &ensure, backing_inspector).await {
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

    /// Admit a declared-driver ensure from the generation history homed here.
    ///
    /// Driver admission deliberately has no companion control state and never
    /// patches the ensure, even when this root also happens to home its
    /// definition. Failed husks are the retry budget; deleting them is an
    /// operator-authorized reset.
    async fn reconcile_driver_convoy_ensure(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        backing_inspector: &dyn StandingConvoyBackingInspector,
    ) -> Result<Option<String>, String> {
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

        if generations.last().is_some_and(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())) {
            return Ok(None);
        }

        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        let demand_name = format!("{ENSURE_HOLD_ATTENTION_PREFIX}{}", ensure.metadata.name);
        let resolved_escalation = match demands.get(&demand_name).await {
            Ok(demand)
                if demand.status.as_ref().is_none_or(|status| matches!(status.state, DemandState::Raised | DemandState::Escalated)) =>
            {
                return Ok(None);
            }
            Ok(_) => {
                demands.delete(&demand_name).await.map_err(|error| error.to_string())?;
                true
            }
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error.to_string()),
        };

        let consecutive_failures = generations
            .iter()
            .rev()
            .take_while(|convoy| convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Failed))
            .count() as u32;
        let latest = generations.last();
        if !resolved_escalation && consecutive_failures >= ENSURE_MAX_CONSECUTIVE_FAILURES {
            let latest = latest.expect("a positive failure count requires a generation");
            let failure = format!("ensured convoy entered a terminal failure phase; {consecutive_failures} consecutive generations failed");
            self.raise_ensure_attention(ensure, latest, &failure, Some(self.clock.now() + ENSURE_ESCALATION_AFTER)).await?;
            return Ok(Some(format!("ConvoyEnsure/{} exhausted restart budget", ensure.metadata.name)));
        }
        if !resolved_escalation && consecutive_failures > 0 {
            let latest = latest.expect("a positive failure count requires a generation");
            backing_inspector.verify_backing_dead(latest).await?;
            let retry_at = latest.metadata.creation_timestamp + ensure_retry_delay(consecutive_failures - 1);
            if retry_at > self.clock.now() {
                return Ok(None);
            }
        }

        self.start_ensured_convoy(namespace, ensure).await?;
        Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
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
            status.observed_config_hash = Some(config_hash);
            if changed {
                self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
                status.restart_count = 0;
                status.retry_at = None;
                status.last_failure = None;
                status.hold_reason = None;
            }
        }
        let convoy = match status.convoy_ref.as_deref() {
            Some(convoy_ref) => match convoys.get(convoy_ref).await {
                Ok(convoy) if convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name) => Some(convoy),
                Ok(_) => return Err(format!("convoy {convoy_ref} exists without this ensure's provenance")),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            },
            None => convoys
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|convoy| {
                    convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name)
                        && convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                })
                .max_by_key(|convoy| convoy.spec.generation),
        };
        let terminal = convoy
            .as_ref()
            .and_then(|convoy| convoy.status.as_ref())
            .is_some_and(|status| matches!(status.phase, ConvoyPhase::Failed | ConvoyPhase::Cancelled | ConvoyPhase::Abandoned));

        if let Some(convoy) = convoy.as_ref().filter(|_| !terminal) {
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

        if convoy.is_none() {
            if status.retry_at.is_some_and(|retry_at| retry_at > now) {
                return Ok(None);
            }
            return self.restart_absent_ensured_convoy(namespace, ensure, &status, now).await;
        }

        let convoy = convoy.expect("terminal branch requires an existing convoy");
        if status.hold_reason == Some(ConvoyEnsureHoldReason::RestartLimit) {
            if self.ensure_attention_is_active(namespace, &ensure.metadata.name).await? {
                return Ok(None);
            }
            self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
            self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::ResetBackoff).await?;
            return Ok(Some(format!("ConvoyEnsure/{} restart hold cleared", ensure.metadata.name)));
        }
        let operator_forced = convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Abandoned);
        if !operator_forced {
            if let Err(reason) = backing_inspector.verify_backing_dead(&convoy).await {
                let failure = format!("standing convoy teardown held: {reason}");
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

        if status.retry_at.is_none() {
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
        if status.retry_at.is_some_and(|retry_at| retry_at > now) {
            return Ok(None);
        }

        let restart = async {
            // The terminal generation remains as history. A successful
            // restart admits the next generation under a fresh record name.
            self.start_ensured_convoy(namespace, ensure).await
        }
        .await;
        match restart {
            Ok(convoy_ref) => {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref: convoy_ref.clone(),
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                let retry_at = now + ensure_retry_delay(status.restart_count);
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("start failed; retry at {retry_at}: {error}"))
            }
        }
    }

    async fn restart_absent_ensured_convoy(
        &self,
        namespace: &str,
        ensure: &ResourceObject<ConvoyEnsure>,
        status: &flotilla_resources::ConvoyEnsureStatus,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, String> {
        self.clear_ensure_attention(namespace, &ensure.metadata.name).await?;
        match self.start_ensured_convoy(namespace, ensure).await {
            Ok(convoy_ref) => {
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Running {
                    convoy_ref,
                    observed_at: now,
                })
                .await?;
                Ok(Some(format!("started {}@{}", ensure.spec.role, ensure.spec.project_ref)))
            }
            Err(error) => {
                let retry_at = now + ensure_retry_delay(status.restart_count);
                self.patch_convoy_ensure(namespace, &ensure.metadata.name, ConvoyEnsureStatusPatch::Retrying {
                    retry_at,
                    failure: error.clone(),
                })
                .await?;
                Err(format!("start failed; retry at {retry_at}: {error}"))
            }
        }
    }

    async fn verify_standing_convoy_resource_backing_dead(&self, convoy: &ResourceObject<ResourceConvoy>) -> Result<(), String> {
        let environments = self
            .resource_backend
            .using::<ResourceEnvironment>(&convoy.metadata.namespace)
            .list()
            .await
            .map_err(|error| format!("could not inspect backing environments: {error}"))?
            .items
            .into_iter()
            .filter(|environment| environment.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name))
            .collect::<Vec<_>>();
        if environments.is_empty() {
            return Err("no backing environment evidence is available".to_string());
        }
        let not_dead = environments
            .iter()
            .filter(|environment| environment.status.as_ref().map(|status| status.phase) != Some(EnvironmentPhase::Failed))
            .map(|environment| {
                let phase = environment.status.as_ref().map(|status| status.phase).unwrap_or(EnvironmentPhase::Pending);
                format!("Environment/{} is {phase:?}", environment.metadata.name)
            })
            .collect::<Vec<_>>();
        if not_dead.is_empty() {
            Ok(())
        } else {
            Err(format!("backing is not verified dead: {}", not_dead.join(", ")))
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

    async fn start_ensured_convoy(&self, namespace: &str, ensure: &ResourceObject<ConvoyEnsure>) -> Result<String, String> {
        let intent = flotilla_protocol::ConvoyStartIntent::builder()
            .namespace(namespace.to_string())
            .project_ref(ensure.spec.project_ref.clone())
            .name(ensure.spec.role.clone())
            .branch(ensure.spec.role.clone())
            .workflow_ref(ensure.spec.workflow_ref.clone())
            .maybe_placement_policy(ensure.spec.placement_policy.clone())
            .auto_attach(flotilla_protocol::ConvoyAutoAttach::Never)
            .build();
        let mut admission = self
            .prepare_convoy_admission_with_preferences(
                namespace,
                &intent,
                &PrincipalRef::implicit_for_namespace(namespace),
                Some(&ensure.spec.repositories),
                ensure.spec.stance,
            )
            .await?;
        self.check_local_free_space_floor().await?;
        self.check_remote_placement_free_space_floor(namespace, admission.placement_decision.as_ref()).await?;
        if admission.workflow.exit.is_some() {
            return Err(format!(
                "workflow template {} declares an exit table; standing convoys require no exit declaration",
                ensure.spec.workflow_ref
            ));
        }
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
        let _admission_guard = self.convoy_admission.lock().await;
        let existing = self
            .resource_backend
            .clone()
            .using::<ResourceConvoy>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|convoy| convoy.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .collect::<Vec<_>>();
        if let Some(existing) = existing
            .iter()
            .filter(|convoy| convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION) == Some(&ensure.metadata.name))
            .max_by_key(|convoy| convoy.spec.generation)
        {
            return Ok(existing.metadata.name.clone());
        }
        if existing
            .iter()
            .any(|convoy| convoy.spec.project_ref.as_deref() == Some(&ensure.spec.project_ref) && convoy.spec.role == ensure.spec.role)
        {
            return Err(format!(
                "live convoy {} already exists outside this ensure",
                convoy_address(&ensure.spec.role, Some(&ensure.spec.project_ref))
            ));
        }
        admission.name = convoy_record_name();
        admission.spec.generation =
            allocate_convoy_generation(&self.resource_backend, namespace, admission.spec.project_ref.as_deref(), &admission.spec.role)
                .await?;
        let workflow_value = serde_json::to_value(&admission.workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.resource_backend, namespace, &workflow_name, &admission.workflow).await?;
        annotations.insert(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name);
        if let Some(placement) = &admission.placement_policy {
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = prepared_snapshot_name("placement", &placement_value)?;
            ensure_prepared_placement_snapshot(&self.resource_backend, namespace, &placement_name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        self.create_convoy_with_annotations(
            namespace,
            &admission.name,
            &admission.spec,
            admission.placement_decision,
            ConvoyDispatchRegard::Suppress,
            annotations,
        )
        .await?;
        Ok(admission.name)
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
        self.reap_convoy_internal(namespace, convoy_name, force).await
    }

    async fn reap_convoy_internal(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        self.verify_convoy_teardown_gate(namespace, name, force).await?;
        self.cascade_convoy_children(namespace, name).await?;
        self.resource_backend.clone().using::<ResourceConvoy>(namespace).delete(name).await.map_err(|error| error.to_string())
    }

    async fn check_local_free_space_floor(&self) -> Result<(), String> {
        let config = Arc::clone(&self.config);
        let available_space_probe = Arc::clone(&self.discovery.available_space_probe);
        let admission_free_space_path = self.admission_free_space_path.read().expect("admission free-space path lock poisoned").clone();
        let host_name = self.host_name.to_string();
        tokio::task::spawn_blocking(move || {
            let daemon_config = config.load_daemon_config()?;
            crate::admission::check_free_space_floor(
                &*available_space_probe,
                &host_name,
                &admission_free_space_path,
                daemon_config.admission.free_space_floor_gib,
            )
        })
        .await
        .map_err(|error| format!("free-space check failed on host `{}`: {error}", self.host_name))?
    }

    pub fn admission_free_space_floor_bytes(&self) -> Result<u64, String> {
        let floor_gib = self.config.load_daemon_config()?.admission.free_space_floor_gib;
        crate::admission::free_space_floor_bytes(floor_gib)
    }

    async fn check_remote_placement_free_space_floor(&self, namespace: &str, placement: Option<&PlacementDecision>) -> Result<(), String> {
        let Some(placement) = placement else {
            return Ok(());
        };
        let target_host = &placement.target_host;

        let sources =
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        let matching_sources =
            sources.items.into_iter().filter(|source| source.object.metadata.name == target_host.reference.as_str()).collect::<Vec<_>>();
        let has_replica = matching_sources.iter().any(|source| matches!(source.provenance, ResourceProvenance::Replica { .. }));
        let is_host_targeted_placement = self
            .resource_backend
            .clone()
            .including_replicas::<PlacementPolicy>(namespace)
            .get(&placement.policy_name)
            .await
            .is_ok_and(|source| placement_host_ref(&source.object).is_some());
        if !has_replica && !is_host_targeted_placement {
            return Ok(());
        }

        let owns_target_identity = self.canonical_local_host_id().as_ref().is_some_and(|host_id| host_id == &target_host.reference);
        let capacity = if owns_target_identity {
            matching_sources
                .iter()
                .find(|source| matches!(source.provenance, ResourceProvenance::Local))
                .and_then(|source| source.object.status.as_ref())
                .and_then(|status| status.admission_free_space_floor_bytes.map(|floor| (floor, status.disk_free_bytes)))
        } else {
            matching_sources
                .into_iter()
                .filter_map(|source| source.object.status)
                .find_map(|status| status.admission_free_space_floor_bytes.map(|floor| (floor, status.disk_free_bytes)))
        };
        check_placement_capacity(target_host, capacity)
    }

    async fn create_convoy_with_workflow_snapshot(
        &self,
        namespace: &str,
        name: &str,
        bundle: ConvoySnapshotBundle<'_>,
        dispatch_regard: ConvoyDispatchRegard,
    ) -> Result<(), String> {
        let ConvoySnapshotBundle { spec, workflow, placement, placement_decision } = bundle;
        let workflow_value = serde_json::to_value(workflow).map_err(|error| error.to_string())?;
        let workflow_name = prepared_snapshot_name("workflow", &workflow_value)?;
        ensure_prepared_workflow_snapshot(&self.resource_backend, namespace, &workflow_name, workflow).await?;
        let mut annotations = BTreeMap::from([(flotilla_resources::WORKFLOW_SNAPSHOT_ANNOTATION.to_string(), workflow_name)]);
        if let Some(placement) = placement {
            let placement_value = serde_json::to_value(placement).map_err(|error| error.to_string())?;
            let placement_name = prepared_snapshot_name("placement", &placement_value)?;
            ensure_prepared_placement_snapshot(&self.resource_backend, namespace, &placement_name, placement).await?;
            annotations.insert(flotilla_resources::PLACEMENT_SNAPSHOT_ANNOTATION.to_string(), placement_name);
        }
        self.create_convoy_with_annotations(namespace, name, spec, placement_decision, dispatch_regard, annotations).await
    }

    async fn create_convoy_with_annotations(
        &self,
        namespace: &str,
        name: &str,
        spec: &ConvoySpec,
        placement_decision: Option<PlacementDecision>,
        dispatch_regard: ConvoyDispatchRegard,
        annotations: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let labels = BTreeMap::from([
            (PROJECT_LABEL.to_string(), spec.project_ref.clone().unwrap_or_default()),
            (ROLE_LABEL.to_string(), spec.role.clone()),
            (GENERATION_LABEL.to_string(), spec.generation.to_string()),
        ]);
        convoys
            .create(&InputMeta::builder().name(name.to_string()).labels(labels).annotations(annotations).build(), spec)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(placement_decision) = placement_decision {
            apply_resource_status_patch(&convoys, name, &ConvoyStatusPatch::SetPlacementDecision { placement_decision })
                .await
                .map_err(|error| error.to_string())?;
        }
        if dispatch_regard == ConvoyDispatchRegard::Emit {
            if let Err(error) = self.emit_implicit_convoy_regard(namespace, name, &spec.dispatching_principal_ref).await {
                warn!(%error, %namespace, %name, "failed to emit convoy dispatch regard");
            }
        }
        Ok(())
    }

    async fn emit_implicit_convoy_regard(&self, namespace: &str, name: &str, principal_ref: &PrincipalRef) -> Result<(), String> {
        let target = ResourceRef::new(api_version(ResourceConvoy::API_PATHS), ResourceConvoy::API_PATHS.kind, namespace, name);
        self.regard_lifecycle.emit_implicit(principal_ref, &target, "convoy-dispatch").await
    }

    async fn emit_attach_regard(&self, binding: &AttachBinding, surface_id: uuid::Uuid) -> Result<(), String> {
        let target = binding.resource_ref().ok_or_else(|| "resolved attach target has no resource identity".to_string())?;
        match self.regard_lifecycle.emit_expressed_for_surface(surface_id, &target).await? {
            SurfaceGestureOutcome::Handled => Ok(()),
            SurfaceGestureOutcome::UnknownSurface => {
                self.regard_lifecycle.emit_expressed(&PrincipalRef::implicit_for_namespace(&binding.namespace), &target).await
            }
        }
    }

    async fn resolve_and_validate_convoy_placement(
        &self,
        namespace: &str,
        workflow: &WorkflowTemplateSpec,
        placement_policy: Option<&str>,
    ) -> Result<PlacementResolution, String> {
        let contained = workflow.vessels.iter().any(|vessel| vessel.stance == flotilla_resources::Stance::Contained);
        let placement = match placement_policy {
            Some(policy) => {
                let policy = required_admission_value(policy, "placement policy")?;
                let resolved = self
                    .resource_backend
                    .clone()
                    .including_replicas::<PlacementPolicy>(namespace)
                    .get(policy)
                    .await
                    .map(|source| source.object)
                    .map_err(|error| format!("placement policy {policy}: {error}"))?;
                if contained && resolved.spec.docker_per_vessel.is_none() {
                    return Err(format!("contained workflow requires a docker placement policy, but {policy} is not contained"));
                }
                PlacementResolution { selected: Some(resolved), refused_candidates: Vec::new(), viable_not_selected: Vec::new() }
            }
            None => {
                let local_host_id = self.canonical_local_host_id();
                let placement =
                    default_convoy_placement_policy(&self.resource_backend, namespace, workflow, local_host_id.as_ref()).await?;
                if contained && placement.selected.is_none() {
                    return Err("contained workflow requires an available docker placement policy".to_string());
                }
                placement
            }
        };
        validate_workflow_agent_adapters(&self.resource_backend, namespace, workflow, placement.selected.as_ref()).await?;
        validate_workflow_credentials(&self.resource_backend, namespace, workflow, placement.selected.as_ref()).await?;
        Ok(placement)
    }

    async fn run_convoy_start(
        &self,
        intent: flotilla_protocol::ConvoyStartIntent,
        dispatching_principal_ref: PrincipalRef,
    ) -> flotilla_protocol::CommandValue {
        let namespace = self.provisioning_namespace().await;
        let requested_namespace = intent.namespace.as_deref().unwrap_or(&namespace);
        if requested_namespace != namespace {
            return flotilla_protocol::CommandValue::Error {
                message: format!("namespace `{requested_namespace}` is not served by this daemon (configured namespace: `{namespace}`)"),
            };
        }
        let auto_attach = self.should_auto_attach(intent.auto_attach);
        match self.admit_convoy_start(&namespace, &intent, &dispatching_principal_ref).await {
            Ok((record_name, address)) if auto_attach => match self.wait_for_convoy_attach(&namespace, &record_name, &address).await {
                Ok(resolved) => flotilla_protocol::CommandValue::ConvoyStarted {
                    name: address,
                    attach_plan: Some(resolved.plan),
                    binding: resolved.binding,
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            },
            Ok((_record_name, address)) => {
                flotilla_protocol::CommandValue::ConvoyStarted { name: address, attach_plan: None, binding: None }
            }
            Err(message) => flotilla_protocol::CommandValue::Error { message },
        }
    }

    async fn supervise_convoy_start(&self, task: ConvoyStartTask) {
        let ConvoyStartTask { command_id, intent, key, dispatching_principal_ref } = task;
        let result = match AssertUnwindSafe(self.run_convoy_start(intent, dispatching_principal_ref)).catch_unwind().await {
            Ok(result) => result,
            Err(_) => {
                warn!(command_id, "convoy start worker panicked");
                flotilla_protocol::CommandValue::Error { message: "convoy start worker panicked".to_string() }
            }
        };
        self.pending_convoy_starts.lock().await.remove(&key);
        self.finish_context_free_command(command_id, empty_repo_identity(), result);
    }

    async fn wait_for_convoy_attach(&self, namespace: &str, name: &str, address: &str) -> Result<ResolvedAttach, String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let listed = convoys.list().await.map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        if let Some(message) = listed.items.iter().find(|convoy| convoy.metadata.name == name).and_then(convoy_start_failure) {
            return Err(message);
        }
        let mut watch = convoys
            .watch(WatchStart::resuming_from(&listed))
            .await
            .map_err(|error| format!("watch convoy {address} while waiting to attach: {error}"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut retry = tokio::time::interval(Duration::from_millis(100));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_attach_error = "attach target is not available yet".to_string();

        loop {
            tokio::select! {
                _ = retry.tick() => {
                    match self.resolve_attach_command_internal(name).await {
                        Ok(resolved) => return Ok(resolved),
                        Err(message) => last_attach_error = message,
                    }
                }
                event = watch.next() => {
                    match event {
                        Some(Ok(WatchEvent::Added(convoy) | WatchEvent::Modified(convoy))) if convoy.metadata.name == name => {
                            if let Some(message) = convoy_start_failure(&convoy) {
                                return Err(message);
                            }
                        }
                        Some(Ok(WatchEvent::Deleted(convoy))) if convoy.metadata.name == name => {
                            return Err(format!("convoy {address} was deleted while waiting for a crew session"));
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => return Err(format!("watch convoy {address} while waiting to attach: {error}")),
                        None => return Err(format!("convoy {address} status watch ended while waiting to attach")),
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(format!("convoy {address} was created but no crew session became attachable: {last_attach_error}"));
                }
            }
        }
    }

    async fn snapshot_project_repositories(&self, namespace: &str, project_ref: &str) -> Result<Vec<ConvoyRepositorySpec>, String> {
        let project = self
            .resource_backend
            .clone()
            .including_replicas::<Project>(namespace)
            .get(project_ref)
            .await
            .map(|project| project.object)
            .map_err(|error| project_not_ready_error(namespace, project_ref, error))?;
        let repositories = self.resource_backend.including_replicas::<Repository>(namespace);
        let mut unresolved = Vec::new();
        let mut snapshots = BTreeMap::<RepositoryKey, (String, RepositorySpec, Option<String>, BTreeSet<String>)>::new();
        for entry in &project.spec.repositories {
            if !entry.roles.is_empty() && !entry.roles.contains(&ProjectRepositoryRole::Code) {
                continue;
            }
            match repositories.get(&entry.repo.to_string()).await {
                Ok(repository) => {
                    let repository = repository.object;
                    if let Err(error) = repository.spec.verify_key(&entry.repo) {
                        unresolved.push(error);
                        continue;
                    }
                    let url = match repository.spec.identity() {
                        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
                        flotilla_resources::RepositoryIdentity::Local { .. } => {
                            unresolved.push(format!("repository {} has no transport remote", entry.repo));
                            continue;
                        }
                    };
                    let default_ref = entry.default_branch.clone().or_else(|| repository.status.as_ref()?.default_branch.clone());
                    let snapshot = snapshots
                        .entry(entry.repo.clone())
                        .or_insert_with(|| (url, repository.spec.clone(), default_ref.clone(), BTreeSet::new()));
                    if snapshot.2 != default_ref {
                        unresolved.push(format!("repository {} has conflicting project default branches", entry.repo));
                    }
                    if let Some(subpath) = &entry.subpath {
                        snapshot.3.insert(subpath.clone());
                    }
                }
                Err(error) => unresolved.push(format!("repository {}: {error}", entry.repo)),
            }
        }
        for (repo_ref, (_, _, default_ref, _)) in &snapshots {
            if default_ref.is_none() {
                unresolved.push(format!("repository {repo_ref} has no resolved default branch"));
            }
        }
        if !unresolved.is_empty() {
            return Err(format!("project {project_ref} is not ready: {}", unresolved.join("; ")));
        }

        let workspace_slugs = flotilla_resources::repository_workspace_slugs(snapshots.iter().map(|(key, (_, spec, _, _))| (key, spec)));
        let mut repositories = snapshots
            .into_iter()
            .map(|(repo_ref, (url, _, default_ref, subpaths))| {
                let default_ref = default_ref.expect("missing default refs were rejected");
                ConvoyRepositorySpec {
                    url,
                    workspace_slug: workspace_slugs[&repo_ref].clone(),
                    repo_ref,
                    source_ref: default_ref.clone(),
                    target_ref: default_ref,
                    subpaths: subpaths.into_iter().collect(),
                }
            })
            .collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.workspace_slug.cmp(&right.workspace_slug).then_with(|| left.repo_ref.cmp(&right.repo_ref)));
        Ok(repositories)
    }

    async fn project_register(&self, target: &str) -> Result<(String, usize), String> {
        let namespace = self.provisioning_namespace().await;
        let path = if Path::new(target).exists() {
            PathBuf::from(target)
        } else {
            let matches = self
                .resource_backend
                .clone()
                .using::<Repository>(&namespace)
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|repository| repository_matches_target(repository, target))
                .collect::<Vec<_>>();
            let [repository] = matches.as_slice() else {
                return Err(match matches.len() {
                    0 => format!("`{target}` is neither a bootstrap repository path nor a repository catalog slug"),
                    _ => format!("bootstrap repository slug `{target}` is ambiguous"),
                });
            };
            let key = RepositoryKey(repository.metadata.name.clone());
            let mut paths = self
                .repository_keys_by_path
                .read()
                .await
                .iter()
                .filter(|(_, candidate)| *candidate == &key)
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            paths.sort();
            match paths.as_slice() {
                [] => return Err(format!("bootstrap repository `{target}` has no local checkout on this host")),
                [path] => path.clone(),
                _ => {
                    let inspector = self.repository_inspector().await?;
                    let mut main_paths = Vec::new();
                    for path in paths {
                        if inspector.inspect_path(&path, None).await?.checkout.is_main {
                            main_paths.push(path);
                        }
                    }
                    match main_paths.as_slice() {
                        [path] => path.clone(),
                        _ => {
                            return Err(format!(
                                "bootstrap repository `{target}` has multiple local checkouts; pass the intended checkout path explicitly"
                            ));
                        }
                    }
                }
            }
        };
        let inspection = self.repository_inspector().await?.inspect_project_declaration(&path).await?;
        let declaration = parse_project_declaration(&inspection.yaml)?;
        let name = declaration.name.clone();
        self.materialize_project_declaration(declaration, inspection).await?;
        let project = self.resource_backend.clone().using::<Project>(&namespace).get(&name).await.map_err(|error| match error {
            ResourceError::NotFound { .. } => format!("project {name} is homed by another root; register it at its home"),
            error => error.to_string(),
        })?;
        Ok((name, project.spec.repositories.len()))
    }

    async fn project_refresh(&self, name: &str) -> Result<(usize, bool, Vec<String>), String> {
        validate_project_name(name)?;
        let namespace = self.provisioning_namespace().await;
        let project =
            self.resource_backend.clone().definitions::<Project>(&namespace).get(name).await.map_err(|error| error.to_string())?;
        let bootstrap_path = project
            .metadata
            .annotations
            .get(BOOTSTRAP_PATH_ANNOTATION)
            .ok_or_else(|| format!("project {name} was registered without a declaration"))?;
        let inspection = self.repository_inspector().await?.inspect_project_declaration(Path::new(bootstrap_path)).await?;
        let declaration = parse_project_declaration(&inspection.yaml)?;
        if declaration.name != name {
            return Err(format!(
                "{} now declares project `{}` instead of `{name}`",
                Path::new(bootstrap_path).join(DECLARATION_FILE).display(),
                declaration.name
            ));
        }
        let changes = self.materialize_project_declaration(declaration, inspection).await?;
        let members = self
            .resource_backend
            .clone()
            .definitions::<Project>(&namespace)
            .get(name)
            .await
            .map_err(|error| error.to_string())?
            .spec
            .repositories
            .len();
        Ok((members, !changes.is_empty(), changes))
    }

    async fn materialize_project_declaration(
        &self,
        declaration: ProjectDeclaration,
        inspection: ProjectDeclarationInspection,
    ) -> Result<Vec<String>, String> {
        validate_project_name(&declaration.name)?;
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let existing_project = match projects.get(&declaration.name).await {
            Ok(project) => Some(project),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let locally_homed = match self.resource_backend.clone().using::<Project>(&namespace).get(&declaration.name).await {
            Ok(_) => true,
            Err(ResourceError::NotFound { .. }) => false,
            Err(error) => return Err(error.to_string()),
        };
        if existing_project.is_some() && !locally_homed {
            debug!(project = %declaration.name, "skipping project materialization away from its home");
            return Ok(Vec::new());
        }
        let aliases = existing_project
            .as_ref()
            .into_iter()
            .flat_map(|project| &project.spec.repositories)
            .filter_map(|member| member.alias.as_ref().map(|alias| (alias.clone(), member.repo.clone())))
            .collect::<BTreeMap<_, _>>();
        let bootstrap_key = inspection.repository.key();
        let bootstrap_path = inspection.repository.checkout.path.to_string_lossy().into_owned();
        let bootstrap_inspection = inspection.clone();
        let provenance = BTreeMap::from([
            (BOOTSTRAP_REPOSITORY_ANNOTATION.to_string(), bootstrap_key.to_string()),
            (BOOTSTRAP_COMMIT_ANNOTATION.to_string(), inspection.commit.clone()),
            (DECLARATION_FILE_ANNOTATION.to_string(), DECLARATION_FILE.to_string()),
        ]);
        let mut converged = false;
        let mut members = Vec::with_capacity(declaration.members.len());
        for member in declaration.members {
            let declared_spec = RepositorySpec::remote(member.url)?;
            let key = aliases.get(&member.alias).cloned().unwrap_or_else(|| declared_spec.key());
            let mut repository = if key == declared_spec.key() {
                ensure_repository(&repositories, &key, &declared_spec).await.map_err(|error| error.to_string())?
            } else {
                repositories
                    .get(&key.to_string())
                    .await
                    .map_err(|error| format!("project member alias `{}` refers to unavailable repository {key}: {error}", member.alias))?
            };
            let mut repository_meta = InputMeta::from(&repository.metadata);
            for (annotation, value) in &provenance {
                if repository_meta.annotations.get(annotation) != Some(value) {
                    converged = true;
                    repository_meta.annotations.insert(annotation.clone(), value.clone());
                }
            }
            if repository_meta.annotations != repository.metadata.annotations {
                repository = repositories
                    .update(&repository_meta, &repository.metadata.resource_version, &repository.spec)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            repository
                .spec
                .verify_key(&key)
                .map_err(|error| format!("project member alias `{}` resolved to invalid repository {key}: {error}", member.alias))?;
            members.push(ProjectRepositorySpec {
                repo: key,
                alias: Some(member.alias),
                roles: member.roles,
                subpath: None,
                default_branch: None,
            });
        }
        let spec = normalize_project_spec(ProjectSpec {
            display_name: declaration.name.clone(),
            default_workflow_ref: "single-agent-trusted".to_string(),
            issue_source: None,
            repositories: members,
            dispatch_policy: existing_project.as_ref().and_then(|project| project.spec.dispatch_policy.clone()),
        })?;
        let mut meta = existing_project
            .as_ref()
            .map_or_else(|| InputMeta::builder().name(declaration.name.clone()).build(), |project| InputMeta::from(&project.metadata));
        for (annotation, value) in provenance {
            meta.annotations.insert(annotation, value);
        }
        meta.annotations.insert(BOOTSTRAP_PATH_ANNOTATION.to_string(), bootstrap_path);
        converged |=
            existing_project.as_ref().is_none_or(|project| project.spec != spec || project.metadata.annotations != meta.annotations);
        projects.apply(&meta, &spec).await.map_err(|error| error.to_string())?;
        let mut changes = if converged { vec![format!("Project/{}", declaration.name)] } else { Vec::new() };
        changes.extend(self.materialize_project_operational_entries(&declaration.name, &bootstrap_inspection).await?);
        Ok(changes)
    }

    async fn materialize_project_operational_entries(
        &self,
        project_name: &str,
        bootstrap: &ProjectDeclarationInspection,
    ) -> Result<Vec<String>, String> {
        let namespace = self.provisioning_namespace().await;
        let project =
            self.resource_backend.clone().definitions::<Project>(&namespace).get(project_name).await.map_err(|e| e.to_string())?;
        let aliases = project
            .spec
            .repositories
            .iter()
            .filter_map(|member| member.alias.as_ref().map(|alias| (alias.clone(), member.repo.clone())))
            .collect::<BTreeMap<_, _>>();
        let all_code = project
            .spec
            .repositories
            .iter()
            .filter(|member| member.roles.contains(&ProjectRepositoryRole::Code))
            .map(|member| member.repo.clone())
            .collect::<Vec<_>>();
        let inspector = self.repository_inspector().await?;
        let mut sources = Vec::new();
        let mut unavailable_source = false;
        for member in project.spec.repositories.iter().filter(|member| member.roles.contains(&ProjectRepositoryRole::Ops)) {
            let path = if member.repo == bootstrap.repository.key() {
                bootstrap.repository.checkout.path.clone()
            } else {
                let mut paths = self
                    .repository_keys_by_path
                    .read()
                    .await
                    .iter()
                    .filter(|(_, key)| *key == &member.repo)
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                paths.sort();
                match paths.as_slice() {
                    [] => {
                        unavailable_source = true;
                        continue;
                    }
                    [path] => path.clone(),
                    _ => {
                        let mut main_paths = Vec::new();
                        for path in paths {
                            if inspector.inspect_path(&path, None).await?.checkout.is_main {
                                main_paths.push(path);
                            }
                        }
                        match main_paths.as_slice() {
                            [path] => path.clone(),
                            _ => {
                                return Err(format!(
                                    "ops member {} has multiple local checkouts; its main checkout cannot be selected unambiguously",
                                    member.alias.as_deref().unwrap_or(&member.repo.0)
                                ));
                            }
                        }
                    }
                }
            };
            let mut source = inspector.inspect_operational_entries(&path).await?;
            if member.repo == bootstrap.repository.key() {
                source.commit.clone_from(&bootstrap.commit);
                source.repository = bootstrap.repository.clone();
            }
            sources.push(source);
        }
        // Registration remains possible from a bootstrap repository that does
        // not host every ops member. Never infer an empty desired set from an
        // unavailable source: that could erase definitions materialized by a
        // previous refresh on a host that had the checkout.
        if unavailable_source {
            return Ok(Vec::new());
        }

        let mut workflows = BTreeMap::new();
        let mut ensures = BTreeMap::new();
        let mut commands = BTreeMap::<RepositoryKey, BTreeMap<String, String>>::new();
        let mut command_provenance = BTreeMap::<RepositoryKey, Vec<serde_json::Value>>::new();
        for source in sources {
            self.collect_operational_entries(
                project_name,
                &aliases,
                &all_code,
                source,
                &mut workflows,
                &mut ensures,
                &mut commands,
                &mut command_provenance,
            )?;
        }

        let templates = self.resource_backend.clone().using::<WorkflowTemplate>(&namespace);
        let mut changes = Vec::new();
        for (name, (meta, spec)) in &workflows {
            let current = match templates.get(name).await {
                Ok(current) => Some(current),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            if let Some(current) = &current {
                match current.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION) {
                    Some(owner) if owner == project_name => {}
                    Some(owner) => return Err(format!("WorkflowTemplate `{name}` is materialized by project `{owner}`")),
                    None => return Err(format!("WorkflowTemplate `{name}` already exists and is not materialized by a project")),
                }
            }
            if current.as_ref().is_none_or(|current| current.spec != *spec || current.metadata.annotations != meta.annotations) {
                match current {
                    Some(current) => {
                        templates.update(meta, &current.metadata.resource_version, spec).await.map_err(|error| error.to_string())?;
                    }
                    None => {
                        templates.create(meta, spec).await.map_err(|error| error.to_string())?;
                    }
                }
                changes.push(format!("WorkflowTemplate/{name}"));
            }
        }
        for stale in templates.list().await.map_err(|error| error.to_string())?.items.into_iter().filter(|template| {
            template.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !workflows.contains_key(&template.metadata.name)
        }) {
            templates.delete(&stale.metadata.name).await.map_err(|error| error.to_string())?;
            changes.push(format!("deleted WorkflowTemplate/{}", stale.metadata.name));
        }

        let convoy_ensures = self.resource_backend.clone().definitions::<ConvoyEnsure>(&namespace);
        for (name, (meta, spec)) in &ensures {
            let workflow = templates.get(&spec.workflow_ref).await.map_err(|error| {
                format!(
                    "{} ensure `{name}` references workflow template {}: {error}",
                    meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str).unwrap_or("operational entry"),
                    spec.workflow_ref
                )
            })?;
            if workflow.spec.exit.is_some() {
                return Err(format!(
                    "{} ensure `{name}` references workflow template {} with an exit declaration",
                    meta.annotations.get(SOURCE_ENTRY_PATH_ANNOTATION).map(String::as_str).unwrap_or("operational entry"),
                    spec.workflow_ref
                ));
            }
            let current = match convoy_ensures.get(name).await {
                Ok(current) => Some(current),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
            if let Some(current) = &current {
                match current.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION) {
                    Some(owner) if owner == project_name => {}
                    Some(owner) => return Err(format!("ConvoyEnsure `{name}` is materialized by project `{owner}`")),
                    None => return Err(format!("ConvoyEnsure `{name}` already exists and is not materialized by a project")),
                }
            }
            if current.as_ref().is_none_or(|current| current.spec != *spec || current.metadata.annotations != meta.annotations) {
                convoy_ensures.apply(meta, spec).await.map_err(|error| error.to_string())?;
                changes.push(format!("ConvoyEnsure/{name}"));
            }
        }
        for stale in convoy_ensures.list().await.map_err(|error| error.to_string())?.into_iter().filter(|ensure| {
            ensure.metadata.annotations.get(MATERIALIZED_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !ensures.contains_key(&ensure.metadata.name)
        }) {
            if let Some(convoy_ref) = stale.status.as_ref().and_then(|status| status.convoy_ref.as_deref()) {
                self.reap_ensured_convoy(&namespace, &stale.metadata.name, convoy_ref, false).await?;
            }
            convoy_ensures.delete(&stale.metadata.name).await.map_err(|error| error.to_string())?;
            changes.push(format!("deleted ConvoyEnsure/{}", stale.metadata.name));
        }

        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let current_code_members = project
            .spec
            .repositories
            .iter()
            .filter(|member| member.roles.contains(&ProjectRepositoryRole::Code))
            .map(|member| member.repo.clone())
            .collect::<BTreeSet<_>>();
        for member in project.spec.repositories.iter().filter(|member| member.roles.contains(&ProjectRepositoryRole::Code)) {
            let current = repositories.get(&member.repo.to_string()).await.map_err(|error| error.to_string())?;
            let desired_commands = commands.remove(&member.repo).unwrap_or_default();
            let owner = current.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).map(String::as_str);
            match owner {
                Some(owner) if owner == project_name => {}
                Some(owner) if !desired_commands.is_empty() => {
                    return Err(format!("Repository {} verification commands are materialized by project `{owner}`", member.repo));
                }
                None if !desired_commands.is_empty() && !current.spec.verification_commands().is_empty() => {
                    return Err(format!(
                        "Repository {} already has verification commands that are not materialized by a project",
                        member.repo
                    ));
                }
                None if !desired_commands.is_empty() => {}
                _ => continue,
            }
            let desired_spec = current.spec.clone().with_verification_commands(desired_commands);
            let mut meta = InputMeta::from(&current.metadata);
            match command_provenance.remove(&member.repo) {
                Some(provenance) => {
                    meta.annotations.insert(VERIFICATION_PROJECT_ANNOTATION.to_string(), project_name.to_string());
                    meta.annotations.insert(
                        VERIFICATION_PROVENANCE_ANNOTATION.to_string(),
                        serde_json::to_string(&provenance).expect("JSON provenance values serialize"),
                    );
                }
                None => {
                    meta.annotations.remove(VERIFICATION_PROJECT_ANNOTATION);
                    meta.annotations.remove(VERIFICATION_PROVENANCE_ANNOTATION);
                }
            }
            if current.spec != desired_spec || current.metadata.annotations != meta.annotations {
                repositories.update(&meta, &current.metadata.resource_version, &desired_spec).await.map_err(|error| error.to_string())?;
                changes.push(format!("Repository/{} verification commands", member.repo));
            }
        }
        for stale in repositories.list().await.map_err(|error| error.to_string())?.items.into_iter().filter(|repository| {
            repository.metadata.annotations.get(VERIFICATION_PROJECT_ANNOTATION).map(String::as_str) == Some(project_name)
                && !current_code_members.contains(&RepositoryKey(repository.metadata.name.clone()))
        }) {
            let mut meta = InputMeta::from(&stale.metadata);
            meta.annotations.remove(VERIFICATION_PROJECT_ANNOTATION);
            meta.annotations.remove(VERIFICATION_PROVENANCE_ANNOTATION);
            let spec = stale.spec.clone().with_verification_commands(BTreeMap::new());
            repositories.update(&meta, &stale.metadata.resource_version, &spec).await.map_err(|error| error.to_string())?;
            changes.push(format!("Repository/{} verification commands", stale.metadata.name));
        }
        changes.sort();
        Ok(changes)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_operational_entries(
        &self,
        project_name: &str,
        aliases: &BTreeMap<String, RepositoryKey>,
        all_code: &[RepositoryKey],
        source: OperationalEntriesInspection,
        workflows: &mut BTreeMap<String, (InputMeta, WorkflowTemplateSpec)>,
        ensures: &mut BTreeMap<String, (InputMeta, ConvoyEnsureSpec)>,
        commands: &mut BTreeMap<RepositoryKey, BTreeMap<String, String>>,
        command_provenance: &mut BTreeMap<RepositoryKey, Vec<serde_json::Value>>,
    ) -> Result<(), String> {
        let source_repository = source.repository.key();
        for file in source.files {
            let Some(entry) = parse_operational_entry(&file.contents).map_err(|error| format!("{}: {error}", file.path))? else {
                continue;
            };
            let requires_code_role =
                matches!(&entry.definition, OperationalEntryDefinition::VerificationCommand { .. } | OperationalEntryDefinition::Ensure(_));
            let targets = match entry.repos {
                Some(repo_aliases) => repo_aliases
                    .into_iter()
                    .map(|alias| {
                        let target = aliases
                            .get(&alias)
                            .cloned()
                            .ok_or_else(|| format!("{} names unknown repository alias `{alias}`", file.path))?;
                        if requires_code_role && !all_code.contains(&target) {
                            return Err(format!(
                                "{} operational entry `{}` targets repository alias `{alias}` without the code role",
                                file.path, entry.name
                            ));
                        }
                        Ok(target)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                None => all_code.to_vec(),
            };
            if targets.is_empty() {
                return Err(format!("{} has no code-role repositories to target", file.path));
            }
            let provenance = serde_json::json!({
                "sourceRepository": source_repository,
                "sourceCommit": source.commit,
                "entryPath": file.path,
            });
            match entry.definition {
                OperationalEntryDefinition::WorkflowTemplate(mut spec) => {
                    for vessel in &mut spec.vessels {
                        vessel.repository_refs = Some(targets.clone());
                    }
                    flotilla_resources::validate(&spec).map_err(|errors| {
                        let errors = errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
                        format!("{} contains invalid workflow template `{}`: {errors}", file.path, entry.name)
                    })?;
                    let meta = InputMeta::builder()
                        .name(entry.name.clone())
                        .annotations(BTreeMap::from([
                            (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project_name.to_string()),
                            (SOURCE_REPOSITORY_ANNOTATION.to_string(), source_repository.to_string()),
                            (SOURCE_COMMIT_ANNOTATION.to_string(), source.commit.clone()),
                            (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), file.path.clone()),
                        ]))
                        .build();
                    if workflows.insert(entry.name.clone(), (meta, spec)).is_some() {
                        return Err(format!("duplicate materialized WorkflowTemplate `{}`", entry.name));
                    }
                }
                OperationalEntryDefinition::VerificationCommand { command } => {
                    for target in targets {
                        if commands.entry(target.clone()).or_default().insert(entry.name.clone(), command.clone()).is_some() {
                            return Err(format!("duplicate verification command `{}` for repository {target}", entry.name));
                        }
                        command_provenance.entry(target).or_default().push(provenance.clone());
                    }
                }
                OperationalEntryDefinition::Ensure(ensure) => {
                    let role = entry.name;
                    let ensure_name = convoy_ensure_name(project_name, &role);
                    let mut meta = InputMeta::builder()
                        .name(ensure_name.clone())
                        .annotations(BTreeMap::from([
                            (MATERIALIZED_PROJECT_ANNOTATION.to_string(), project_name.to_string()),
                            (SOURCE_REPOSITORY_ANNOTATION.to_string(), source_repository.to_string()),
                            (SOURCE_COMMIT_ANNOTATION.to_string(), source.commit.clone()),
                            (SOURCE_ENTRY_PATH_ANNOTATION.to_string(), file.path.clone()),
                        ]))
                        .build();
                    if let Some(presents_as) = &ensure.presents_as {
                        meta.annotations.insert(PRESENTS_AS_ANNOTATION.to_string(), presents_as.clone());
                    }
                    let spec = ConvoyEnsureSpec {
                        project_ref: project_name.to_string(),
                        role: role.clone(),
                        driver_ref: ensure.driver,
                        workflow_ref: ensure.workflow,
                        placement_policy: ensure.placement,
                        stance: ensure.stance,
                        repositories: targets,
                        presents_as: ensure.presents_as,
                    };
                    if ensures.insert(ensure_name, (meta, spec)).is_some() {
                        return Err(format!("duplicate standing convoy role `{role}` in project `{project_name}`"));
                    }
                }
            }
        }
        Ok(())
    }

    async fn project_add(
        &self,
        target: &str,
        explicit_name: Option<&str>,
        explicit_display_name: Option<&str>,
        remote: Option<&str>,
    ) -> Result<String, String> {
        let namespace = self.provisioning_namespace().await;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let target_path = Path::new(target);
        let target_syntax = project_target_syntax(target);
        let path_is_explicit = target_syntax == ProjectTargetSyntax::ExplicitPath;
        let qualified_slug = target_syntax == ProjectTargetSyntax::QualifiedSlug;
        let path_candidate = if !qualified_slug && target_path.exists() {
            Some(self.repository_inspector().await?.inspect_path(target_path, remote).await?)
        } else if path_is_explicit {
            return Err(format!("repository path {} does not exist", target_path.display()));
        } else {
            None
        };

        let catalog_matches = if path_is_explicit {
            Vec::new()
        } else {
            repositories
                .list()
                .await
                .map_err(|error| error.to_string())?
                .items
                .into_iter()
                .filter(|repository| repository_matches_target(repository, target))
                .collect::<Vec<_>>()
        };
        let mut catalog_by_key = BTreeMap::new();
        for repository in catalog_matches {
            let key = RepositoryKey(repository.metadata.name.clone());
            repository.spec.verify_key(&key)?;
            catalog_by_key.insert(key, repository.spec);
        }
        if catalog_by_key.len() > 1 {
            return Err(format!(
                "repository slug `{target}` is ambiguous: {}",
                catalog_by_key.keys().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ));
        }
        let catalog_candidate = catalog_by_key.into_iter().next();
        if remote.is_some() && path_candidate.is_none() && catalog_candidate.is_some() {
            return Err("--remote can only select identity while inspecting a local repository path".to_string());
        }

        let (key, repository_spec, checkout) = match (path_candidate, catalog_candidate) {
            (Some(inspection), Some((catalog_key, _))) if inspection.key() != catalog_key => {
                return Err(format!(
                    "`{target}` resolves to different path and catalog repositories: {} and {catalog_key}",
                    inspection.key()
                ));
            }
            (Some(inspection), _) => (inspection.key(), inspection.spec, Some(inspection.checkout)),
            (None, Some((key, spec))) => (key, spec, None),
            (None, None) => return Err(format!("`{target}` is neither a repository path nor a repository catalog slug")),
        };

        ensure_repository_and_default_project_workflow(&self.resource_backend, &namespace, &key, &repository_spec).await?;
        if let Some(checkout) = checkout {
            self.reconcile_project_checkouts(&namespace, &key, &repository_spec, checkout).await?;
        }

        let default_name = normalize_project_name(&repository_spec.leaf_slug())?;
        let project_name = explicit_name.map(str::to_string).unwrap_or(default_name.clone());
        validate_project_name(&project_name)?;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        match projects.get(&project_name).await {
            Ok(existing) => {
                if is_declaration_backed_project(&existing) {
                    return Err(format!("project {project_name} is managed by a declaration; use project refresh to update it"));
                }
                if !is_whole_repository_project(&existing.spec, &key) {
                    return Err(format!("project {project_name} already exists with a different repository definition"));
                }
                if explicit_display_name.is_some_and(|display_name| display_name != existing.spec.display_name) {
                    return Err(format!(
                        "project {project_name} already exists with display name `{}`; use project apply to change it",
                        existing.spec.display_name
                    ));
                }
                reconcile_whole_repository_project_definition(&projects, existing).await?;
                return Ok(project_name);
            }
            Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }

        let spec = whole_repository_project_spec(key, explicit_display_name.map(str::to_string).unwrap_or(default_name))?;
        projects.apply(&whole_repository_project_meta(project_name.clone()), &spec).await.map_err(|error| error.to_string())?;
        Ok(project_name)
    }

    async fn reconcile_whole_repository_project(
        &self,
        inspection: &crate::repository_inspection::RepositoryInspection,
    ) -> Result<Option<RepositoryIdentityChange>, String> {
        let namespace = self.provisioning_namespace().await;
        let repository_spec = &inspection.spec;
        let repository_key = repository_spec.key();
        ensure_repository_and_default_project_workflow(&self.resource_backend, &namespace, &repository_key, repository_spec).await?;
        self.reconcile_project_checkouts(&namespace, &repository_key, repository_spec, inspection.checkout.clone()).await?;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace);
        let stored = repositories.get(&repository_key.to_string()).await.map_err(|error| error.to_string())?;
        if stored.spec != *repository_spec {
            // This path is authoritative for per-repository config, so it may
            // intentionally clear provenance that identity-only observations
            // preserve in `ensure_repository`.
            repositories
                .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, repository_spec)
                .await
                .map_err(|error| error.to_string())?;
        }

        let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
        let repository_objects = repositories.list().await.map_err(|error| error.to_string())?.items;
        let repository_specs = repository_objects
            .iter()
            .map(|repository| (RepositoryKey(repository.metadata.name.clone()), repository.spec.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut superseded_keys = BTreeSet::new();
        let mut declared_alias_keys = BTreeSet::new();
        let previous_tracked_key = self
            .repository_keys_by_path
            .read()
            .await
            .get(&inspection.checkout.path)
            .filter(|previous| *previous != &repository_key)
            .cloned();
        if let Some(previous) = &previous_tracked_key {
            superseded_keys.insert(previous.clone());
        }
        for (key, spec) in &repository_specs {
            let aliases_current_repository = spec.remotes().iter().any(|remote| repository_spec.declares_remote(remote));
            if key != &repository_key && (local_repository_matches_checkout(spec, &inspection.checkout) || aliases_current_repository) {
                superseded_keys.insert(key.clone());
                if aliases_current_repository {
                    declared_alias_keys.insert(key.clone());
                }
            }
        }
        for checkout in self
            .observed_resource_backend
            .clone()
            .using::<ResourceCheckout>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
        {
            if let ResourceCheckoutSpec::Observed(observed) = checkout.spec {
                if Path::new(&observed.path) == inspection.checkout.path && observed.repo_ref != repository_key {
                    superseded_keys.insert(observed.repo_ref);
                }
            }
        }

        let other_tracked_keys = self
            .repository_keys_by_path
            .read()
            .await
            .iter()
            .filter(|(path, _)| *path != &inspection.checkout.path)
            .map(|(_, key)| key.clone())
            .collect::<BTreeSet<_>>();
        let mut migratable_keys = superseded_keys.difference(&other_tracked_keys).cloned().collect::<BTreeSet<_>>();
        migratable_keys.extend(declared_alias_keys);

        let mut project_objects = projects.list().await.map_err(|error| error.to_string())?;
        let display_name = normalize_project_name(&repository_spec.leaf_slug())?;
        let canonical_generated_names = whole_repository_project_names(repository_spec)?.into_iter().collect::<BTreeSet<_>>();
        let mut generated_names = canonical_generated_names.clone();
        let mut generated_display_names = BTreeSet::from([display_name.clone()]);
        for key in &migratable_keys {
            if let Some(spec) = repository_specs.get(key) {
                generated_names.extend(whole_repository_project_names(spec)?);
                generated_display_names.insert(normalize_project_name(&spec.leaf_slug())?);
            }
        }
        let obsolete_mirror_projects = project_objects
            .iter()
            .filter(|project| {
                // The standing lab mirrors predate deterministic generated
                // names and were explicitly materialized as `<name>-lab`.
                project.metadata.labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == WHOLE_REPOSITORY_PROJECT_MANAGED_BY_VALUE)
                    && !canonical_generated_names.contains(&project.metadata.name)
                    && (generated_names.contains(&project.metadata.name) || project.metadata.name.ends_with("-lab"))
                    && matches!(project.spec.repositories.as_slice(), [entry] if migratable_keys.contains(&entry.repo))
                    && repository_specs.get(&project.spec.repositories[0].repo).is_some_and(|spec| !spec.remotes().is_empty())
            })
            .map(|project| project.metadata.name.clone())
            .collect::<BTreeSet<_>>();
        for project_name in &obsolete_mirror_projects {
            projects.delete(project_name).await.map_err(|error| error.to_string())?;
        }
        project_objects.retain(|project| !obsolete_mirror_projects.contains(&project.metadata.name));
        let mut migrated_project_names = BTreeSet::new();
        for project in &mut project_objects {
            if is_declaration_backed_project(project) {
                continue;
            }
            let mut updated = project.spec.clone();
            let mut changed = false;
            for entry in &mut updated.repositories {
                if migratable_keys.contains(&entry.repo) {
                    entry.repo = repository_key.clone();
                    changed = true;
                }
            }
            if changed {
                updated = normalize_project_spec(updated)?;
                projects.apply(&InputMeta::from(&project.metadata), &updated).await.map_err(|error| error.to_string())?;
                project.spec = updated;
                migrated_project_names.insert(project.metadata.name.clone());
            }
        }

        let spec = whole_repository_project_spec(repository_key.clone(), display_name)?;
        let primary_name = project_objects
            .iter()
            .filter(|project| is_whole_repository_project(&project.spec, &repository_key))
            .min_by_key(|project| {
                (
                    !migrated_project_names.contains(&project.metadata.name),
                    generated_names.contains(&project.metadata.name),
                    project.metadata.name.as_str(),
                )
            })
            .map(|project| project.metadata.name.clone());
        if let Some(primary_name) = &primary_name {
            let primary = project_objects
                .iter_mut()
                .find(|project| project.metadata.name == *primary_name)
                .expect("selected primary Project should remain in the listed objects");
            *primary = reconcile_whole_repository_project_definition(&projects, primary.clone()).await?;
            for duplicate in project_objects.iter().filter(|project| {
                project.metadata.name != *primary_name
                    && generated_names.contains(&project.metadata.name)
                    && generated_display_names.contains(&project.spec.display_name)
                    && project.spec.default_workflow_ref == spec.default_workflow_ref
                    && project.spec.issue_source == spec.issue_source
                    && project.spec.repositories == spec.repositories
            }) {
                projects.delete(&duplicate.metadata.name).await.map_err(|error| error.to_string())?;
            }
        }

        let remaining_projects = projects.list().await.map_err(|error| error.to_string())?;
        let durable_checkouts =
            self.resource_backend.clone().using::<ResourceCheckout>(&namespace).list().await.map_err(|error| error.to_string())?.items;
        for old_key in &migratable_keys {
            let still_referenced =
                remaining_projects.iter().any(|project| project.spec.repositories.iter().any(|entry| &entry.repo == old_key));
            let has_durable_checkout = durable_checkouts.iter().any(|checkout| checkout.spec.repo_ref() == old_key);
            if !still_referenced && !has_durable_checkout {
                crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, old_key)
                    .await
                    .map_err(|error| error.to_string())?;
                match repositories.delete(&old_key.to_string()).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            } else if !still_referenced && has_durable_checkout {
                if let Some(old_repository) = repository_objects.iter().find(|repository| repository.metadata.name == old_key.to_string()) {
                    let mut meta = InputMeta::from(&old_repository.metadata);
                    meta.annotations.insert(SUPERSEDED_BY_ANNOTATION.to_string(), repository_key.to_string());
                    match repositories.update(&meta, &old_repository.metadata.resource_version, &old_repository.spec).await {
                        Ok(_) | Err(ResourceError::NotFound { .. }) => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
        }

        let previous_spec = previous_tracked_key
            .as_ref()
            .and_then(|key| repository_specs.get(key))
            .or_else(|| superseded_keys.iter().find_map(|key| repository_specs.get(key)));
        let identity_change = previous_spec.map(|previous| RepositoryIdentityChange {
            previous_display: repository_identity_display(previous),
            current_display: repository_identity_display(repository_spec),
        });
        if primary_name.is_some() {
            return Ok(identity_change);
        }

        for project_name in whole_repository_project_names(repository_spec)? {
            match projects.create(&whole_repository_project_meta(project_name.clone()), &spec).await {
                Ok(_) => return Ok(identity_change),
                Err(ResourceError::Conflict { .. }) => {
                    let existing = projects.get(&project_name).await.map_err(|error| error.to_string())?;
                    if is_whole_repository_project(&existing.spec, &repository_key) {
                        reconcile_whole_repository_project_definition(&projects, existing).await?;
                        return Ok(identity_change);
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Err(format!("could not allocate a deterministic Project name for repository {repository_key}"))
    }

    async fn reconcile_repository_config(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
    ) -> Result<(), String> {
        let repositories = self.resource_backend.clone().using::<Repository>(namespace);
        let stored = flotilla_resources::ensure_repository(&repositories, repository_key, repository_spec)
            .await
            .map_err(|error| error.to_string())?;
        if stored.spec != *repository_spec {
            // Unlike identity-only observations, the current per-repository
            // config is authoritative and may remove a previously set stance.
            repositories
                .update(&InputMeta::from(&stored.metadata), &stored.metadata.resource_version, repository_spec)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub async fn materialize_tracked_repo_projects(&self) -> Result<(), String> {
        for repo_path in self.tracked_repo_paths().await {
            let inspection = match self.inspect_repository_path(&repo_path, None).await {
                Ok(inspection) => inspection,
                Err(error) => {
                    warn!(repo = %repo_path.display(), %error, "skipping Project backfill because repository identity resolution failed");
                    continue;
                }
            };
            self.reconcile_whole_repository_project(&inspection)
                .await
                .map_err(|error| format!("materialize whole-repository Project for {}: {error}", repo_path.display()))?;
        }
        Ok(())
    }

    async fn reconcile_project_checkouts(
        &self,
        namespace: &str,
        repository_key: &RepositoryKey,
        repository_spec: &RepositorySpec,
        checkout: crate::repository_inspection::LocalCheckoutInspection,
    ) -> Result<(), String> {
        let inspection = RepositoryInspection { spec: repository_spec.clone(), checkout, transport_url: None };
        let inspector = self.repository_inspector().await?;
        let mut providers = ProviderData::default();
        for checkout in inspector.inspect_checkouts(&inspection).await? {
            providers.checkouts.insert(QualifiedPath::host(HostId::new(checkout.host_ref), checkout.path), flotilla_protocol::Checkout {
                branch: checkout.git_ref,
                is_main: checkout.is_main,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                host_name: None,
                environment_id: None,
            });
        }
        crate::observed_resources::reconcile_checkouts(
            &self.observed_resource_backend,
            namespace,
            repository_key,
            &repository_spec.catalog_slug(),
            &providers,
            &inspection.checkout.host_ref,
        )
        .await
        .map_err(|error| error.to_string())
    }

    pub async fn refresh(&self, repo: &flotilla_protocol::RepoSelector) -> Result<Option<RepositoryIdentityChange>, String> {
        let repo = self.resolve_repo_selector(repo).await?;
        let identity = self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
        let identity_change = match self.inspect_repository_path(&repo, None).await {
            Ok(inspection) => {
                let key_changed = self.repository_keys_by_path.read().await.get(&repo) != Some(&inspection.key());
                let identity_change = if key_changed {
                    self.reconcile_whole_repository_project(&inspection).await?
                } else {
                    let namespace = self.provisioning_namespace().await;
                    self.reconcile_repository_config(&namespace, &inspection.key(), &inspection.spec).await?;
                    None
                };
                if key_changed {
                    {
                        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                        if self.tracked_repo_identity_for_path(&repo).await.as_ref() != Some(&identity) {
                            return Err(format!("repo not tracked: {}", repo.display()));
                        }
                        self.repository_keys_by_path.write().await.insert(repo.clone(), inspection.key());
                    }
                    self.publish_repo_info_update(&identity).await;
                }
                identity_change
            }
            Err(error) => {
                warn!(repo = %repo.display(), %error, "repository identity is unavailable during refresh");
                None
            }
        };
        Ok(identity_change)
    }

    /// Refresh host-local bare pane state and publish field-scoped deltas.
    /// Pools are scanned once even when several tracked repositories share the
    /// same host-scoped provider.
    pub async fn refresh_managed_terminal_attention(&self) {
        struct RepoTerminals {
            identity: RepoIdentity,
            roots: Vec<PathBuf>,
            pool_key: usize,
        }

        let (repos, pools) = {
            let tracked = self.repos.read().await;
            let mut repos = Vec::new();
            let mut pools = HashMap::new();
            for state in tracked.values() {
                let registry = state.registry();
                let Some(pool) = registry.terminal_pools.preferred().cloned() else { continue };
                let pool_key = Arc::as_ptr(&pool) as *const () as usize;
                pools.entry(pool_key).or_insert(pool);
                let roots = state.local_paths().into_iter().map(|root| canonical_or_original(&root)).collect();
                repos.push(RepoTerminals { identity: state.identity().clone(), roots, pool_key });
            }
            (repos, pools)
        };

        let store = self.discovery.shared_attachable_store(&self.config);
        for (pool_key, pool) in pools {
            let manager = crate::terminal_manager::TerminalManager::new(pool, store.clone(), self.host_name.clone());
            let terminals = match manager.refresh().await {
                Ok(terminals) => terminals,
                Err(error) => {
                    warn!(%error, "failed to refresh managed terminal attention");
                    continue;
                }
            };
            let pool_repos = repos.iter().filter(|repo| repo.pool_key == pool_key).collect::<Vec<_>>();
            let mut current = pool_repos.iter().map(|repo| (repo.identity.clone(), HashMap::new())).collect::<HashMap<_, HashMap<_, _>>>();
            for terminal in &terminals {
                let working_directory = canonical_or_original(terminal.working_directory.as_path());
                // A nested checkout can share a path prefix with another
                // tracked repository. Attribute the pane to the most-specific
                // root only so one exit cannot surface on multiple checkouts.
                let owner = pool_repos
                    .iter()
                    .flat_map(|repo| repo.roots.iter().map(move |root| (*repo, root)))
                    .filter(|(_, root)| working_directory.starts_with(root))
                    .max_by_key(|(_, root)| root.components().count())
                    .map(|(repo, _)| repo);
                if let Some(repo) = owner {
                    current.get_mut(&repo.identity).expect("pool repository is initialized").insert(
                        terminal.attachable_id.clone(),
                        ManagedTerminal {
                            set_id: terminal.attachable_set_id.clone(),
                            role: terminal.role.clone(),
                            command: terminal.command.clone(),
                            working_directory: terminal.working_directory.as_path().to_path_buf(),
                            status: terminal.status.clone(),
                            attention: terminal.attention.clone(),
                        },
                    );
                }
            }

            let mut previous = self.managed_terminals_by_repo.write().await;
            for repo in pool_repos {
                let next = current.remove(&repo.identity).expect("pool repository is initialized");
                let changes = managed_terminal_changes(previous.get(&repo.identity), &next);
                previous.insert(repo.identity.clone(), next);
                if changes.is_empty() {
                    continue;
                }
                let _ = self.event_tx.send(DaemonEvent::RepoDelta(Box::new(RepoDelta {
                    seq: 0,
                    prev_seq: 0,
                    repo_identity: repo.identity.clone(),
                    repo: repo.roots.first().cloned(),
                    changes,
                })));
            }
        }
    }

    /// Resolve a path that might be a git worktree to the main repo root.
    ///
    /// Returns `(resolved_path, Some(original_path))` if normalization changed
    /// the path, or `(original_path, None)` if no change was needed.
    async fn normalize_repo_path(&self, path: &Path) -> (PathBuf, Option<PathBuf>) {
        use crate::{
            path_context::ExecutionEnvironmentPath,
            providers::vcs::{git::GitVcs, Vcs},
        };

        let vcs = GitVcs::new(self.discovery.runner.clone());
        let ee_path = ExecutionEnvironmentPath::new(path);
        match vcs.resolve_repo_root(&ee_path).await {
            Some(repo_root) => {
                let repo_root_raw = repo_root.into_path_buf();
                // Canonicalize to handle symlinks (e.g. /var -> /private/var on macOS).
                let canonical_root = std::fs::canonicalize(&repo_root_raw).unwrap_or(repo_root_raw);
                let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if canonical_root != canonical_path {
                    debug!(
                        worktree = %path.display(),
                        repo_root = %canonical_root.display(),
                        "normalized worktree path to main repo root"
                    );
                    (canonical_root, Some(path.to_path_buf()))
                } else {
                    (canonical_root, None)
                }
            }
            None => (path.to_path_buf(), None),
        }
    }

    async fn publish_repo_info_update(&self, identity: &flotilla_protocol::RepoIdentity) {
        if let Ok(repo_infos) = self.list_repos().await {
            if let Some(info) = repo_infos.into_iter().find(|info| info.identity == *identity) {
                // RepoTracked also carries late identity enrichment: surfaces
                // treat an existing identity as an update.
                let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(info)));
            }
        }
    }

    /// Add a repo to tracking and report path normalization or identity migration.
    ///
    /// If `path` is a git worktree, the main repo root is resolved via
    /// `git rev-parse --path-format=absolute --git-common-dir` and tracked
    /// instead. When the path resolves to a git repo root, the returned
    /// `tracked_path` is canonicalized and `resolved_from` is
    /// `Some(original_path)` when the repo root changes.
    pub async fn add_repo(&self, path: &Path) -> Result<AddRepoOutcome, String> {
        let (path, resolved_from) = self.normalize_repo_path(path).await;

        // Create the model outside the lock (spawns provider detection and refresh)
        let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } = discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &self.local_environment_id,
            &path,
        )
        .await?;
        if !unmet.is_empty() {
            debug!(count = unmet.len(), ?unmet, "providers not activated: missing requirements");
        }
        let identity = configured_repo_identity_or_bag_or_path(&self.config, &path, &host_repo_bag);
        // Resolve the storage identity before publishing RepoTracked so a
        // surface can subscribe to issues{repository} immediately. The
        // background refresh also reconciles the Repository resource and
        // observed Checkouts.
        let repository_inspection = self
            .inspect_repository_path(&path, None)
            .await
            .map_err(|error| format!("cannot track repository {}: {error}", path.display()))?;
        let repository_key = Some(repository_inspection.key());
        let identity_change = self.reconcile_whole_repository_project(&repository_inspection).await?;
        if let Some(tracked_identity) = self.tracked_repo_identity_for_path(&path).await {
            if tracked_identity == identity {
                let key_became_available = {
                    let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                    if self.tracked_repo_identity_for_path(&path).await.as_ref() != Some(&identity) {
                        false
                    } else if let Some(repository_key) = repository_key.as_ref() {
                        self.repository_keys_by_path.write().await.insert(path.clone(), repository_key.clone()).as_ref()
                            != Some(repository_key)
                    } else {
                        false
                    }
                };
                if key_became_available {
                    self.publish_repo_info_update(&identity).await;
                }
                if self.tracked_repo_identity_for_path(&path).await.as_ref() == Some(&identity) {
                    return Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change });
                }
            }
            if let Err(error) = self.remove_repo(&path).await {
                // Another add_repo call may have removed or migrated this path
                // after our identity lookup. Continue through the idempotent
                // insertion path unless it is still tracked elsewhere.
                if self.tracked_repo_identity_for_path(&path).await.is_some_and(|current| current != identity) {
                    return Err(error);
                }
            }
        }
        let slug = repo_slug.clone();
        let model = RepoModel::new(registry, Some(self.local_environment_id.clone()));
        let root = RepoRootState { path: path.clone(), model, slug, repo_bag, unmet, is_local: true };

        let repo_info = RepoInfo {
            identity: identity.clone(),
            repository_key: repository_key.clone(),
            path: Some(path.clone()),
            name: repo_name(&path),
            labels: root.model.labels.clone(),
            provider_names: provider_names_from_registry(&root.model.registry)
                .into_iter()
                .map(|(category, entries)| (category, entries.into_iter().map(|e| e.display_name).collect()))
                .collect(),
            provider_health: HashMap::new(),
            loading: false,
        };

        // Insert under write lock — re-check to avoid TOCTOU duplicate
        let mut added_new_identity = false;
        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
        let already_tracked = self.path_identities.read().await.contains_key(&path);
        if already_tracked {
            return Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change });
        }
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            if let Some(state) = repos.get_mut(&identity) {
                state.add_root(root);
            } else {
                repos.insert(identity.clone(), RepoState::new(identity.clone(), root));
                order.push(identity.clone());
                added_new_identity = true;
            }
            self.path_identities.write().await.insert(path.clone(), identity.clone());
        }
        if let Some(repository_key) = repository_key {
            self.repository_keys_by_path.write().await.insert(path.clone(), repository_key);
        }

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        self.config.save_repo(&ExecutionEnvironmentPath::new(&path));

        info!(repo = %path.display(), "added repo");
        if added_new_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoTracked(Box::new(repo_info)));
        }

        Ok(AddRepoOutcome { tracked_path: path, resolved_from, identity_change })
    }

    pub async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        let repo_identity = self.tracked_repo_identity_for_path(&path).await.unwrap_or_else(|| fallback_repo_identity(&path));
        let observed_reconciliation = self.observed_checkout_reconciliation.lock().await;
        let repository_key = match self.repository_keys_by_path.read().await.get(&path).cloned() {
            Some(key) => Some(key),
            None => self.inspect_repository_path(&path, None).await.ok().map(|inspection| inspection.key()),
        };

        let mut removed_identity = false;
        let removed_final_local_root;
        {
            let mut repos = self.repos.write().await;
            let mut order = self.repo_order.write().await;
            let Some(state) = repos.get_mut(&repo_identity) else {
                return Err(format!("repo not tracked: {}", path.display()));
            };
            let previous_preferred = state.preferred_path().to_path_buf();
            if !state.remove_root(&path) {
                return Err(format!("repo not tracked: {}", path.display()));
            }
            removed_final_local_root = state.local_paths().is_empty();
            if state.roots.is_empty() {
                repos.remove(&repo_identity);
                order.retain(|repo| repo != &repo_identity);
                removed_identity = true;
            } else if previous_preferred == path {
            }
        }

        // Remove from identity maps.
        self.path_identities.write().await.remove(&path);
        self.repository_keys_by_path.write().await.remove(&path);

        if removed_final_local_root {
            let namespace = self.provisioning_namespace().await;
            if let Some(repository_key) = repository_key {
                if let Err(error) =
                    crate::observed_resources::delete_observed_checkouts(&self.observed_resource_backend, &namespace, &repository_key).await
                {
                    warn!(repo = %repo_identity.path, %error, "failed to delete observed checkouts for untracked repo");
                }
            } else {
                warn!(repo = %repo_identity.path, "could not resolve repository identity while deleting observed checkouts");
            }
        }
        drop(observed_reconciliation);

        // Persist to config. Tab order is Surface-owned (open-views.toml,
        // ADR 0013) — the daemon only tracks registration.
        self.config.remove_repo(&ExecutionEnvironmentPath::new(&path));

        info!(repo = %path.display(), "removed repo");
        if removed_identity {
            let _ = self.event_tx.send(DaemonEvent::RepoUntracked { repo_identity, path: Some(path) });
        }

        Ok(())
    }

    // --- Internal query helpers (formerly DaemonHandle trait methods) ---

    pub async fn get_repo_providers_internal(&self, repo: &flotilla_protocol::RepoSelector) -> Result<RepoProvidersResponse, String> {
        let repo_path = self.resolve_repo_selector(repo).await?;
        let identity =
            self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;

        let host_bag = state
            .preferred_environment_id()
            .and_then(|env_id| self.environment_manager.environment_bag(env_id))
            .unwrap_or_else(|| self.environment_manager.local_environment_bag());
        let host_discovery = host_bag.assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();
        let repo_discovery = state.repo_bag().assertions().iter().map(crate::convert::assertion_to_discovery_entry).collect();

        let provider_infos = state
            .preferred_root()
            .model
            .registry
            .provider_infos()
            .into_iter()
            .map(|(category, name)| ProviderInfo { category, name, healthy: true, disabled_reason: None })
            .collect();

        let unmet_requirements =
            state.unmet().iter().map(|(factory, req)| crate::convert::unmet_requirement_to_proto(factory, req)).collect();

        Ok(RepoProvidersResponse {
            path: state.preferred_path().to_path_buf(),
            slug: state.slug().map(str::to_string),
            host_discovery,
            repo_discovery,
            providers: provider_infos,
            unmet_requirements,
        })
    }

    pub async fn list_hosts_internal(&self) -> Result<HostListResponse, String> {
        let _ = self.refresh_local_host_summary().await;
        let counts = self.local_host_counts().await;
        Ok(self.host_registry.list_hosts(&counts).await)
    }

    pub async fn dispatch_queue_internal(&self, project_filter: Option<&str>) -> Result<DispatchQueueResponse, String> {
        let observed_at = Utc::now();
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        for project in projects {
            if project_filter.is_some_and(|filter| filter != project.metadata.name) {
                continue;
            }
            let Some(status) = project.status else { continue };
            let attention = status.dispatch_queue_attention.is_some();
            for entry in status.dispatch_queue {
                entries.push(
                    DispatchQueueRow::builder()
                        .namespace(project.metadata.namespace.clone())
                        .project(project.metadata.name.clone())
                        .issue(entry.issue)
                        .title(entry.title)
                        .ready_observed_at(entry.ready_observed_at)
                        .age_seconds(observed_at.signed_duration_since(entry.ready_observed_at).num_seconds().max(0) as u64)
                        .attention(attention)
                        .provenance(entry.provenance)
                        .build(),
                );
            }
        }
        entries.sort_by(|left, right| {
            (&left.namespace, &left.project, left.ready_observed_at, &left.issue).cmp(&(
                &right.namespace,
                &right.project,
                right.ready_observed_at,
                &right.issue,
            ))
        });
        Ok(DispatchQueueResponse { observed_at, entries })
    }

    pub async fn fleet_health_internal(&self) -> Result<FleetHealthResponse, String> {
        let now = Utc::now();
        let namespace = self.provisioning_namespace().await;
        let host_list = self.list_hosts_internal().await?;
        let configured_hosts = self.config.load_hosts().map(|hosts| hosts.hosts).unwrap_or_default();
        let configured_names =
            configured_hosts.values().map(|remote| HostName::new(remote.expected_host_name.clone())).collect::<HashSet<_>>();
        let configured_by_node = configured_hosts
            .values()
            .filter_map(|remote| remote.expected_node_id.clone().map(|node_id| (node_id, HostName::new(remote.expected_host_name.clone()))))
            .collect::<HashMap<_, _>>();

        let mut host_rows = BTreeMap::<HostName, (bool, bool, PeerConnectionState)>::new();
        for entry in host_list.hosts {
            let configured = entry.configured || configured_names.contains(&entry.host_name);
            host_rows
                .entry(entry.host_name)
                .and_modify(|row| {
                    row.0 |= entry.is_local;
                    row.1 |= configured;
                    if entry.connection_status == PeerConnectionState::Connected {
                        row.2 = PeerConnectionState::Connected;
                    }
                })
                .or_insert((entry.is_local, configured, entry.connection_status));
        }
        for host in configured_names {
            host_rows.entry(host).or_insert((false, true, PeerConnectionState::Disconnected));
        }
        host_rows.entry(self.host_name.clone()).or_insert((true, false, PeerConnectionState::Connected));

        let local_host_id = self.local_host_id().map(|host_id| host_id.to_string());
        let mut statuses = HashMap::<HostName, ResourceHostStatus>::new();
        let resource_hosts =
            self.resource_backend.clone().including_replicas::<ResourceHost>(&namespace).list().await.map_err(|error| error.to_string())?;
        for resource_host in resource_hosts.items {
            let host = match &resource_host.provenance {
                ResourceProvenance::Local if local_host_id.as_deref() == Some(resource_host.object.metadata.name.as_str()) => {
                    Some(self.host_name.clone())
                }
                ResourceProvenance::Local => None,
                ResourceProvenance::Replica { origin_root, .. } => {
                    // An origin replicates every Host it observes, including this daemon's Host.
                    // Only the Host matching the origin's canonical environment is its self-report.
                    let is_self_report = self
                        .host_registry
                        .environment_id_for_node(origin_root)
                        .await
                        .and_then(|environment_id| environment_id.host_id().map(ToString::to_string))
                        .is_some_and(|host_id| host_id == resource_host.object.metadata.name);
                    if !is_self_report {
                        None
                    } else {
                        self.host_registry.host_name_for_node(origin_root).await.or_else(|| configured_by_node.get(origin_root).cloned())
                    }
                }
            };
            let (Some(host), Some(status)) = (host, resource_host.object.status) else {
                continue;
            };
            let replace = statuses.get(&host).is_none_or(|current| current.heartbeat_at < status.heartbeat_at);
            if replace {
                statuses.insert(host, status);
            }
        }

        let (local_rows, _) = self.local_fleet_rows(&namespace).await?;
        let mut counts = HashMap::<HostName, (usize, HashSet<String>)>::new();
        accumulate_fleet_health_counts(&mut counts, &local_rows);
        let replicas = self.fleet_replica_cache.read().await;
        for entry in replicas.values() {
            accumulate_fleet_health_counts(&mut counts, &entry.rows);
        }

        let warning_window_days = self.config.load_daemon_config().unwrap_or_default().credentials.warning_window_days;
        let credential_warning_window = chrono::Duration::days(i64::from(warning_window_days));
        let mut rows = Vec::with_capacity(host_rows.len());
        for (host, (is_local, configured, link)) in host_rows {
            let status = statuses.get(&host);
            let replica = replicas.get(&host);
            let heartbeat_at = status.and_then(|status| status.heartbeat_at);
            let heartbeat_fresh = heartbeat_at.is_some_and(|at| now.signed_duration_since(at).num_seconds() <= HEARTBEAT_READY_TTL_SECS);
            let replica_fresh = is_local
                || replica.is_some_and(|replica| {
                    replica.last_error.is_none()
                        && replica.last_sync.is_some_and(|at| now.signed_duration_since(at).num_seconds() <= FLEET_REPLICA_FRESH_SECS)
                });
            let daemon_generation = status.and_then(|status| status.daemon_generation.clone());
            let replica_generation =
                if is_local { daemon_generation.clone() } else { replica.and_then(|replica| replica.generation.clone()) };
            let staleness = if heartbeat_fresh && replica_fresh {
                FleetHostStaleness::Current
            } else if heartbeat_at.is_some() || replica.and_then(|replica| replica.last_sync).is_some() {
                FleetHostStaleness::Stale
            } else {
                FleetHostStaleness::Unknown
            };
            let observation_agreement = fleet_observation_agreement(
                &link,
                heartbeat_at,
                heartbeat_fresh,
                daemon_generation.as_deref(),
                replica_generation.as_deref(),
                is_local,
            );
            let (crew_count, convoys) = counts.remove(&host).unwrap_or_default();
            let degraded_conditions = status
                .into_iter()
                .flat_map(|status| status.conditions.iter())
                .filter(|condition| condition.value == ConditionValue::False)
                .map(|condition| format!("{}: {}", condition.condition_type, condition.message))
                .collect();
            let credential_attention =
                status.map(|status| host_credential_attention(status, now, credential_warning_window)).unwrap_or_default();

            rows.push(
                FleetHostRow::builder()
                    .host(host)
                    .is_local(is_local)
                    .configured(configured)
                    .link(link)
                    .maybe_daemon_generation(daemon_generation)
                    .maybe_daemon_version(status.and_then(|status| status.daemon_version.clone()))
                    .maybe_daemon_uptime_seconds(status.and_then(|status| {
                        status.daemon_started_at.map(|started_at| now.signed_duration_since(started_at).num_seconds().max(0) as u64)
                    }))
                    .maybe_heartbeat_at(heartbeat_at)
                    .maybe_replica_last_sync(if is_local { Some(now) } else { replica.and_then(|replica| replica.last_sync) })
                    .maybe_replica_generation(replica_generation)
                    .crew_count(crew_count)
                    .convoy_count(convoys.len())
                    .maybe_disk_free_bytes(status.and_then(|status| status.disk_free_bytes))
                    .sleep_inhibition(status.map(|status| status.sleep_inhibition.clone()).unwrap_or_default())
                    .staleness(staleness)
                    .observation_agreement(observation_agreement)
                    .degraded_conditions(degraded_conditions)
                    .credential_attention(credential_attention)
                    .build(),
            );
        }
        rows.sort_by(|left, right| right.is_local.cmp(&left.is_local).then_with(|| left.host.cmp(&right.host)));
        let dispatch_queue = self.dispatch_queue_internal(None).await?;
        Ok(FleetHealthResponse { hosts: rows, dispatch_queue })
    }

    pub async fn list_projects_internal(&self) -> Result<ProjectListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.clone().definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = self.resource_backend.clone().using::<Repository>(&namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = repositories
            .items
            .into_iter()
            .map(|repository| (RepositoryKey(repository.metadata.name.clone()), repository))
            .collect::<Vec<_>>();
        let repository_slugs = repository_display_labels(repositories.iter().map(|(key, repository)| (key, &repository.spec)));

        let mut entries = projects
            .into_iter()
            .map(|project| {
                let conflicts = project.metadata.merge.as_ref().map(|merge| merge.conflicts.keys().cloned().collect()).unwrap_or_default();
                let mut project_repositories = BTreeMap::<RepositoryKey, BTreeSet<String>>::new();
                for repository in project.spec.repositories {
                    if let Some(subpath) = repository.subpath {
                        project_repositories.entry(repository.repo).or_default().insert(subpath);
                    } else {
                        project_repositories.entry(repository.repo).or_default();
                    }
                }
                let repositories = project_repositories
                    .into_iter()
                    .map(|(key, subpaths)| ProjectListRepository {
                        slug: repository_slugs.get(&key).cloned(),
                        key,
                        subpaths: subpaths.into_iter().collect(),
                    })
                    .collect::<Vec<_>>();
                ProjectListEntry::builder()
                    .namespace(project.metadata.namespace.clone())
                    .name(project.metadata.name.clone())
                    .display_name(project.spec.display_name)
                    .address(ViewAddress::Project { namespace: project.metadata.namespace, name: project.metadata.name })
                    .repositories(repositories)
                    .maybe_issue_source(project.spec.issue_source)
                    .default_workflow_ref(project.spec.default_workflow_ref)
                    .conflicts(conflicts)
                    .build()
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
        Ok(ProjectListResponse { projects: entries })
    }

    pub async fn get_host_status_internal(&self, environment_id: &EnvironmentId) -> Result<HostStatusResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        let counts = self.local_host_counts().await;
        let mut response = self.host_registry.get_host_status(environment_id, &counts).await?;
        if environment_id == &local_summary.environment_id {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub async fn get_host_providers_internal(&self, environment_id: &EnvironmentId) -> Result<HostProvidersResponse, String> {
        let local_summary = self.refresh_local_host_summary().await;
        let counts = self.local_host_counts().await;
        let mut response = self.host_registry.get_host_providers(environment_id, &counts).await?;
        if environment_id == &local_summary.environment_id {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub async fn fleet_replica_snapshot_internal(&self) -> Result<FleetReplicaSnapshot, String> {
        let namespace = self.provisioning_namespace().await;
        let (rows, generation) = self.local_fleet_rows(&namespace).await?;
        let result_sets = self.aggregator_projection_state().await.local_result_sets().await;
        Ok(FleetReplicaSnapshot { host: self.host_name.clone(), generation, rows, result_sets })
    }

    pub async fn fleet_list_internal(&self) -> Result<FleetListResponse, String> {
        let namespace = self.provisioning_namespace().await;
        let (mut rows, _generation) = self.local_fleet_rows(&namespace).await?;
        let mut replicas = Vec::new();
        let now = Utc::now();
        let configured_hosts = self.config.load_hosts().map(|hosts| hosts.hosts).unwrap_or_default();
        let failures = self.resource_replication_failures.read().await.clone();
        let mut replication_failures_by_host = HashMap::<HostName, Vec<ResourceReplicationFailure>>::new();
        for (peer, peer_failures) in failures {
            let host = self.host_registry.host_name_for_node(&peer).await.unwrap_or_else(|| HostName::new(peer.as_str()));
            replication_failures_by_host
                .entry(host)
                .or_default()
                .extend(peer_failures.into_iter().map(|(kind, message)| ResourceReplicationFailure { kind, message }));
        }
        let cache = self.fleet_replica_cache.read().await;

        for (label, remote) in configured_hosts {
            let host = HostName::new(remote.expected_host_name);
            let replication_failures = replication_failures_by_host.remove(&host).unwrap_or_default();
            let replication_error = format_resource_replication_failures(&replication_failures);
            match cache.get(&host) {
                Some(entry) => {
                    let staleness = replica_staleness(entry, now);
                    rows.extend(entry.rows.iter().cloned().map(|mut row| {
                        row.staleness = staleness.clone();
                        row
                    }));
                    replicas.push(FleetReplicaStatus {
                        host,
                        reachable: entry.last_error.is_none() && replication_error.is_none(),
                        last_sync: entry.last_sync,
                        generation: entry.generation.clone(),
                        skipped_records: entry.skipped_records,
                        first_parse_error: entry.first_parse_error.clone(),
                        message: join_replica_errors(entry.last_error.as_deref(), replication_error.as_deref()),
                    });
                }
                None => {
                    let unsynced = format!("replica source '{label}' has not synced yet");
                    replicas.push(FleetReplicaStatus {
                        host,
                        reachable: false,
                        last_sync: None,
                        generation: None,
                        skipped_records: 0,
                        first_parse_error: None,
                        message: join_replica_errors(Some(&unsynced), replication_error.as_deref()),
                    });
                }
            }
        }
        for (host, failures) in replication_failures_by_host {
            replicas.push(FleetReplicaStatus {
                host,
                reachable: false,
                last_sync: None,
                generation: None,
                skipped_records: 0,
                first_parse_error: None,
                message: format_resource_replication_failures(&failures),
            });
        }

        rows.sort_by(|left, right| {
            (&left.convoy, left.host.as_str(), &left.vessel, &left.crew).cmp(&(
                &right.convoy,
                right.host.as_str(),
                &right.vessel,
                &right.crew,
            ))
        });
        replicas.sort_by(|left, right| left.host.as_str().cmp(right.host.as_str()));
        Ok(FleetListResponse { rows, replicas })
    }

    /// Resolve enough crew identity locally to route a verb to the convoy
    /// authority. Unlike `resolve_crew_context`, this does not read the
    /// authority-owned Convoy or Vessel.
    pub async fn resolve_crew_routing_context(&self, requested: &CrewCommandContext) -> Result<CrewRoutingContext, String> {
        let provisioning_namespace = self.provisioning_namespace().await;
        let namespace = requested.namespace.clone().unwrap_or_else(|| provisioning_namespace.clone());
        if namespace != provisioning_namespace {
            return Err(format!("crew namespace `{namespace}` is not served by this daemon"));
        }
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace);
        let session_list = sessions.list().await.map_err(|err| err.to_string())?.items;

        if let Some(crew_id) = requested.crew_id.as_deref() {
            let session = session_list
                .iter()
                .find(|session| session.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
                .ok_or_else(|| format!("unknown FLOTILLA_CREW_ID `{crew_id}`"))?;
            let role = session.spec.role.clone();
            let (convoy, vessel_ref) = match &session.spec.source {
                TerminalSessionSource::Agent { context, .. } => (context.convoy.clone(), context.vessel_ref.clone()),
                TerminalSessionSource::Tool { .. } => {
                    return Err(format!("crew identity `{crew_id}` belongs to a non-agent process"));
                }
            };
            return Ok(CrewRoutingContext {
                command_context: CrewCommandContext {
                    crew_id: None,
                    namespace: Some(namespace),
                    convoy: Some(convoy.clone()),
                    vessel_ref: Some(vessel_ref),
                    role: Some(role),
                },
                session_name: Some(session.metadata.name.clone()),
                convoy,
            });
        }

        let convoy = requested
            .convoy
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let vessel_ref = requested
            .vessel_ref
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let role = requested
            .role
            .clone()
            .ok_or_else(|| "crew context requires FLOTILLA_CREW_ID or --convoy, --vessel-ref, and --role".to_string())?;
        let caller = session_list.iter().find(|session| {
            session.spec.role == role
                && (session.metadata.labels.get(VESSEL_REF_LABEL).map(String::as_str) == Some(vessel_ref.as_str())
                    || matches!(
                        &session.spec.source,
                        TerminalSessionSource::Agent { context, .. } if context.vessel_ref == vessel_ref && context.convoy == convoy
                    ))
        });
        Ok(CrewRoutingContext {
            command_context: CrewCommandContext {
                crew_id: None,
                namespace: Some(namespace),
                convoy: Some(convoy.clone()),
                vessel_ref: Some(vessel_ref),
                role: Some(role),
            },
            session_name: caller.map(|session| session.metadata.name.clone()),
            convoy,
        })
    }

    async fn resolve_crew_context(&self, requested: &CrewCommandContext) -> Result<ResolvedCrewContext, String> {
        let routing = self.resolve_crew_routing_context(requested).await?;
        self.resolve_crew_context_from_routing(&routing).await
    }

    async fn resolve_crew_context_from_routing(&self, routing: &CrewRoutingContext) -> Result<ResolvedCrewContext, String> {
        let namespace = routing.command_context.namespace.as_ref().expect("routing context always has namespace").clone();
        let convoy = routing.command_context.convoy.as_ref().expect("routing context always has convoy").clone();
        let vessel_ref = routing.command_context.vessel_ref.as_ref().expect("routing context always has vessel ref").clone();
        let role = routing.command_context.role.as_ref().expect("routing context always has role").clone();
        let caller = match routing.session_name.as_ref() {
            Some(name) => self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).get(name).await.ok(),
            None => None,
        };
        self.resolved_crew_context(namespace, convoy, vessel_ref, role, caller).await
    }

    pub async fn mark_crew_completion_pending(
        &self,
        namespace: &str,
        session_name: &str,
        pending: CrewCompletionPending,
    ) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::MarkCompletionPending { pending },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub async fn clear_crew_completion_pending(&self, namespace: &str, session_name: &str) -> Result<(), String> {
        apply_resource_status_patch(
            &self.resource_backend.clone().using::<ResourceTerminalSession>(namespace),
            session_name,
            &TerminalSessionStatusPatch::ClearCompletionPending,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    pub async fn pending_crew_completions(&self) -> Result<Vec<(String, CrewCompletionPending, CrewCommandContext)>, String> {
        let namespace = self.provisioning_namespace().await;
        let sessions =
            self.resource_backend.clone().using::<ResourceTerminalSession>(&namespace).list().await.map_err(|error| error.to_string())?;
        Ok(sessions
            .items
            .into_iter()
            .filter_map(|session| {
                let pending = session.status.as_ref()?.completion_pending.clone()?;
                let TerminalSessionSource::Agent { context, .. } = &session.spec.source else { return None };
                Some((session.metadata.name, pending, CrewCommandContext {
                    crew_id: None,
                    namespace: Some(context.namespace.clone()),
                    convoy: Some(context.convoy.clone()),
                    vessel_ref: Some(context.vessel_ref.clone()),
                    role: Some(session.spec.role),
                }))
            })
            .collect())
    }

    async fn resolved_crew_context(
        &self,
        namespace: String,
        convoy: String,
        vessel_ref: String,
        caller_role: String,
        caller_session: Option<flotilla_resources::ResourceObject<ResourceTerminalSession>>,
    ) -> Result<ResolvedCrewContext, String> {
        let workspace = self.resource_backend.clone().using::<Vessel>(&namespace).get(&vessel_ref).await.map_err(|err| err.to_string())?;
        if workspace.spec.convoy_ref != convoy {
            return Err(format!("vessel `{vessel_ref}` does not belong to convoy `{convoy}`"));
        }
        Ok(ResolvedCrewContext::builder()
            .namespace(namespace)
            .convoy(convoy)
            .vessel_ref(vessel_ref)
            .vessel(workspace.spec.vessel_name)
            .caller_role(caller_role)
            .maybe_caller_session(caller_session)
            .build())
    }

    pub async fn crew_list_internal(&self, requested: &CrewCommandContext) -> Result<CrewListResponse, String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let task = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().find(|vessel| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&context.namespace);
        let by_role: HashMap<_, _> = sessions
            .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|session| (session.spec.role.clone(), session))
            .collect();
        let members = task
            .crew
            .iter()
            .map(|process| {
                let session = by_role.get(&process.role);
                let work_unsettled = convoy
                    .status
                    .as_ref()
                    .and_then(|status| status.crew_work.get(&context.vessel))
                    .and_then(|crew| crew.get(&process.role))
                    .is_none_or(|work| crew_work_unsettled(work.phase));
                let state = match session.and_then(|session| session.status.as_ref().map(|status| status.phase)) {
                    Some(ResourceTerminalSessionPhase::Starting) => "starting",
                    Some(ResourceTerminalSessionPhase::Running) => "active",
                    Some(ResourceTerminalSessionPhase::Stopped) => "stopped",
                    Some(ResourceTerminalSessionPhase::Failed) => "failed",
                    None if matches!(process.source, CrewSource::Agent { .. }) => "latent",
                    None => "pending",
                };
                let crew = session.and_then(|session| session.status.as_ref()).and_then(|status| status.crew.as_ref());
                CrewListMember::builder()
                    .role(process.role.clone())
                    .kind(if matches!(process.source, CrewSource::Agent { .. }) { "agent" } else { "tool" }.to_string())
                    .state(state.to_string())
                    .maybe_attention(crew_attention(session.and_then(|session| session.status.as_ref()), work_unsettled, Utc::now()))
                    .maybe_adapter(crew.map(|crew| crew.adapter.clone()))
                    .maybe_model(crew.and_then(|crew| crew.model.clone()))
                    .maybe_stance(crew.map(|crew| crew.stance.clone()))
                    .build()
            })
            .collect();
        Ok(CrewListResponse::builder()
            .convoy(context.convoy)
            .vessel_ref(context.vessel_ref)
            .vessel(context.vessel)
            .members(members)
            .build())
    }

    pub async fn crew_complete_internal(&self, requested: &CrewCommandContext, message: Option<String>) -> Result<(), String> {
        self.crew_complete_with_disposition_internal(requested, message, None, None).await
    }

    pub async fn crew_complete_with_disposition_internal(
        &self,
        requested: &CrewCommandContext,
        message: Option<String>,
        disposition: Option<String>,
        decision_ledger_ref: Option<String>,
    ) -> Result<(), String> {
        if decision_ledger_ref.as_deref().is_some_and(|reference| !(reference.starts_with("https://") || reference.starts_with("http://")))
        {
            return Err("decision ledger reference must use an HTTP(S) URL".to_string());
        }
        let routing = self.resolve_crew_routing_context(requested).await?;
        let namespace = routing.command_context.namespace.as_deref().expect("resolved crew routing context has a namespace");
        let convoy_name = routing.command_context.convoy.as_deref().expect("resolved crew routing context has a convoy");
        let message_lock = self.convoy_message_lock(namespace, convoy_name).await;
        let _message_guard = message_lock.lock().await;
        let context = self.resolve_crew_context_from_routing(&routing).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(convoy_name).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        if let Some(pending) = convoy
            .status
            .as_ref()
            .and_then(|status| status.pending_brief())
            .filter(|pending| pending.vessel == context.vessel && pending.role == context.caller_role)
        {
            let session_name = routing.session_name.as_deref().ok_or_else(|| {
                format!("pending brief target `{}/{}` has no intact terminal session", context.vessel, context.caller_role)
            })?;
            let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
            let session = sessions.get(session_name).await.map_err(|err| err.to_string())?;
            queue_pending_crew_message(&sessions, &session, &pending.content).await?;
            apply_resource_status_patch(
                &convoys,
                convoy_name,
                &convoy_external_patches::deliver_pending_brief(
                    context.vessel,
                    context.caller_role,
                    chrono::Utc::now(),
                    pending.content.clone(),
                    message,
                    disposition,
                    decision_ledger_ref,
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            self.clear_crew_completion_pending(namespace, session_name).await?;
            return Ok(());
        }
        apply_resource_status_patch(
            &convoys,
            convoy_name,
            &convoy_external_patches::mark_crew_completed(
                context.vessel,
                context.caller_role,
                chrono::Utc::now(),
                message,
                disposition,
                decision_ledger_ref,
            ),
        )
        .await
        .map_err(|err| err.to_string())?;
        if let Some(session_name) = routing.session_name {
            self.clear_crew_completion_pending(namespace, &session_name).await?;
        }
        Ok(())
    }

    pub async fn crew_fail_internal(&self, requested: &CrewCommandContext, message: String) -> Result<(), String> {
        self.apply_crew_work_patch(requested, |context| {
            convoy_external_patches::mark_crew_failed(context.vessel.clone(), context.caller_role.clone(), chrono::Utc::now(), message)
        })
        .await
    }

    async fn runner_for_resource_checkout(&self, _checkout: &ResourceObject<ResourceCheckout>) -> Result<Arc<dyn CommandRunner>, String> {
        self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())
    }

    pub async fn verify_convoy_teardown_gate(&self, namespace: &str, name: &str, force: bool) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }

        let checkout_sources =
            self.resource_backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|err| err.to_string())?;
        let checkout_list = flotilla_resources::select_convoy_children(&convoy, &checkout_sources.items).into_values().collect::<Vec<_>>();
        self.verify_convoy_teardown_gate_for_checkouts(&convoy, &checkout_list, false).await
    }

    async fn cascade_convoy_children(&self, namespace: &str, name: &str) -> Result<(), String> {
        let selector = BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]);
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourcePresentation>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<Vessel>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceTerminalSession>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        delete_lifecycle_owned_matching(&self.resource_backend.clone().using::<ResourceCheckout>(namespace), &selector)
            .await
            .map_err(|error| error.to_string())?;
        let demands = self.resource_backend.clone().using::<ResourceDemand>(namespace);
        for demand in demands.list().await.map_err(|error| error.to_string())?.items {
            let target = &demand.spec.originating_work_ref;
            if target.api_version == flotilla_resources::api_version(ResourceConvoy::API_PATHS)
                && target.kind == ResourceConvoy::API_PATHS.kind
                && target.namespace == namespace
                && target.name == name
            {
                match demands.delete(&demand.metadata.name).await {
                    Ok(()) | Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
        Ok(())
    }

    pub async fn verify_convoy_teardown_gate_for_checkouts(
        &self,
        convoy: &ResourceObject<ResourceConvoy>,
        checkout_list: &[ResourceObject<ResourceCheckout>],
        force: bool,
    ) -> Result<(), String> {
        if force {
            return Ok(());
        }
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        if convoy.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Abandoned) {
            return Ok(());
        }
        let expected = flotilla_resources::expected_checkout_refs(convoy)?;
        if expected.is_empty() {
            return Ok(());
        }
        // Once the convoy sanctions checkout reclaim (Landed, or the convoy is
        // being deleted), the checkout authority's `OwnerTerminal` cascade may
        // legitimately collect an expected checkout before this gate runs. An
        // absent or already-deleting expected checkout under that sanction is
        // evidence of completed reclaim, not missing evidence — refusing here
        // would wedge vessel reclaim forever on a deletion the substrate itself
        // authorized. Outside the sanction, absence stays a hard refusal.
        let reclaim_sanctioned = flotilla_resources::convoy_sanctions_checkout_reclaim(convoy);
        let missing = expected
            .iter()
            .filter(|checkout_name| !checkout_list.iter().any(|checkout| &checkout.metadata.name == *checkout_name))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() && !reclaim_sanctioned {
            return Err(format!(
                "convoy {namespace}/{name} is not safe to delete: missing checkout integration evidence for {}",
                missing.join(", ")
            ));
        }

        let mut refusals = Vec::new();
        for checkout in checkout_list
            .iter()
            .filter(|checkout| expected.contains(&checkout.metadata.name))
            .filter(|checkout| !(reclaim_sanctioned && checkout.metadata.deletion_timestamp.is_some()))
        {
            let is_adopted = checkout.metadata.lifecycle_authority().map_err(|err| err.to_string())? == Some(LifecycleAuthority::Adopted);
            let Some(integration) = checkout.status.as_ref().map(|status| &status.integration) else {
                refusals.push(format!("{}: integration evidence is missing", checkout.metadata.name));
                continue;
            };
            let required = if is_adopted {
                vec![("Landed", &integration.landed)]
            } else {
                vec![("Clean", &integration.clean), ("Pushed", &integration.pushed), ("Landed", &integration.landed)]
            };
            let stale = required
                .iter()
                .filter_map(|(label, condition)| (!integration_condition_is_fresh(condition, self.clock.now())).then_some(*label))
                .collect::<Vec<_>>();
            if !stale.is_empty() {
                refusals.push(format!("{}: {} evidence is missing or stale", checkout.metadata.name, stale.join(", ")));
                continue;
            }
            if is_adopted {
                if !condition_is_true(&integration.landed) {
                    refusals.push(
                        checkout_integration_summary(checkout, integration)
                            .unwrap_or_else(|| format!("{}: Landed is not verified", checkout.metadata.name)),
                    );
                }
                continue;
            }
            if !(condition_is_true(&integration.clean) && condition_is_true(&integration.pushed) && condition_is_true(&integration.landed))
            {
                if let Some(summary) = checkout_integration_summary(checkout, integration) {
                    refusals.push(summary);
                }
            }
        }
        if refusals.is_empty() {
            Ok(())
        } else {
            refusals.sort();
            Err(format!("convoy {namespace}/{name} is not safe to delete:\n{}", refusals.join("\n")))
        }
    }

    async fn archive_convoy_checkouts_best_effort(&self, namespace: &str, name: &str) -> Result<(), String> {
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let checkout_list = checkouts
            .list_matching_labels(&BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string())]))
            .await
            .map_err(|err| err.to_string())?
            .items;
        for checkout in checkout_list {
            let Some(path) = checkout_path(&checkout) else {
                continue;
            };
            let Ok(runner) = self.runner_for_resource_checkout(&checkout).await else {
                continue;
            };
            let output = runner.run_output("git", &["push", "-u", "origin", "HEAD"], Path::new(path), &ChannelLabel::Default).await;
            if let Ok(output) = output {
                if !output.success {
                    warn!(checkout = %checkout.metadata.name, stderr = %output.stderr.trim(), "best-effort abandon archive push failed");
                }
            }
        }
        Ok(())
    }

    async fn abandon_convoy_internal(&self, namespace: &str, name: &str, reason: &str) -> Result<(), String> {
        if reason.trim().is_empty() {
            return Err("convoy abandon requires a non-empty reason".to_string());
        }
        self.archive_convoy_checkouts_best_effort(namespace, name).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        apply_resource_status_patch(
            &convoys,
            name,
            &convoy_external_patches::mark_convoy_abandoned(Utc::now(), WorkCompletionAuthority::HumanOverride, reason.to_string()),
        )
        .await
        .map_err(|err| err.to_string())?;
        // Abandonment is an explicit terminal override: the phase stamp above is
        // the teardown gate, after the best-effort archive push has run. The
        // lifecycle reconciler reclaims children while retaining the convoy.
        Ok(())
    }

    async fn apply_crew_work_patch(
        &self,
        requested: &CrewCommandContext,
        patch: impl FnOnce(&ResolvedCrewContext) -> ConvoyStatusPatch,
    ) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        ensure_crew_work_is_defined(&convoy, &context)?;
        apply_resource_status_patch(&convoys, &context.convoy, &patch(&context)).await.map(|_| ()).map_err(|err| err.to_string())
    }

    pub async fn crew_handoff_internal(&self, requested: &CrewCommandContext, target: &str, message: &str) -> Result<(), String> {
        let context = self.resolve_crew_context(requested).await?;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&context.namespace);
        let convoy = convoys.get(&context.convoy).await.map_err(|err| err.to_string())?;
        let (task_index, task) = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .and_then(|snapshot| snapshot.vessels.iter().enumerate().find(|(_, vessel)| vessel.name == context.vessel))
            .ok_or_else(|| format!("vessel `{}` is missing from convoy `{}`", context.vessel, context.convoy))?;
        let (process_index, process) = task
            .crew
            .iter()
            .enumerate()
            .find(|(_, process)| process.role == target && target != context.caller_role)
            .ok_or_else(|| crew_handoff_address_error(target, &context.vessel))?;
        let CrewSource::Agent { selector, prompt, brief_template } = &process.source else {
            return Err(format!("crew target `{target}` is a tool process and cannot receive a handoff"));
        };
        let repository_refs = task
            .repository_refs
            .clone()
            .unwrap_or_else(|| convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect());
        if convoy
            .status
            .as_ref()
            .and_then(|status| status.crew_work.get(&context.vessel))
            .and_then(|crew| crew.get(target))
            .is_some_and(|state| state.phase == flotilla_resources::CrewWorkPhase::Failed)
        {
            return Err(format!("crew target `{target}` has failed work and cannot receive a handoff"));
        }

        let delivered_message = crew_handoff_message(&context, message);
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&context.namespace);
        let identity = TerminalSessionIdentity::builder()
            .vessel_ref(context.vessel_ref.clone())
            .convoy(context.convoy.clone())
            .vessel(context.vessel.clone())
            .role(target.to_string())
            .vessel_index(task_index)
            .crew_index(process_index)
            .labels(process.labels.clone())
            .build();
        let terminal_name = identity.name();
        let handoff_result = match sessions.get(&terminal_name).await {
            Ok(existing) => match existing.status.as_ref().map(|status| status.phase) {
                Some(ResourceTerminalSessionPhase::Running) => queue_pending_crew_message(&sessions, &existing, &delivered_message).await,
                Some(ResourceTerminalSessionPhase::Stopped) => {
                    queue_pending_crew_message(&sessions, &existing, &delivered_message).await?;
                    apply_resource_status_patch(&sessions, &terminal_name, &TerminalSessionStatusPatch::MarkStarting)
                        .await
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                }
                Some(ResourceTerminalSessionPhase::Failed) => {
                    Err(format!("crew target `{target}` failed provisioning and cannot be revived"))
                }
                Some(ResourceTerminalSessionPhase::Starting) | None => {
                    queue_pending_crew_message(&sessions, &existing, &delivered_message).await
                }
            },
            Err(ResourceError::NotFound { .. }) => {
                let anchor = if let Some(caller) = context.caller_session.as_ref() {
                    caller.clone()
                } else {
                    sessions
                        .list_matching_labels(&BTreeMap::from([(VESSEL_REF_LABEL.to_string(), context.vessel_ref.clone())]))
                        .await
                        .map_err(|err| err.to_string())?
                        .items
                        .into_iter()
                        .next()
                        .ok_or_else(|| format!("vessel `{}` has no active session to anchor the handoff", context.vessel_ref))?
                };
                let current = self.crew_list_internal(requested).await?;
                let repo_roots = crew_brief_repo_roots(&self.resource_backend, &context.namespace, &convoy, &repository_refs).await;
                let repositories = self.resource_backend.clone().using::<Repository>(&context.namespace);
                let mut fork_stance = false;
                for repository_ref in &repository_refs {
                    if let Ok(repository) = repositories.get(&repository_ref.to_string()).await {
                        fork_stance |= repository.spec.is_fork();
                    }
                }
                let render_options = crate::agent_adapter::CrewBriefTemplateResolver::with_config_dir(self.config.base_path().as_path())
                    .render_options_with_fork_stance(
                        brief_template.as_deref(),
                        convoy.spec.project_ref.as_deref(),
                        repo_roots,
                        fork_stance,
                    );
                let brief =
                    handoff_crew_brief(&context, &convoy, target, prompt.as_deref(), &current.members, &repository_refs, &render_options)?;
                let terminal_meta = terminal_meta_with_vessel_credentials(identity.input_meta(), task);
                sessions
                    .create(&terminal_meta, &flotilla_resources::TerminalSessionSpec {
                        env_ref: anchor.spec.env_ref,
                        role: target.to_string(),
                        source: TerminalSessionSource::Agent {
                            selector: selector.clone(),
                            brief,
                            context: Box::new(TerminalCrewContext {
                                namespace: context.namespace.clone(),
                                convoy: context.convoy.clone(),
                                vessel_ref: context.vessel_ref.clone(),
                            }),
                            message: Some(pending_crew_message(&delivered_message)),
                        },
                        cwd: anchor.spec.cwd,
                        pool: anchor.spec.pool,
                    })
                    .await
                    .map(|_| ())
                    .map_err(|err| err.to_string())
            }
            Err(err) => Err(err.to_string()),
        };
        handoff_result?;
        apply_resource_status_patch(
            &convoys,
            &context.convoy,
            &convoy_external_patches::handoff_crew_work(
                context.vessel,
                context.caller_role,
                target.to_string(),
                chrono::Utc::now(),
                message.to_string(),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
    }

    pub async fn convoy_resume_internal(
        &self,
        namespace: &str,
        name: &str,
        prompt: &str,
        requested_vessel: Option<&str>,
        requested_role: Option<&str>,
    ) -> Result<ConvoyResumeOutcome, String> {
        if prompt.trim().is_empty() {
            return Err("convoy resume requires a non-empty prompt".to_string());
        }
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept a brief", status.phase));
        }
        let candidates = status
            .crew_work
            .iter()
            .flat_map(|(vessel, crew)| crew.iter().map(move |(role, state)| (vessel, role, state)))
            .filter(|(vessel, role, state)| {
                matches!(state.phase, flotilla_resources::CrewWorkPhase::Working | flotilla_resources::CrewWorkPhase::Done)
                    && requested_vessel.is_none_or(|requested| requested == vessel.as_str())
                    && requested_role.is_none_or(|requested| requested == role.as_str())
            })
            .map(|(vessel, role, _)| (vessel.clone(), role.clone()))
            .collect::<Vec<_>>();
        let (vessel, role) = match candidates.as_slice() {
            [] => {
                let scope = match (requested_vessel, requested_role) {
                    (Some(vessel), Some(role)) => format!(" for vessel `{vessel}` role `{role}`"),
                    (Some(vessel), None) => format!(" for vessel `{vessel}`"),
                    (None, Some(role)) => format!(" for role `{role}`"),
                    (None, None) => String::new(),
                };
                return Err(format!("convoy `{name}` has no active or completed crew work{scope}"));
            }
            [candidate] => candidate.clone(),
            _ => {
                let matches = candidates.iter().map(|(vessel, role)| format!("{vessel}/{role}")).collect::<Vec<_>>().join(", ");
                return Err(format!(
                    "convoy `{name}` has multiple active or completed crew members ({matches}); select one with --vessel and --role"
                ));
            }
        };

        let crew_phase = status
            .crew_work
            .get(&vessel)
            .and_then(|crew| crew.get(&role))
            .map(|state| state.phase)
            .expect("selected candidate has crew work");
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let session = sessions
            .list_matching_labels(&BTreeMap::from([
                (CONVOY_LABEL.to_string(), name.to_string()),
                (VESSEL_LABEL.to_string(), vessel.clone()),
                (ROLE_LABEL.to_string(), role.clone()),
            ]))
            .await
            .map_err(|err| err.to_string())
            .map(|list| list.items.into_iter().next());
        let at_turn_boundary = session.as_ref().ok().and_then(Option::as_ref).is_some_and(|session| {
            session.status.as_ref().is_some_and(|status| {
                status.phase == ResourceTerminalSessionPhase::Running
                    && status.attention.as_ref().is_some_and(|attention| attention.state == TerminalAttentionState::Idle)
            })
        });
        if crew_phase == flotilla_resources::CrewWorkPhase::Working && !at_turn_boundary {
            let displaced = status.pending_brief().map(|brief| brief.content.clone());
            apply_resource_status_patch(
                &convoys,
                name,
                &convoy_external_patches::set_pending_brief(
                    PendingBrief::builder().vessel(vessel).role(role).content(prompt.to_string()).queued_at(chrono::Utc::now()).build(),
                ),
            )
            .await
            .map_err(|err| err.to_string())?;
            return Ok(ConvoyResumeOutcome::Queued { displaced });
        }

        let displaced =
            status.pending_brief().filter(|brief| brief.vessel == vessel && brief.role == role).map(|brief| brief.content.clone());
        let session = session?.ok_or_else(|| format!("crew member `{role}` on vessel `{vessel}` has no intact terminal session"))?;
        match session.status.as_ref().map(|status| status.phase) {
            Some(ResourceTerminalSessionPhase::Running) => queue_pending_crew_message(&sessions, &session, prompt).await?,
            Some(ResourceTerminalSessionPhase::Stopped) => {
                queue_pending_crew_message(&sessions, &session, prompt).await?;
                apply_resource_status_patch(&sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting)
                    .await
                    .map_err(|err| err.to_string())?;
            }
            Some(ResourceTerminalSessionPhase::Starting) | None => queue_pending_crew_message(&sessions, &session, prompt).await?,
            Some(ResourceTerminalSessionPhase::Failed) => {
                return Err(format!("crew member `{role}` on vessel `{vessel}` failed provisioning and cannot be resumed"));
            }
        }
        apply_resource_status_patch(
            &convoys,
            name,
            &convoy_external_patches::resume_crew_work(vessel, role, chrono::Utc::now(), prompt.to_string()),
        )
        .await
        .map(|_| ConvoyResumeOutcome::Delivered { displaced })
        .map_err(|err| err.to_string())
    }

    pub async fn convoy_withdraw_pending_brief_internal(&self, namespace: &str, name: &str) -> Result<Option<String>, String> {
        let message_lock = self.convoy_message_lock(namespace, name).await;
        let _message_guard = message_lock.lock().await;
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let convoy = convoys.get(name).await.map_err(|err| err.to_string())?;
        let status = convoy.status.as_ref().ok_or_else(|| format!("convoy `{name}` has no status"))?;
        if status.phase.is_terminal() {
            return Err(format!("convoy `{name}` is in terminal phase `{:?}` and cannot accept message changes", status.phase));
        }
        let withdrawn = status.pending_brief().map(|brief| brief.content.clone());
        if withdrawn.is_some() {
            apply_resource_status_patch(&convoys, name, &convoy_external_patches::clear_pending_brief())
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(withdrawn)
    }

    async fn convoy_message_lock(&self, namespace: &str, name: &str) -> ConvoyMessageLock {
        let key = (namespace.to_string(), name.to_string());
        let mut locks = self.convoy_message_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        }
    }

    async fn deliver_standing_turn(&self, request: &crate::leaf_engine::TurnDeliveryRequest) -> Result<TurnDeliveryRung, String> {
        let sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(&request.namespace);
        let session = sessions
            .list_matching_labels(&BTreeMap::from([
                (CONVOY_LABEL.to_string(), request.convoy.clone()),
                (VESSEL_LABEL.to_string(), request.vessel.clone()),
                (ROLE_LABEL.to_string(), request.role.clone()),
            ]))
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .next()
            .ok_or_else(|| format!("turn-delivery target {}/{} has no durable terminal-session record", request.vessel, request.role))?;
        let mut spec = session.spec.clone();
        let TerminalSessionSource::Agent { brief, message, .. } = &mut spec.source else {
            return Err(format!("turn-delivery target {}/{} is not an agent", request.vessel, request.role));
        };
        let delivery_message =
            TerminalCrewMessage { id: format!("turn-delivery:{}:{}", request.source, request.head_sha), text: request.brief.clone() };
        let plan = turn_delivery_session_plan(session.status.as_ref().map(|status| status.phase), &request.vessel, &request.role)?;
        match plan {
            TurnDeliverySessionPlan::QueueWarm | TurnDeliverySessionPlan::QueueFresh => {
                *message = Some(delivery_message);
            }
            TurnDeliverySessionPlan::RestartFresh => {
                brief.content = request.brief.clone();
                *message = None;
            }
        }
        sessions
            .update(&input_meta_from_resource(&session), &session.metadata.resource_version, &spec)
            .await
            .map_err(|error| error.to_string())?;
        match plan {
            TurnDeliverySessionPlan::QueueWarm => Ok(TurnDeliveryRung::WarmSession),
            TurnDeliverySessionPlan::RestartFresh => {
                apply_resource_status_patch(&sessions, &session.metadata.name, &TerminalSessionStatusPatch::MarkStarting)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(TurnDeliveryRung::FreshAgent)
            }
            TurnDeliverySessionPlan::QueueFresh => Ok(TurnDeliveryRung::FreshAgent),
        }
    }

    async fn execute_turn_delivery_hold(
        &self,
        request: &crate::leaf_engine::TurnDeliveryRequest,
        act: &HoldAct,
        reason: &str,
    ) -> Result<(), String> {
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&request.namespace);
        let convoy = convoys.get(&request.convoy).await.map_err(|error| error.to_string())?;
        let bound = convoy.spec.change_request.as_ref().ok_or_else(|| "turn-delivery convoy has no bound change request".to_string())?;
        let repository = convoy
            .spec
            .repositories
            .iter()
            .find(|repository| repository.repo_ref == bound.repository_ref)
            .ok_or_else(|| format!("bound repository {} is absent", bound.repository_ref))?;
        let canonical = flotilla_resources::canonicalize_repo_url(&repository.url)?;
        let repository_name = canonical
            .split_once("://")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once('/').map(|(_, scope)| scope))
            .ok_or_else(|| format!("cannot derive repository scope from {}", repository.url))?;
        let HoldAct::ChangeRequestComment { body } = act;
        let comment = format!("{}\n\n{}", body.trim(), reason);
        let runner = self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
        runner
            .run("gh", &["pr", "comment", &bound.id, "-R", repository_name, "--body", &comment], Path::new("/"), &ChannelLabel::Default)
            .await
            .map(|_| ())
    }

    pub async fn refresh_fleet_replicas_once(&self) -> Result<(), String> {
        let hosts = self.config.load_hosts()?;
        let namespace = self.provisioning_namespace().await;
        let runner = self.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
        let configured: HashSet<_> = hosts.hosts.values().map(|remote| HostName::new(remote.expected_host_name.clone())).collect();
        {
            let mut cache = self.fleet_replica_cache.write().await;
            cache.retain(|host, _| configured.contains(host));
        }
        for (label, remote) in &hosts.hosts {
            let host = HostName::new(remote.expected_host_name.clone());
            let multiplex = hosts.resolved_ssh_multiplex(label);
            let result = self.fetch_fleet_replica_snapshot(remote, multiplex, Arc::clone(&runner)).await;
            match result {
                Ok(parsed) => {
                    let now = Utc::now();
                    let diagnostics = parsed.diagnostics;
                    let snapshot = parsed.snapshot;
                    let snapshot_host = snapshot.host;
                    let generation = snapshot.generation;
                    let result_sets = snapshot.result_sets.clone();
                    let staleness = FleetStaleness::Fresh { last_sync: now };
                    let mut rows: Vec<_> = snapshot
                        .rows
                        .into_iter()
                        .map(|mut row| {
                            row.host = snapshot_host.clone();
                            row.staleness = staleness.clone();
                            row
                        })
                        .collect();
                    // Replica rows from current daemons already include crewless rows via local_fleet_rows.
                    // Keep result-set rows as a secondary source for direct snapshots; existing rows win.
                    append_crewless_convoy_rows(&mut rows, &namespace, &snapshot.result_sets, &snapshot_host, staleness);
                    self.fleet_replica_cache.write().await.insert(host, FleetReplicaCacheEntry {
                        rows,
                        result_sets,
                        last_sync: Some(now),
                        generation,
                        skipped_records: diagnostics.skipped_records,
                        first_parse_error: diagnostics.first_error,
                        last_error: None,
                    });
                }
                Err(message) => {
                    let mut cache = self.fleet_replica_cache.write().await;
                    cache.entry(host).and_modify(|entry| entry.last_error = Some(message.clone())).or_insert_with(|| {
                        FleetReplicaCacheEntry {
                            rows: Vec::new(),
                            result_sets: Vec::new(),
                            last_sync: None,
                            generation: None,
                            skipped_records: 0,
                            first_parse_error: None,
                            last_error: Some(message),
                        }
                    });
                }
            }
        }
        let _ = self.fleet_replica_tx.send(self.cached_fleet_replica_snapshots().await);
        Ok(())
    }

    async fn fetch_fleet_replica_snapshot(
        &self,
        remote: &RemoteHostConfig,
        multiplex: bool,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<ParsedFleetReplicaSnapshot, String> {
        let args = fleet_replica_ssh_args(remote, multiplex);
        let arg_refs: Vec<_> = args.iter().map(String::as_str).collect();
        let output = tokio::time::timeout(
            FLEET_REPLICA_REFRESH_TIMEOUT,
            runner.run_output("ssh", &arg_refs, Path::new("/"), &ChannelLabel::Default),
        )
        .await
        .map_err(|_| format!("replica snapshot timed out after {}s", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()))?
        .map_err(|err| format!("replica snapshot ssh failed: {err}"))?;
        if !output.success {
            let message = if output.stderr.trim().is_empty() { output.stdout.trim() } else { output.stderr.trim() };
            return Err(format!("replica snapshot command failed: {message}"));
        }
        parse_fleet_replica_snapshot(output.stdout.trim())
    }

    async fn local_fleet_rows(&self, namespace: &str) -> Result<(Vec<FleetListRow>, Option<String>), String> {
        let terminal_sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(namespace);
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let observed_checkouts = self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace);

        let session_list = terminal_sessions.list().await.map_err(|err| err.to_string())?;
        let host_sources =
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|err| err.to_string())?;
        let observed_generation = observed_checkouts.list().await.map_err(|err| err.to_string())?.generation;
        let result_sets = self.aggregator_projection_state().await.local_result_sets().await;
        let placement_by_convoy = result_sets
            .iter()
            .filter_map(|result_set| result_set.rows.as_convoys())
            .flatten()
            .filter_map(|convoy| {
                convoy
                    .placement_decision
                    .clone()
                    .map(|decision| ((convoy.resource.namespace.clone(), convoy.resource.name.clone()), decision))
            })
            .collect::<HashMap<_, _>>();
        let environment_map: HashMap<_, _> = environments
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|environment| (environment.metadata.name.clone(), environment))
            .collect();
        let convoy_items = convoys.list().await.map_err(|err| err.to_string())?.items;
        let convoy_addresses = convoy_items
            .iter()
            .map(|convoy| {
                let address = convoy
                    .spec
                    .project_ref
                    .as_ref()
                    .map_or_else(|| convoy.spec.role.clone(), |project| format!("{} @ {project}", convoy.spec.role));
                (convoy.metadata.name.clone(), address)
            })
            .collect::<HashMap<_, _>>();
        let work_unsettled = convoy_items
            .into_iter()
            .flat_map(|convoy| {
                let convoy_name = convoy.metadata.name;
                convoy.status.into_iter().flat_map(move |status| {
                    let convoy_name = convoy_name.clone();
                    status.crew_work.into_iter().flat_map(move |(vessel, crew)| {
                        let convoy_name = convoy_name.clone();
                        crew.into_iter()
                            .map(move |(role, work)| ((convoy_name.clone(), vessel.clone(), role), crew_work_unsettled(work.phase)))
                    })
                })
            })
            .collect::<HashMap<_, _>>();
        let mut authority_by_convoy = HashMap::new();
        for checkout in checkouts.list().await.map_err(|err| err.to_string())?.items {
            let Some(convoy) = checkout.metadata.labels.get(CONVOY_LABEL).cloned() else {
                continue;
            };
            let authority = checkout
                .metadata
                .lifecycle_authority()
                .map_err(|err| err.to_string())?
                .map(|authority| authority.as_label_value().to_string());
            if authority.is_some() {
                authority_by_convoy.insert(convoy, authority);
            }
        }

        let mut rows = Vec::new();
        for session in session_list.items {
            let labels = &session.metadata.labels;
            let convoy = labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let task = labels.get(VESSEL_LABEL).cloned();
            let role = labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = match task.as_ref() {
                Some(task) => format!("{task}/{role}"),
                None => role.clone(),
            };
            let attention = crew_attention(
                session.status.as_ref(),
                task.as_ref().and_then(|task| work_unsettled.get(&(convoy.clone(), task.clone(), role))).copied().unwrap_or(true),
                Utc::now(),
            );
            let convoy_key = (session.metadata.namespace.clone(), convoy.clone());
            let host = if let Some(host_ref) =
                environment_map.get(&session.spec.env_ref).and_then(|environment| resource_environment_host_ref(environment))
            {
                canonical_placement_host_ref_from_sources(&host_sources.items, host_ref).ok().flatten().map_or_else(
                    || {
                        if self.canonical_local_host_id().is_some_and(|local| local.as_str() == host_ref) {
                            self.host_name.clone()
                        } else {
                            HostName::new(host_ref)
                        }
                    },
                    |target| self.host_name_for_canonical_ref(&target.reference),
                )
            } else {
                self.host_name.clone()
            };
            rows.push(
                FleetListRow::builder()
                    .convoy(convoy_addresses.get(&convoy).cloned().unwrap_or_else(|| convoy.clone()))
                    .maybe_convoy_ref((convoy != "-").then_some(convoy.clone()))
                    .vessel(session.spec.env_ref.clone())
                    .maybe_authority(authority_by_convoy.get(&convoy).cloned().flatten())
                    .crew(crew)
                    .crew_state(session_status_label(session.status.as_ref().map(|status| status.phase)))
                    .maybe_attention(attention)
                    .host(host)
                    .maybe_placement_decision(placement_by_convoy.get(&convoy_key).cloned())
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .staleness(FleetStaleness::Local)
                    .build(),
            );
        }
        append_crewless_convoy_rows(&mut rows, namespace, &result_sets, &self.host_name, FleetStaleness::Local);
        rows.sort_by(|left, right| {
            (&left.convoy, left.host.as_str(), &left.vessel, &left.crew).cmp(&(
                &right.convoy,
                right.host.as_str(),
                &right.vessel,
                &right.crew,
            ))
        });
        Ok((rows, observed_generation))
    }

    pub async fn resolve_attach_command_internal(&self, reference: &str) -> Result<ResolvedAttach, String> {
        self.resolve_attach_command_on_host_internal(reference, None).await
    }

    async fn attach_project_context(&self, selector: Option<&flotilla_protocol::RepoSelector>) -> Result<Option<String>, String> {
        let Some(selector) = selector else {
            return Ok(None);
        };
        let Ok(path) = self.resolve_repo_selector(selector).await else {
            return Ok(None);
        };
        let Some(repository_key) = self.repository_keys_by_path.read().await.get(&path).cloned() else {
            return Ok(None);
        };
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let mut matches = projects
            .into_iter()
            .filter(|project| project.spec.repositories.iter().any(|repository| repository.repo == repository_key))
            .collect::<Vec<_>>();
        let repository_key = repository_key.to_string();
        let mut declaration_matches = matches
            .iter()
            .filter(|project| project.metadata.annotations.get(BOOTSTRAP_REPOSITORY_ANNOTATION) == Some(&repository_key))
            .map(|project| project.metadata.name.clone())
            .collect::<Vec<_>>();
        declaration_matches.sort();
        declaration_matches.dedup();
        if let [project] = declaration_matches.as_slice() {
            return Ok(Some(project.clone()));
        }
        matches.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
        matches.dedup_by(|left, right| left.metadata.name == right.metadata.name);
        Ok(matches.as_slice().first().filter(|_| matches.len() == 1).map(|project| project.metadata.name.clone()))
    }

    async fn resolve_live_convoy_record(&self, reference: &str, project_context: Option<&str>) -> Result<Option<LiveConvoyRecord>, String> {
        let explicit = reference.contains('@');
        let requested = if explicit {
            Some(RoleAddress::from_str(reference)?)
        } else {
            project_context.map(|project| RoleAddress { project: project.to_string(), role: reference.to_string() })
        };
        let namespace = self.provisioning_namespace().await;
        let sources =
            self.resource_backend.including_replicas::<ResourceConvoy>(&namespace).list().await.map_err(|error| error.to_string())?;
        let has_role = sources.items.iter().any(|source| source.object.spec.role == reference);
        if !explicit && requested.is_none() && !has_role {
            return Ok(None);
        }

        let role = requested.as_ref().map_or(reference, |address| address.role.as_str());
        let mut candidates = sources
            .items
            .into_iter()
            .filter(|source| {
                source.object.spec.role == role
                    && source.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                    && requested.as_ref().is_none_or(|address| source.object.spec.project_ref.as_deref() == Some(&address.project))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            (&left.object.spec.project_ref, &left.object.metadata.name).cmp(&(&right.object.spec.project_ref, &right.object.metadata.name))
        });
        match candidates.as_slice() {
            [] if explicit => Err(format!("no live convoy matches `{reference}`")),
            [] => Ok(None),
            [source] => {
                let project = source
                    .object
                    .spec
                    .project_ref
                    .clone()
                    .ok_or_else(|| format!("live convoy {} has no project identity", source.object.metadata.name))?;
                let owner_host = match &source.provenance {
                    ResourceProvenance::Local => self.host_name.clone(),
                    ResourceProvenance::Replica { origin_root, .. } => self
                        .host_registry
                        .live_routed_host_name(origin_root)
                        .await
                        .ok_or_else(|| format!("owner host for {role}@{project} is unreachable"))?,
                };
                Ok(Some(LiveConvoyRecord {
                    address: RoleAddress { project, role: role.to_string() },
                    record_name: source.object.metadata.name.clone(),
                    owner_host,
                }))
            }
            candidates => {
                let addresses = candidates
                    .iter()
                    .filter_map(|source| {
                        source
                            .object
                            .spec
                            .project_ref
                            .as_ref()
                            .map(|project| RoleAddress { project: project.clone(), role: source.object.spec.role.clone() })
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|address| address.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!("{role} is ambiguous: {addresses}"))
            }
        }
    }

    pub async fn resolve_attach_command_on_host_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
    ) -> Result<ResolvedAttach, String> {
        self.resolve_attach_with_mode_internal(reference, host, false, AttachMode::Default).await
    }

    async fn resolve_attach_with_mode_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        self.resolve_attach_with_context(reference, host, transient, mode, None).await
    }

    async fn resolve_attach_with_context(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        // Preserve validation precedence without paying to build the candidate index.
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        if let Some(record) = self.resolve_live_convoy_record(reference, project_context).await? {
            if let Some(requested) = host {
                if requested != &record.owner_host {
                    return Err(format!("no attach target matching '{reference}' on host '{requested}'"));
                }
            }
            if record.owner_host != self.host_name {
                let plan = self.recursive_attach_plan_for_remote(&record.owner_host, &record.address.to_string(), mode).await?;
                let binding = AttachBinding::builder()
                    .host(record.owner_host)
                    .namespace(self.provisioning_namespace().await)
                    .convoy(record.record_name)
                    .role(record.address.role)
                    .build();
                return Ok(ResolvedAttach { plan, binding: Some(binding) });
            }
            let index = self.attach_candidate_index().await?;
            return index.resolve(self, &record.record_name, host, transient, mode).await;
        }
        let index = self.attach_candidate_index().await?;
        index.resolve(self, reference, host, transient, mode).await
    }

    pub async fn resolve_transient_attach_command_internal(
        &self,
        reference: &str,
        host: Option<&HostName>,
    ) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        self.resolve_attach_with_mode_internal(reference, host, true, AttachMode::Default).await
    }

    pub async fn resolvable_attach_references_internal(&self, references: &[String]) -> Result<HashSet<String>, String> {
        if references.is_empty() {
            return Ok(HashSet::new());
        }
        let index = self.attach_candidate_index().await?;
        let mut resolved = HashSet::new();
        for reference in references {
            if index.resolve(self, reference, None, false, AttachMode::Default).await.is_ok() {
                resolved.insert(reference.clone());
            }
        }
        Ok(resolved)
    }

    pub async fn resolvable_attach_targets_internal(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        let index = self.attach_candidate_index().await?;
        let mut resolved = Vec::with_capacity(targets.len());
        for (reference, host) in targets {
            resolved.push(index.resolve(self, reference, Some(host), false, AttachMode::Default).await.is_ok());
        }
        Ok(resolved)
    }

    pub async fn origin_host_names_internal(&self, origins: &HashSet<NodeId>) -> HashMap<NodeId, HostName> {
        let mut hosts = HashMap::new();
        for origin in origins {
            if let Some(host) = self.host_registry.host_name_for_node(origin).await {
                hosts.insert(origin.clone(), host);
            }
        }
        hosts
    }

    async fn attach_candidate_index(&self) -> Result<AttachCandidateIndex, String> {
        let namespace = self.provisioning_namespace().await;
        let convoy_addresses = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|source| source.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .map(|source| {
                let address = convoy_address(&source.object.spec.role, source.object.spec.project_ref.as_deref());
                (source.object.metadata.name, address)
            })
            .collect::<HashMap<_, _>>();
        let durable_sessions = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let observed_sessions = self
            .observed_resource_backend
            .clone()
            .using::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let mut sessions_by_name = HashMap::new();
        let mut replicated_sessions = Vec::new();
        for session in durable_sessions {
            match session.provenance {
                ResourceProvenance::Local => {
                    sessions_by_name.insert(session.object.metadata.name.clone(), session.object);
                }
                ResourceProvenance::Replica { .. } => replicated_sessions.push(session),
            }
        }
        for session in observed_sessions {
            sessions_by_name.insert(session.metadata.name.clone(), session);
        }
        let mut candidates = Vec::new();
        for session in sessions_by_name.into_values() {
            if session.status.as_ref().map(|status| status.phase) != Some(ResourceTerminalSessionPhase::Running) {
                continue;
            }
            let convoy_address = session.metadata.labels.get(CONVOY_LABEL).and_then(|name| convoy_addresses.get(name));
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                host: self.host_name.clone(),
                target: AttachTarget::Local(Box::new(session)),
            });
        }
        let mut indexed_remote_sessions = HashSet::new();
        for replicated in replicated_sessions {
            let session = replicated.object;
            if session.status.as_ref().map(|status| status.phase) != Some(ResourceTerminalSessionPhase::Running) {
                continue;
            }
            let ResourceProvenance::Replica { origin_root, .. } = replicated.provenance else {
                unreachable!("local durable sessions were partitioned above");
            };
            let Some(host) = self.host_registry.live_routed_host_name(&origin_root).await else {
                continue;
            };
            indexed_remote_sessions.insert((host.clone(), session.metadata.name.clone()));
            let convoy_address = session.metadata.labels.get(CONVOY_LABEL).and_then(|name| convoy_addresses.get(name));
            let convoy = session.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let role = session.metadata.labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = session.metadata.labels.get(VESSEL_LABEL).map_or_else(|| role.clone(), |vessel| format!("{vessel}/{role}"));
            let row = FleetListRow::builder()
                .convoy(convoy)
                .vessel(session.spec.env_ref.clone())
                .crew(crew)
                .crew_state("running")
                .host(host.clone())
                .namespace(session.metadata.namespace.clone())
                .session(session.metadata.name.clone())
                .staleness(FleetStaleness::Local)
                .build();
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                host,
                target: AttachTarget::Replica { row: Box::new(row) },
            });
        }

        let checkout_set = self
            .aggregator_projection_state()
            .await
            .result_set_for(&flotilla_protocol::QueryId::Checkouts { scope: None })
            .await
            .expect("checkout query is always materialized");
        if let Rows::Checkouts { rows, .. } = checkout_set.rows {
            candidates.extend(rows.into_iter().filter(|row| row.for_convoy.is_none()).map(|row| AttachCandidate {
                label: format!("{} ({})", row.path, row.host),
                references: vec![row.path.clone()],
                host: row.host.clone(),
                target: AttachTarget::Checkout(Box::new(row)),
            }));
        }

        let configured_replica_hosts: HashSet<HostName> = self
            .config
            .load_hosts()
            .map(|hosts| hosts.hosts.into_values().map(|remote| HostName::new(remote.expected_host_name)).collect())
            .unwrap_or_default();
        let cache = self.fleet_replica_cache.read().await;
        for host in configured_replica_hosts {
            if let Some(entry) = cache.get(&host) {
                let independent_references = entry
                    .result_sets
                    .iter()
                    .filter_map(|result_set| result_set.rows.as_independents())
                    .flatten()
                    .filter_map(|row| row.attach.as_deref())
                    .collect::<HashSet<_>>();
                let mut indexed_sessions = HashSet::new();
                for row in &entry.rows {
                    if row.crew_state != "running" {
                        continue;
                    }
                    if let Some(session) = &row.session {
                        if indexed_remote_sessions.contains(&(row.host.clone(), session.clone())) {
                            continue;
                        }
                        if independent_references.contains(session.as_str()) {
                            continue;
                        }
                        indexed_sessions.insert(session.clone());
                    }
                    candidates.push(AttachCandidate {
                        label: fleet_row_attach_reference_label(row),
                        references: fleet_row_attach_reference_keys(row),
                        host: row.host.clone(),
                        target: AttachTarget::Replica { row: Box::new(row.clone()) },
                    });
                }
                for result_set in &entry.result_sets {
                    let Rows::Independents { scope: None, rows } = &result_set.rows else { continue };
                    for row in rows {
                        let Some(reference) = &row.attach else { continue };
                        if row.phase != flotilla_protocol::SessionPhase::Running || !indexed_sessions.insert(reference.clone()) {
                            continue;
                        }
                        let fleet_row = FleetListRow::builder()
                            .convoy("-")
                            .vessel("-")
                            .crew("-")
                            .crew_state("running")
                            .host(host.clone())
                            .namespace(row.resource.namespace.clone())
                            .session(reference.clone())
                            .staleness(FleetStaleness::Local)
                            .build();
                        candidates.push(AttachCandidate {
                            label: format!("{} ({host})", row.name),
                            references: vec![reference.clone()],
                            host: host.clone(),
                            target: AttachTarget::Replica { row: Box::new(fleet_row) },
                        });
                    }
                }
            }
        }
        drop(cache);
        Ok(AttachCandidateIndex::new(candidates))
    }

    async fn local_checkout_terminal_plan(&self, checkout: &CheckoutRow, seat: AttachMode) -> Result<ResolvedAttachPlan, String> {
        let cwd = ExecutionEnvironmentPath::new(&checkout.path);
        let discovery = discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &self.local_environment_id,
            cwd.as_path(),
        )
        .await
        .map_err(|error| format!("checkout {} provider discovery failed: {error}", checkout.path))?;
        let pool = discovery
            .registry
            .terminal_pools
            .preferred()
            .cloned()
            .ok_or_else(|| format!("no terminal pool available for checkout {}", checkout.path))?;
        let session_name = transient_checkout_session_name(checkout);
        let command = "${SHELL:-/bin/sh}";
        pool.preflight_attach(seat).await?;
        pool.ensure_session(&session_name, command, &cwd, &Vec::new(), &[]).await?;
        let args = pool.attach_args_for_mode(&session_name, command, &cwd, &Vec::new(), seat)?;
        Ok(ResolvedAttachPlan(vec![ResolvedAttachAction::Command(args)]))
    }

    /// Resolve the attach plan for a locally-known session, returning it
    /// with the host that actually owns the session (the binding host).
    async fn attach_plan_for_session(
        &self,
        reference: &str,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        seat: AttachMode,
    ) -> Result<(ResolvedAttachPlan, HostName), String> {
        let namespace = self.provisioning_namespace().await;
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(&namespace);
        let environment = environments
            .get(&session.spec.env_ref)
            .await
            .map_err(|err| format!("environment {} lookup failed: {err}", session.spec.env_ref))?;
        let host_ref = environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .ok_or_else(|| format!("environment {} has no host binding", session.spec.env_ref))?;
        let target_host = self.target_host_for_resource_ref(&namespace, host_ref).await?;
        if target_host != self.host_name {
            let plan = self.recursive_attach_plan_for_remote(&target_host, reference, seat).await?;
            return Ok((plan, target_host));
        }

        let plan = self.local_attach_plan_for_session(session, &environment, seat).await?;
        Ok((plan, self.host_name.clone()))
    }

    async fn recursive_attach_plan_for_remote(
        &self,
        target_host: &HostName,
        reference: &str,
        seat: AttachMode,
    ) -> Result<ResolvedAttachPlan, String> {
        let next_hop = self.host_registry.next_hop_host_for_target_host(target_host).await?.unwrap_or_else(|| target_host.clone());
        if next_hop == self.host_name {
            return Err(format!("unreachable next hop for host '{target_host}': route points back to local host"));
        }

        let resolver = ssh_resolver_from_config(self.config.base_path())?;
        let mut command =
            vec![flotilla_protocol::arg::Arg::Literal("flotilla".to_string()), flotilla_protocol::arg::Arg::Literal("attach".to_string())];
        command.push(flotilla_protocol::arg::Arg::Literal("--host".to_string()));
        command.push(flotilla_protocol::arg::Arg::Quoted(target_host.to_string()));
        // Recursive attaches only traverse transport boundaries; Presentation
        // Manager identity belongs to the original foreground attach.
        command.push(flotilla_protocol::arg::Arg::Literal("--transient".to_string()));
        match seat {
            AttachMode::Default => {}
            AttachMode::Strict => command.push(flotilla_protocol::arg::Arg::Literal("--strict".to_string())),
            AttachMode::Take => command.push(flotilla_protocol::arg::Arg::Literal("--take".to_string())),
        }
        command.push(flotilla_protocol::arg::Arg::Quoted(reference.to_string()));
        resolver
            .one_hop_command_args(&next_hop, command)
            .map(ResolvedAttachPlan::command)
            .map_err(|err| format!("unreachable next hop '{next_hop}' for host '{target_host}': {err}"))
    }

    pub async fn route_remote_attach_binding(&self, binding: &AttachBinding) -> Result<ResolvedAttachPlan, String> {
        let reference = binding
            .session
            .as_deref()
            .or(binding.convoy.as_deref())
            .ok_or_else(|| "remote attach binding has neither a session nor convoy reference".to_string())?;
        self.recursive_attach_plan_for_remote(&binding.host, reference, AttachMode::Default).await
    }

    async fn local_attach_plan_for_session(
        &self,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        seat: AttachMode,
    ) -> Result<ResolvedAttachPlan, String> {
        let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let registry = self.registry_for_resource_environment(environment, cwd.as_path()).await?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;
        let attach_target = terminal_session_attach_target(session)?;
        pool.preflight_attach(seat).await?;
        let attach_args = pool.attach_args_for_mode(attach_target.session_id, attach_target.launch_command, &cwd, &Vec::new(), seat)?;
        if environment.spec.docker.is_some() {
            let environment_id = EnvironmentId::new(session.spec.env_ref.clone());
            let container_name = environment.status.as_ref().and_then(|status| status.docker_container_id.as_deref());
            let container_name =
                container_name.ok_or_else(|| format!("environment {} has no docker container id", session.spec.env_ref))?;
            let environment_resolver =
                DockerEnvironmentHopResolver::new(HashMap::from([(environment_id.clone(), container_name.to_string())]));
            let hop_resolver = HopResolver::new(
                Arc::new(crate::hop_chain::remote::NoopRemoteHopResolver),
                Arc::new(environment_resolver),
                Arc::new(NoopTerminalHopResolver),
            );
            let plan = HopPlan(vec![Hop::EnterEnvironment { env_id: environment_id, provider: "docker".to_string() }, Hop::RunCommand {
                command: attach_args,
            }]);
            let mut context = ResolutionContext {
                current_host: self.host_name.clone(),
                current_environment: None,
                working_directory: Some(cwd),
                actions: Vec::new(),
                nesting_depth: 0,
            };
            return hop_resolver.resolve(&plan, &mut context).map(|resolved| ResolvedAttachPlan(resolved.0));
        }
        Ok(ResolvedAttachPlan(vec![ResolvedAttachAction::Command(attach_args)]))
    }

    async fn target_host_for_resource_ref(&self, namespace: &str, host_ref: &str) -> Result<HostName, String> {
        let canonical_ref = match canonical_placement_host_ref(&self.resource_backend, namespace, host_ref).await {
            Ok(Some(target_host)) => target_host.reference,
            Ok(None) => return Err(format!("references unknown host `{host_ref}`")),
            Err(error) => return Err(error),
        };
        Ok(self.host_name_for_canonical_ref(&canonical_ref))
    }

    pub async fn canonical_host_id_internal(&self, namespace: &str, host_ref: &str) -> Result<CanonicalHostId, String> {
        canonical_placement_host_ref(&self.resource_backend, namespace, host_ref)
            .await?
            .map(|target| target.reference)
            .ok_or_else(|| format!("references unknown host `{host_ref}`"))
    }

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.local_host_id().map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    fn host_name_for_canonical_ref(&self, canonical_ref: &CanonicalHostId) -> HostName {
        if self.canonical_local_host_id().as_ref() == Some(canonical_ref) {
            self.host_name.clone()
        } else {
            HostName::new(canonical_ref.as_str())
        }
    }

    async fn registry_for_resource_environment(
        &self,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &Path,
    ) -> Result<Arc<crate::providers::registry::ProviderRegistry>, String> {
        let environment_id = if let Some(host_direct) = environment.spec.host_direct.as_ref() {
            let canonical_ref =
                match canonical_placement_host_ref(&self.resource_backend, &environment.metadata.namespace, &host_direct.host_ref).await {
                    Ok(Some(target_host)) => target_host.reference,
                    Ok(None) => return Err(format!("references unknown host `{}`", host_direct.host_ref)),
                    Err(error) => return Err(error),
                };
            if self.canonical_local_host_id().as_ref() == Some(&canonical_ref) {
                self.local_environment_id.clone()
            } else {
                EnvironmentId::new(environment.metadata.name.clone())
            }
        } else {
            EnvironmentId::new(environment.metadata.name.clone())
        };

        if let Some(registry) = self.environment_registry_for_environment(&environment_id) {
            return Ok(registry);
        }

        discover_repo_for_environment(
            &self.environment_manager,
            &self.discovery,
            &self.config,
            &self.local_environment_id,
            &environment_id,
            cwd,
        )
        .await
        .map(|result| Arc::new(result.registry))
    }

    async fn refresh_local_host_summary(&self) -> HostSummary {
        let mut providers = crate::host_summary::provider_statuses_from_registries(
            self.repos.read().await.values().map(|state| state.preferred_root().model.registry.as_ref()),
        );
        for advertised in self.local_placement_provider_statuses.read().await.iter() {
            if !providers
                .iter()
                .any(|provider| provider.category == advertised.category && provider.implementation == advertised.implementation)
            {
                providers.push(advertised.clone());
            }
        }
        providers.sort_by(|left, right| (&left.category, &left.name).cmp(&(&right.category, &right.name)));
        let summary = crate::host_summary::build_local_host_summary(
            &self.node_id,
            &self.host_name,
            EnvironmentId::host(self.environment_manager.local_host_id().clone()),
            &self.environment_manager,
            providers,
            &*self.discovery.env,
        )
        .await;
        self.host_registry.set_local_host_summary(summary.clone()).await;
        summary
    }

    async fn get_issue_provider_for_repo(&self, repo: &Path) -> Result<(Arc<dyn IssueProvider>, flotilla_protocol::IssueSource), String> {
        let identity = self.tracked_repo_identity_for_path(repo).await.ok_or_else(|| "no tracked repo for path".to_string())?;
        let repos = self.repos.read().await;
        let state = repos.get(&identity).ok_or_else(|| "repo not found".to_string())?;
        let source = forge_issue_source(state.identity());
        let provider = state
            .registry()
            .issue_provider_for(&source)
            .ok_or_else(|| format!("no issue provider available for {} {}", source.service, source.scope))?;
        Ok((provider, source))
    }

    pub async fn execute_with_remote_executor(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
    ) -> Result<u64, String> {
        self.execute_impl(command, remote_executor, true, None).await
    }

    pub async fn execute_for_principal(&self, command: Command, principal_ref: Option<PrincipalRef>) -> Result<u64, String> {
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false, principal_ref).await
    }

    async fn executor_provider_data(&self, repo_identity: &RepoIdentity, repo_root: &Path, registry: &ProviderRegistry) -> ProviderData {
        let mut providers = ProviderData::default();

        if let Some(checkout_manager) = registry.checkout_managers.preferred() {
            match checkout_manager.list_checkouts(&ExecutionEnvironmentPath::new(repo_root)).await {
                Ok(checkouts) => {
                    for (path, mut checkout) in checkouts {
                        checkout.host_name.get_or_insert_with(|| self.host_name.clone());
                        providers
                            .checkouts
                            .insert(QualifiedPath::host(self.environment_manager.local_host_id().clone(), path.into_path_buf()), checkout);
                    }
                }
                Err(error) => warn!(repo = %repo_identity, %error, "failed to read checkouts for command execution"),
            }
        }

        let criteria = RepoCriteria { repo_slug: Some(repo_identity.path.clone()) };
        for (descriptor, agent) in registry.cloud_agents.iter() {
            match agent.list_sessions(&criteria).await {
                Ok(sessions) => providers.sessions.extend(sessions),
                Err(error) => {
                    warn!(repo = %repo_identity, provider = %descriptor.display_name, %error, "failed to read sessions for command execution")
                }
            }
        }

        providers
    }

    pub async fn execute_remote_step_batch(
        &self,
        request: RemoteStepBatchRequest,
        progress_sink: Arc<dyn RemoteStepProgressSink>,
        cancel: CancellationToken,
    ) -> Result<Vec<StepOutcome>, String> {
        let local_repo_path = self
            .preferred_local_path_for_identity(&request.repo_identity)
            .await
            .ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
        let registry = {
            let repos = self.repos.read().await;
            let state = repos.get(&request.repo_identity).ok_or_else(|| format!("repo not tracked locally: {}", request.repo_identity))?;
            state.registry()
        };
        let providers_data = Arc::new(self.executor_provider_data(&request.repo_identity, &local_repo_path, &registry).await);

        let config_base = DaemonHostPath::new(self.config.base_path().as_path());
        let attachable_store = self.discovery.shared_attachable_store(&self.config);
        let daemon_socket_path = self.daemon_socket_path.read().await.clone().map(DaemonHostPath::new);
        let resolver = executor::ExecutorStepResolver {
            repo: executor::RepoExecutionContext {
                identity: request.repo_identity.clone(),
                root: ExecutionEnvironmentPath::new(&local_repo_path),
            },
            registry,
            providers_data,
            runner: Arc::clone(&self.discovery.runner),
            env: Arc::clone(&self.discovery.env),
            config_base,
            attachable_store,
            daemon_socket_path,
            local_node_id: self.node_id.clone(),
            local_host: self.host_name.clone(),
            environment_manager: Arc::clone(&self.environment_manager),
        };

        let result = execute_local_remote_step_batch(self.node_id.clone(), request, progress_sink, cancel, &resolver).await;
        result
    }

    async fn execute_impl(
        &self,
        command: Command,
        remote_executor: Arc<dyn RemoteStepExecutor>,
        allow_remote_host: bool,
        dispatching_principal_ref: Option<PrincipalRef>,
    ) -> Result<u64, String> {
        let command_node_id = command.node_id.clone().unwrap_or_else(|| self.node_id.clone());
        debug!(
            %command_node_id, local_node = %self.node_id, %allow_remote_host,
            desc = %command.description(), "execute_impl"
        );
        if !allow_remote_host && command_node_id != self.node_id {
            return Err(format!("remote command routing not implemented yet for node {command_node_id}"));
        }

        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);

        if command.action.is_query() {
            // Query commands should be dispatched through `execute_query`,
            // not through `execute`. Return an error to surface misrouting.
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = flotilla_protocol::CommandValue::Error { message: "query commands should use execute_query, not execute".into() };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceApply { namespace, document } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match apply_resource_document(&self.resource_backend, namespace, document.clone()).await {
                Ok(applied) => flotilla_protocol::CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                    kind: applied.kind,
                    plural: applied.plural,
                    namespace: applied.namespace,
                    value: applied.value,
                    replica_origin: None,
                })),
                Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceStatusPatch { namespace, kind, name, status } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result =
                match flotilla_resources::patch_resource_status(&self.resource_backend, namespace, kind, name, status.clone()).await {
                    Ok(patched) => flotilla_protocol::CommandValue::ResourceObject(Box::new(ResourceJsonResponse {
                        kind: patched.kind,
                        plural: patched.plural,
                        namespace: patched.namespace,
                        value: patched.value,
                        replica_origin: None,
                    })),
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceDelete { namespace, kind, name, replica_origin } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = if let Some(origin_root) = replica_origin {
                let deleted = if self.peer_connection_status(origin_root).await == PeerConnectionState::Connected {
                    Err(ResourceError::invalid(format!(
                        "replica origin {origin_root} is connected; delete the authoritative resource instead"
                    )))
                } else {
                    flotilla_resources::collect_resource_replica_kind(&self.resource_backend, namespace, kind, name, origin_root).await
                };
                match deleted {
                    Ok(deleted) => flotilla_protocol::CommandValue::ResourceDeleted(Box::new(ResourceJsonResponse {
                        kind: deleted.kind,
                        plural: deleted.plural,
                        namespace: deleted.namespace,
                        value: deleted.value,
                        replica_origin: replica_origin.clone(),
                    })),
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                }
            } else {
                match flotilla_resources::delete_resource_kind(&self.resource_backend, namespace, kind, name).await {
                    Ok(deleted) => {
                        let response = Box::new(ResourceJsonResponse {
                            kind: deleted.object.kind,
                            plural: deleted.object.plural,
                            namespace: deleted.object.namespace,
                            value: deleted.object.value,
                            replica_origin: None,
                        });
                        if deleted.already_deleted {
                            flotilla_protocol::CommandValue::ResourceAlreadyDeleted(response)
                        } else {
                            flotilla_protocol::CommandValue::ResourceDeleted(response)
                        }
                    }
                    Err(error) => flotilla_protocol::CommandValue::Error { message: error.to_string() },
                }
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ResourceWatch { namespace, kind, name, include_replicas, replica_sources, cursor } =
            command.action
        {
            let repo_identity = empty_repo_identity();
            let description = format!("watch resource {namespace}/{kind}");
            let token = CancellationToken::new();
            {
                let mut guard = self.active_commands.lock().await;
                guard.insert(id, token.clone());
            }
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: command_node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: None,
                description,
            });

            let backend = self.resource_backend.clone();
            let event_tx = self.event_tx.clone();
            let active_ref = Arc::clone(&self.active_commands);
            tokio::spawn(async move {
                let result = run_resource_watch_command(
                    ResourceWatchCommandContext::builder()
                        .backend(backend)
                        .namespace(namespace)
                        .kind(kind)
                        .maybe_name(name)
                        .include_replicas(include_replicas)
                        .replica_sources(replica_sources)
                        .maybe_cursor(cursor)
                        .command_id(id)
                        .node_id(command_node_id.clone())
                        .repo_identity(repo_identity.clone())
                        .event_tx(event_tx.clone())
                        .token(token)
                        .build(),
                )
                .await;
                active_ref.lock().await.remove(&id);
                let _ = event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: command_node_id,
                    repo_identity,
                    repo: None,
                    result,
                });
            });
            return Ok(id);
        }

        if matches!(command.action, flotilla_protocol::CommandAction::Refresh { repo: None }) {
            let repo_paths = {
                let repos = self.repos.read().await;
                let order = self.repo_order.read().await;
                order
                    .iter()
                    .filter_map(|identity| repos.get(identity).map(|state| state.preferred_path().to_path_buf()))
                    .collect::<Vec<_>>()
            };
            let repo_path = repo_paths.first().cloned().unwrap_or_default();
            let repo_identity = self.tracked_repo_identity_for_path(&repo_path).await.unwrap_or_else(|| fallback_repo_identity(&repo_path));
            let description = command.description().to_string();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let mut refreshed = Vec::new();
            let mut identity_changes = Vec::new();
            let result = match async {
                for repo in &repo_paths {
                    if let Some(change) = self.refresh(&flotilla_protocol::RepoSelector::Path(repo.clone())).await? {
                        identity_changes.push(change);
                    }
                    refreshed.push(repo.clone());
                }
                Ok::<(), String>(())
            }
            .await
            {
                Ok(()) => flotilla_protocol::CommandValue::Refreshed { repos: refreshed, identity_changes },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewHandoff { context, target, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_handoff_internal(context, target, message).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyResume { namespace, name, prompt, vessel, role } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => {
                    match self.convoy_resume_internal(&namespace, &record_name, prompt, vessel.as_deref(), role.as_deref()).await {
                        Ok(ConvoyResumeOutcome::Delivered { displaced }) => {
                            flotilla_protocol::CommandValue::ConvoyBriefDelivered { displaced }
                        }
                        Ok(ConvoyResumeOutcome::Queued { displaced }) => flotilla_protocol::CommandValue::ConvoyBriefQueued { displaced },
                        Err(message) => flotilla_protocol::CommandValue::Error { message },
                    }
                }
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyWithdrawPendingBrief { namespace, name } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.convoy_withdraw_pending_brief_internal(&namespace, &record_name).await {
                    Ok(withdrawn) => flotilla_protocol::CommandValue::ConvoyBriefWithdrawn { withdrawn },
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewComplete { context, message, disposition, decision_ledger_ref } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self
                .crew_complete_with_disposition_internal(context, message.clone(), disposition.clone(), decision_ledger_ref.clone())
                .await
            {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::CrewFail { context, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let result = match self.crew_fail_internal(context, message.clone()).await {
                Ok(()) => flotilla_protocol::CommandValue::Ok,
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyDelete { namespace, name, force } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.reap_convoy_internal(&namespace, &record_name, *force).await {
                    Ok(()) => flotilla_protocol::CommandValue::Ok,
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyAbandon { namespace, name, reason } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = match namespace {
                Some(namespace) => namespace.clone(),
                None => self.provisioning_namespace().await,
            };
            let result = match resolve_local_convoy_name(&self.resource_backend, &namespace, name).await {
                Ok(record_name) => match self.abandon_convoy_internal(&namespace, &record_name, reason).await {
                    Ok(()) => flotilla_protocol::CommandValue::Ok,
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyWorkForceComplete { convoy, work, message } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let namespace = self.provisioning_namespace().await;
            let convoys = self.resource_backend.clone().using::<ResourceConvoy>(&namespace);
            let record_name = match resolve_local_convoy_name(&self.resource_backend, &namespace, convoy).await {
                Ok(record_name) => record_name,
                Err(message) => {
                    self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let check_work_is_completable = |current: &ResourceObject<ResourceConvoy>| match current.status.as_ref() {
                None => Err(ResourceError::other(format!("convoy {convoy} has no status"))),
                Some(status) => match status.work.get(work) {
                    None => Err(ResourceError::other(format!("convoy {convoy} does not contain work {work}"))),
                    Some(state)
                        if matches!(
                            state.phase,
                            flotilla_resources::WorkPhase::Failed
                                | flotilla_resources::WorkPhase::Cancelled
                                | flotilla_resources::WorkPhase::Abandoned
                        ) =>
                    {
                        Err(ResourceError::other(format!("convoy {convoy} work {work} is already terminal")))
                    }
                    Some(_) => Ok(()),
                },
            };
            let result = match apply_resource_status_patch_checked(
                &convoys,
                &record_name,
                &convoy_external_patches::force_work_completed(work.clone(), chrono::Utc::now(), message.clone()),
                check_work_is_completable,
            )
            .await
            {
                Ok(_) => flotilla_protocol::CommandValue::Ok,
                Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
            };
            self.finish_context_free_command(id, empty_identity, result);
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyStart { intent } = &command.action {
            let empty_identity = self.start_context_free_command(id, command.description().to_string());
            let default_namespace = intent.namespace.clone().unwrap_or(self.provisioning_namespace().await);
            let (namespace, intent) = match normalize_convoy_start_intent(&default_namespace, intent) {
                Ok(resolved) => resolved,
                Err(message) => {
                    self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error { message });
                    return Ok(id);
                }
            };
            let dispatching_principal_ref =
                dispatching_principal_ref.clone().unwrap_or_else(|| PrincipalRef::implicit_for_namespace(&namespace));
            let key = ConvoyStartKey::new(namespace, &intent);
            if !self.pending_convoy_starts.lock().await.insert(key.clone()) {
                self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error {
                    message: format!("convoy start for project {} is already in progress", intent.project_ref),
                });
                return Ok(id);
            }
            let task = ConvoyStartTask::builder()
                .command_id(id)
                .intent(intent)
                .key(key.clone())
                .dispatching_principal_ref(dispatching_principal_ref)
                .build();
            if let Some(daemon) = self.self_weak.upgrade() {
                tokio::spawn(async move {
                    daemon.supervise_convoy_start(task).await;
                });
            } else {
                self.pending_convoy_starts.lock().await.remove(&key);
                self.finish_context_free_command(id, empty_identity, flotilla_protocol::CommandValue::Error {
                    message: "convoy start worker is unavailable".to_string(),
                });
            }
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ConvoyCreate {
            name,
            workflow_ref,
            inputs,
            repository_url,
            r#ref,
            project_ref,
            placement_policy,
            adopted_checkout,
        } = &command.action
        {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let role = name.clone();
            let project_identity = project_ref.as_deref();
            if let Err(message) = validate_convoy_name(&role) {
                let result = flotilla_protocol::CommandValue::Error { message };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            if let Err(message) = allocate_convoy_generation(&self.resource_backend, &namespace, project_identity, &role).await {
                let result = flotilla_protocol::CommandValue::Error { message };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            let record_name = convoy_record_name();
            let name = &record_name;
            if let Err(message) = self.check_local_free_space_floor().await {
                let result = flotilla_protocol::CommandValue::Error { message };
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result,
                });
                return Ok(id);
            }
            let mut workflow = match self
                .resource_backend
                .clone()
                .including_replicas::<WorkflowTemplate>(&namespace)
                .get(workflow_ref)
                .await
                .map(|source| source.object)
                .map_err(|error| format!("workflow template {workflow_ref}: {error}"))
            {
                Ok(workflow) => workflow,
                Err(message) => {
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
            };
            let project_repositories = if let Some(project_ref) = project_ref {
                match self.snapshot_project_repositories(&namespace, project_ref).await {
                    Ok(repositories) => Some(repositories),
                    Err(message) => {
                        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.node_id.clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                None
            };
            if project_repositories.is_some() && repository_url.is_some() {
                let message = "convoy repository selection is not allowed when a project is supplied".to_string();
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let mut direct_repository_url = repository_url.clone();
            let mut r#ref = r#ref.clone();
            let adopted_checkout = match adopted_checkout {
                Some(path) => {
                    let adopted_result = async {
                        let inspection =
                            self.inspect_adopted_checkout(path.as_ref(), direct_repository_url.as_deref(), r#ref.as_deref()).await?;
                        let repo_ref = inspection.spec.key();
                        let transport_url = inspection
                            .transport_url
                            .as_deref()
                            .ok_or_else(|| "an adopted checkout requires a repository transport URL".to_string())?;
                        let git_ref = r#ref.as_deref().unwrap_or(&inspection.checkout.git_ref);
                        let _reconciliation = self.observed_checkout_reconciliation.lock().await;
                        let (checkout_ref, inferred_repository_url, inferred_ref) = create_adopted_checkout_resource(
                            &self.resource_backend,
                            &self.observed_resource_backend,
                            AdoptedCheckoutRequest::builder()
                                .namespace(&namespace)
                                .convoy_name(name)
                                .checkout_path(&inspection.checkout.path)
                                .repository_spec(&inspection.spec)
                                .repository_url(transport_url)
                                .git_ref(git_ref)
                                .host_ref(&inspection.checkout.host_ref)
                                .build(),
                        )
                        .await?;
                        Ok::<_, String>((repo_ref, checkout_ref, inferred_repository_url, inferred_ref))
                    }
                    .await;
                    match adopted_result {
                        Ok((repo_ref, checkout_ref, inferred_repository_url, inferred_ref)) => {
                            if project_repositories.is_none() {
                                direct_repository_url.get_or_insert(inferred_repository_url);
                            }
                            r#ref.get_or_insert(inferred_ref);
                            Some((repo_ref, checkout_ref))
                        }
                        Err(message) => {
                            let result = flotilla_protocol::CommandValue::Error { message };
                            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                                command_id: id,
                                node_id: self.node_id.clone(),
                                repo_identity: empty_identity,
                                repo: None,
                                result,
                            });
                            return Ok(id);
                        }
                    }
                }
                None => None,
            };
            let repositories = if let Some(repositories) = project_repositories {
                repositories
            } else if let Some(url) = direct_repository_url {
                let resolved = async {
                    let repository_spec = self.resolve_repository_remote(&url).await?;
                    let canonical_url = match repository_spec.identity() {
                        flotilla_resources::RepositoryIdentity::Remote { canonical_remote } => canonical_remote.clone(),
                        flotilla_resources::RepositoryIdentity::Local { .. } => {
                            return Err(format!("repository {url} did not resolve to a remote identity"));
                        }
                    };
                    let repo_ref = repository_spec.key();
                    let repository = flotilla_resources::ensure_repository(
                        &self.resource_backend.clone().using::<Repository>(&namespace),
                        &repo_ref,
                        &repository_spec,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let default_ref = repository
                        .status
                        .as_ref()
                        .and_then(|status| status.default_branch.clone())
                        .or_else(|| if adopted_checkout.is_some() { r#ref.clone() } else { None })
                        .ok_or_else(|| format!("repository {repo_ref} has no resolved default branch"))?;
                    let workspace_slug = flotilla_resources::repository_workspace_slugs([(&repo_ref, &repository_spec)])
                        .remove(&repo_ref)
                        .expect("repository slug should resolve");
                    Ok::<_, String>(vec![ConvoyRepositorySpec {
                        url: canonical_url,
                        repo_ref,
                        source_ref: default_ref.clone(),
                        target_ref: default_ref,
                        workspace_slug,
                        subpaths: Vec::new(),
                    }])
                }
                .await;
                match resolved {
                    Ok(repositories) => repositories,
                    Err(message) => {
                        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.node_id.clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                }
            } else {
                Vec::new()
            };
            let adopted_checkout_ref_to_cleanup = adopted_checkout.as_ref().map(|(_, checkout_ref)| checkout_ref.clone());
            let mut adopted_checkout_refs = BTreeMap::new();
            if let Some((repo_ref, checkout_ref)) = adopted_checkout {
                if !repositories.iter().any(|repository| repository.repo_ref == repo_ref) {
                    let message =
                        format!("adopted checkout repository {repo_ref} is not part of project {}", project_ref.as_deref().unwrap_or(""));
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
                adopted_checkout_refs.insert(repo_ref, checkout_ref);
            }
            if let Err(message) =
                resolve_workflow_credentials(&self.resource_backend, &namespace, project_ref.as_deref(), &repositories, &mut workflow.spec)
                    .await
            {
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement = match self.resolve_and_validate_convoy_placement(&namespace, &workflow.spec, placement_policy.as_deref()).await
            {
                Ok(placement) => placement,
                Err(message) => {
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result: flotilla_protocol::CommandValue::Error { message },
                    });
                    return Ok(id);
                }
            };
            let placement_decision = match placement.selected.as_ref() {
                Some(selected) => match placement_target_host(&self.resource_backend, &namespace, selected).await {
                    Ok(target_host) => Some(PlacementDecision {
                        policy_name: selected.metadata.name.clone(),
                        target_host,
                        refused_candidates: placement.refused_candidates,
                        viable_not_selected: placement.viable_not_selected,
                    }),
                    Err(message) => {
                        let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                            command_id: id,
                            node_id: self.node_id.clone(),
                            repo_identity: empty_identity,
                            repo: None,
                            result: flotilla_protocol::CommandValue::Error { message },
                        });
                        return Ok(id);
                    }
                },
                None => None,
            };
            if let Err(message) = self.check_remote_placement_free_space_floor(&namespace, placement_decision.as_ref()).await {
                let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                    command_id: id,
                    node_id: self.node_id.clone(),
                    repo_identity: empty_identity,
                    repo: None,
                    result: flotilla_protocol::CommandValue::Error { message },
                });
                return Ok(id);
            }
            let placement_policy = placement.selected.as_ref().map(|placement| placement.metadata.name.clone());
            let _admission_guard = self.convoy_admission.lock().await;
            let generation = match allocate_convoy_generation(&self.resource_backend, &namespace, project_identity, &role).await {
                Ok(generation) => generation,
                Err(message) => {
                    if let Some(checkout_ref) = adopted_checkout_ref_to_cleanup {
                        if let Err(error) = self.resource_backend.clone().using::<ResourceCheckout>(&namespace).delete(&checkout_ref).await
                        {
                            warn!(%error, %checkout_ref, "failed to clean up adopted checkout after convoy identity conflict");
                        }
                    }
                    let result = flotilla_protocol::CommandValue::Error { message };
                    let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: self.node_id.clone(),
                        repo_identity: empty_identity,
                        repo: None,
                        result,
                    });
                    return Ok(id);
                }
            };
            let spec = ConvoySpec {
                workflow_ref: workflow_ref.clone(),
                role: role.clone(),
                generation,
                dispatching_principal_ref: dispatching_principal_ref
                    .clone()
                    .unwrap_or_else(|| PrincipalRef::implicit_for_namespace(&namespace)),
                inputs: inputs.iter().map(|(k, v)| (k.clone(), InputValue::String(v.clone()))).collect(),
                placement_policy,
                repositories,
                r#ref,
                project_ref: project_ref.clone(),
                adopted_checkout_refs,
                issues: Vec::new(),
                change_request: None,
                instruction: None,
            };
            let result = match self
                .create_convoy_with_workflow_snapshot(
                    &namespace,
                    name,
                    ConvoySnapshotBundle::builder()
                        .spec(&spec)
                        .workflow(&workflow.spec)
                        .maybe_placement(placement.selected.as_ref().map(|placement| &placement.spec))
                        .maybe_placement_decision(placement_decision)
                        .build(),
                    ConvoyDispatchRegard::Emit,
                )
                .await
            {
                Ok(()) => flotilla_protocol::CommandValue::ConvoyCreated { name: convoy_address(&role, project_identity) },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::WorkflowTemplateApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let templates = self.resource_backend.clone().using::<WorkflowTemplate>(&namespace);
            let result = match parse_and_validate_workflow_template_yaml(spec_yaml) {
                Ok(spec) => {
                    let meta = InputMeta::builder().name(name.clone()).build();
                    let outcome = match templates.get(name).await {
                        Ok(existing) => templates.update(&meta, &existing.metadata.resource_version, &spec).await.map(|_| ()),
                        Err(ResourceError::NotFound { .. }) => templates.create(&meta, &spec).await.map(|_| ()),
                        Err(err) => Err(err),
                    };
                    match outcome {
                        Ok(()) => flotilla_protocol::CommandValue::WorkflowTemplateApplied { name: name.clone() },
                        Err(err) => flotilla_protocol::CommandValue::Error { message: err.to_string() },
                    }
                }
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectAdd { target, name, display_name, remote } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_add(target, name.as_deref(), display_name.as_deref(), remote.as_deref()).await {
                Ok(name) => flotilla_protocol::CommandValue::ProjectAdded { name },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectApply { name, spec_yaml } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let namespace = self.provisioning_namespace().await;
            let projects = self.resource_backend.clone().definitions::<Project>(&namespace);
            let result = match validate_project_name(name).and_then(|_| parse_project_yaml(spec_yaml)) {
                Ok(spec) => match normalize_project_spec(spec) {
                    Ok(spec) => {
                        let outcome = match projects.get(name).await {
                            Ok(existing) if is_declaration_backed_project(&existing) => {
                                Err(format!("project {name} is managed by a declaration; use project refresh to update it"))
                            }
                            Ok(existing) => projects
                                .apply(&InputMeta::from(&existing.metadata), &spec)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(ResourceError::NotFound { .. }) => projects
                                .apply(&InputMeta::builder().name(name.clone()).build(), &spec)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error.to_string()),
                        };
                        match outcome {
                            Ok(()) => flotilla_protocol::CommandValue::ProjectApplied { name: name.clone() },
                            Err(message) => flotilla_protocol::CommandValue::Error { message },
                        }
                    }
                    Err(message) => flotilla_protocol::CommandValue::Error { message },
                },
                Err(err) => flotilla_protocol::CommandValue::Error { message: err },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectRegister { target } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_register(target).await {
                Ok((name, members)) => CommandValue::ProjectRegistered { name, members },
                Err(message) => CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::ProjectRefresh { name } = &command.action {
            let empty_identity = empty_repo_identity();
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity.clone(),
                repo: None,
                description: command.description().to_string(),
            });
            let result = match self.project_refresh(name).await {
                Ok((members, converged, changes)) => CommandValue::ProjectRefreshed { name: name.clone(), members, converged, changes },
                Err(message) => CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: empty_identity,
                repo: None,
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::TrackRepoPath { path } = &command.action {
            let description = command.description().to_string();
            let repo_path = path.clone();
            let repo_identity = self.detect_repo_identity(path).await;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.add_repo(path).await {
                Ok(outcome) => flotilla_protocol::CommandValue::RepoTracked {
                    path: outcome.tracked_path,
                    resolved_from: outcome.resolved_from,
                    identity_change: outcome.identity_change,
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: self.tracked_repo_identity_for_path(path).await.unwrap_or(repo_identity),
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::UntrackRepo { repo } = &command.action {
            let repo_path = self.resolve_repo_selector(repo).await?;
            let description = command.description().to_string();
            let repo_identity =
                self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.remove_repo(&repo_path).await {
                Ok(()) => flotilla_protocol::CommandValue::RepoUntracked { path: repo_path.clone() },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        if let flotilla_protocol::CommandAction::Refresh { repo: Some(selector) } = &command.action {
            let repo_path = self.resolve_repo_selector(selector).await?;
            let description = command.description().to_string();
            let repo_identity =
                self.tracked_repo_identity_for_path(&repo_path).await.ok_or_else(|| format!("repo not found: {}", repo_path.display()))?;
            let _ = self.event_tx.send(DaemonEvent::CommandStarted {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity: repo_identity.clone(),
                repo: Some(repo_path.clone()),
                description,
            });
            let result = match self.refresh(&flotilla_protocol::RepoSelector::Path(repo_path.clone())).await {
                Ok(identity_change) => flotilla_protocol::CommandValue::Refreshed {
                    repos: vec![repo_path.clone()],
                    identity_changes: identity_change.into_iter().collect(),
                },
                Err(message) => flotilla_protocol::CommandValue::Error { message },
            };
            let _ = self.event_tx.send(DaemonEvent::CommandFinished {
                command_id: id,
                node_id: self.node_id.clone(),
                repo_identity,
                repo: Some(repo_path),
                result,
            });
            return Ok(id);
        }

        // Gather what the spawned task needs — validate repo before broadcasting
        let repo = self.resolve_repo_for_command(&command).await?;
        let repository_action_policy_error = self.repository_action_policy_error(&command, &repo).await;
        let runner = Arc::clone(&self.discovery.runner);
        let env = Arc::clone(&self.discovery.env);
        let event_tx = self.event_tx.clone();
        let (repo_identity, registry) = {
            let repos = self.repos.read().await;
            let identity =
                self.tracked_repo_identity_for_path(&repo).await.ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            let state = repos.get(&identity).ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;
            (state.identity().clone(), state.registry())
        };
        let providers_data = Arc::new(self.executor_provider_data(&repo_identity, &repo, &registry).await);

        let description = command.description().to_string();
        let repo_path = repo.to_path_buf();
        let config_base = DaemonHostPath::new(self.config.base_path().as_path());

        let active_ref = Arc::clone(&self.active_commands);
        let token = CancellationToken::new();
        {
            let mut guard = active_ref.lock().await;
            guard.insert(id, token.clone());
        }

        let _ = self.event_tx.send(DaemonEvent::CommandStarted {
            command_id: id,
            node_id: command_node_id.clone(),
            repo_identity: repo_identity.clone(),
            repo: Some(repo_path.clone()),
            description,
        });

        let local_host = self.host_name.clone();
        let local_node_id = self.node_id.clone();
        let attachable_store = self.discovery.shared_attachable_store(&self.config);
        let daemon_socket_path = self.daemon_socket_path.read().await.clone();
        let environment_manager = Arc::clone(&self.environment_manager);
        tokio::spawn(async move {
            let resolver_registry = Arc::clone(&registry);
            let resolver_providers_data = Arc::clone(&providers_data);
            let resolver_runner = Arc::clone(&runner);
            let resolver_env = Arc::clone(&env);
            let resolver_config_base = config_base.clone();
            let resolver_attachable_store = attachable_store.clone();
            let resolver_local_host = local_host.clone();
            let ee_repo_path = ExecutionEnvironmentPath::new(&repo_path);
            let resolver_repo = executor::RepoExecutionContext { identity: repo_identity.clone(), root: ee_repo_path.clone() };
            let daemon_socket_dhp = daemon_socket_path.map(DaemonHostPath::new);

            let plan = match repository_action_policy_error {
                Some(message) => Err(CommandValue::Error { message }),
                None => executor::build_plan(
                    command,
                    executor::RepoExecutionContext { identity: repo_identity.clone(), root: ee_repo_path },
                    registry,
                    providers_data,
                    config_base,
                    attachable_store,
                    daemon_socket_dhp.clone(),
                    local_node_id.clone(),
                    local_host,
                )
                .await
                .map_err(executor::PlannerRefusal::into_command_value),
            };

            match plan {
                Err(result) => {
                    {
                        let mut guard = active_ref.lock().await;
                        guard.remove(&id);
                    }
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id.clone(),
                        repo_identity: repo_identity.clone(),
                        repo: Some(repo_path),
                        result,
                    });
                }
                Ok(step_plan) => {
                    let resolver = executor::ExecutorStepResolver {
                        repo: resolver_repo,
                        registry: resolver_registry,
                        providers_data: resolver_providers_data,
                        runner: resolver_runner,
                        env: resolver_env,
                        config_base: resolver_config_base,
                        attachable_store: resolver_attachable_store,
                        daemon_socket_path: daemon_socket_dhp.clone(),
                        local_node_id: local_node_id.clone(),
                        local_host: resolver_local_host.clone(),
                        environment_manager: Arc::clone(&environment_manager),
                    };
                    let result = run_step_plan_with_remote_executor(
                        step_plan,
                        id,
                        local_node_id,
                        repo_identity.clone(),
                        ExecutionEnvironmentPath::new(&repo_path),
                        token,
                        event_tx.clone(),
                        &resolver,
                        remote_executor.as_ref(),
                    )
                    .await;
                    let mut guard = active_ref.lock().await;
                    guard.remove(&id);
                    let _ = event_tx.send(DaemonEvent::CommandFinished {
                        command_id: id,
                        node_id: command_node_id,
                        repo_identity,
                        repo: Some(repo_path),
                        result,
                    });
                }
            }
        });

        Ok(id)
    }
}

fn transient_checkout_session_name(checkout: &CheckoutRow) -> String {
    let mut hasher = Sha256::new();
    hasher.update(checkout.host.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(checkout.path.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("flotilla-checkout-{}", &digest[..32])
}

async fn execute_local_remote_step_batch(
    local_host: NodeId,
    request: RemoteStepBatchRequest,
    progress_sink: Arc<dyn RemoteStepProgressSink>,
    cancel: CancellationToken,
    resolver: &dyn StepResolver,
) -> Result<Vec<StepOutcome>, String> {
    let mut outcomes = Vec::new();
    let step_count = request.steps.len();

    for (index, step) in request.steps.into_iter().enumerate() {
        if step.host.node_id() != &local_host {
            return Err(format!("remote step {} targets {:?}, expected remote node {}", index, step.host, local_host));
        }
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }

        progress_sink
            .emit(crate::step::RemoteStepProgressUpdate {
                batch_step_index: index,
                batch_step_count: step_count,
                description: step.description.clone(),
                status: flotilla_protocol::StepStatus::Started,
            })
            .await;

        let outcome = resolver.resolve(&step.description, &step.host, step.action, &outcomes).await;
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }

        match outcome {
            Ok(step_outcome) => {
                let status = match &step_outcome {
                    StepOutcome::Skipped => flotilla_protocol::StepStatus::Skipped,
                    _ => flotilla_protocol::StepStatus::Succeeded,
                };
                progress_sink
                    .emit(crate::step::RemoteStepProgressUpdate {
                        batch_step_index: index,
                        batch_step_count: step_count,
                        description: step.description,
                        status,
                    })
                    .await;
                outcomes.push(step_outcome);
            }
            Err(message) => {
                progress_sink
                    .emit(crate::step::RemoteStepProgressUpdate {
                        batch_step_index: index,
                        batch_step_count: step_count,
                        description: step.description,
                        status: flotilla_protocol::StepStatus::Failed { message: message.clone() },
                    })
                    .await;
                return Err(message);
            }
        }
    }

    Ok(outcomes)
}

impl InProcessDaemon {
    async fn explain_convoy_internal(&self, requested_namespace: Option<&str>, name: &str) -> Result<ConvoyExplanation, String> {
        let namespace = requested_namespace.map(ToOwned::to_owned).unwrap_or(self.provisioning_namespace().await);
        let convoy_sources =
            self.resource_backend.including_replicas::<ResourceConvoy>(&namespace).list().await.map_err(|error| error.to_string())?;
        let identities = convoy_sources
            .items
            .iter()
            .map(|source| ConvoyAddressIdentity {
                record_name: &source.object.metadata.name,
                role: source.object.metadata.labels.get(ROLE_LABEL).map(String::as_str),
                project: source.object.metadata.labels.get(PROJECT_LABEL).map(String::as_str),
                terminal: source.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
            })
            .collect::<Vec<_>>();
        let selected = resolve_convoy_candidate_indices(&identities, name)?;
        let convoy_source = selected
            .into_iter()
            .map(|index| &convoy_sources.items[index])
            .max_by_key(|source| matches!(source.provenance, ResourceProvenance::Local))
            .ok_or_else(|| format!("no convoy matches `{name}`"))?;
        let convoy = convoy_source.object.clone();
        let now = self.clock.now();
        let change_request_stale_after = self.change_request_stale_after();

        let checkout_sources =
            self.resource_backend.including_replicas::<ResourceCheckout>(&namespace).list().await.map_err(|error| error.to_string())?.items;
        let selected_checkouts = flotilla_resources::select_convoy_children(&convoy, &checkout_sources);
        let expected = expected_checkout_refs(&convoy).map_err(|error| format!("derive expected checkouts: {error}"))?;
        let checkouts = expected
            .iter()
            .map(|checkout_name| {
                let selected = selected_checkouts.get(checkout_name);
                let provenance = selected.and_then(|object| {
                    checkout_sources
                        .iter()
                        .find(|source| {
                            source.object.metadata.name == object.metadata.name
                                && source.object.metadata.resource_version == object.metadata.resource_version
                        })
                        .map(|source| explained_provenance(&source.provenance, &self.node_id))
                });
                let integration = selected.and_then(|checkout| checkout.status.as_ref()).map(|status| &status.integration);
                ExplainedCheckout {
                    name: checkout_name.clone(),
                    observed: selected.is_some(),
                    provenance,
                    clean: integration.map(|status| explain_condition(&status.clean, now, LANDING_EVIDENCE_TTL)),
                    pushed: integration.map(|status| explain_condition(&status.pushed, now, LANDING_EVIDENCE_TTL)),
                    landed: integration.map(|status| explain_condition(&status.landed, now, LANDING_EVIDENCE_TTL)),
                }
            })
            .collect::<Vec<_>>();

        let change_request_sources = self
            .resource_backend
            .including_replicas::<flotilla_resources::ChangeRequest>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items;
        let mut selected_change_requests = BTreeMap::new();
        for source in &change_request_sources {
            let name = source.object.metadata.name.clone();
            let replace = selected_change_requests.get(&name).is_none_or(
                |existing: &&flotilla_resources::ReadResourceObject<flotilla_resources::ChangeRequest>| {
                    !matches!(existing.provenance, ResourceProvenance::Local) && matches!(source.provenance, ResourceProvenance::Local)
                },
            );
            if replace {
                selected_change_requests.insert(name, source);
            }
        }
        let change_request_objects =
            selected_change_requests.iter().map(|(name, source)| (name.clone(), source.object.clone())).collect::<BTreeMap<_, _>>();
        let expected_change_requests = expected_change_request_leaves(&convoy, &selected_checkouts)
            .map_err(|error| format!("derive expected change requests: {error}"))?
            .into_iter()
            .filter_map(|leaf| match leaf.address {
                flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } => {
                    Some(flotilla_resources::change_request_record_name(&service, &scope, number))
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let bound_name = bound_change_request_record_name(&convoy).map_err(|error| format!("derive bound change request: {error}"))?;
        let change_requests = expected_change_requests
            .iter()
            .map(|record_name| {
                let selected = selected_change_requests.get(record_name).copied();
                let observed_at = selected.and_then(|source| source.object.status.as_ref()).map(|status| status.state.observed_at);
                ExplainedChangeRequest {
                    name: record_name.clone(),
                    bound: bound_name.as_ref() == Some(record_name),
                    observed: selected.is_some(),
                    provenance: selected.map(|source| explained_provenance(&source.provenance, &self.node_id)),
                    fields: selected.and_then(|source| serde_json::to_value(&source.object).ok()),
                    observed_at: observed_at.map(|at| at.to_rfc3339()),
                    freshness: observed_freshness(observed_at, now, change_request_stale_after),
                }
            })
            .collect::<Vec<_>>();

        let evaluation = evaluate_landing_settlement(
            &convoy,
            &selected_checkouts,
            &change_request_objects,
            change_request_stale_after,
            LANDING_EVIDENCE_TTL,
            now,
        );
        let settlement = ExplainedSettlement {
            mode: match evaluation.mode {
                SettlementMode::NoExit => flotilla_protocol::commands::SETTLEMENT_MODE_STANDING,
                SettlementMode::ClaimExit => "claim_exit",
                SettlementMode::WorldTerminal => "world_terminal",
            }
            .to_string(),
            satisfied: evaluation.satisfied,
            unmet: evaluation.unmet.into_iter().map(explain_unmet_expectation).collect(),
        };

        let subscriptions = self
            .leaf_subscriptions
            .diagnostics()
            .await
            .into_iter()
            .filter(|(row, _)| matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy } | LeafWatcher::TurnDelivery { convoy, .. } if convoy == name))
            .map(|(row, firings)| ExplainedSubscription {
                id: row.id,
                watcher: match row.watcher {
                    LeafWatcher::WaitCaller { .. } => "wait_caller",
                    LeafWatcher::ReconcilerWake { .. } => "reconciler_wake",
                    LeafWatcher::TurnDelivery { .. } => "turn_delivery",
                }
                .to_string(),
                leaves: row.leaves,
                last_leaf_firings: firings
                    .into_iter()
                    .map(|firing| ExplainedLeafFiring { leaf: firing.leaf, value: firing.value, fired_at: firing.fired_at.to_rfc3339() })
                    .collect(),
            })
            .collect();

        let mut crew_deliveries = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|source| source.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|convoy| convoy == name))
            .map(|source| ExplainedCrewDelivery {
                session: source.object.metadata.name,
                role: source.object.spec.role,
                // ADR 0028's delivery ladder has not landed yet. Keep the
                // field explicit so recorded rungs appear without inventing
                // one from session liveness or message delivery.
                last_delivery_rung: None,
                delivered_message_id: source.object.status.and_then(|status| status.delivered_message_id),
            })
            .collect::<Vec<_>>();
        crew_deliveries.sort_by(|left, right| left.session.cmp(&right.session));

        let decision_ledgers = explained_decision_ledgers(convoy.status.as_ref());

        Ok(ConvoyExplanation {
            namespace,
            convoy: name.to_string(),
            phase: convoy.status.as_ref().map_or_else(|| "Unknown".to_string(), |status| format!("{:?}", status.phase)),
            evidence_ttl_seconds: LANDING_EVIDENCE_TTL.as_secs(),
            change_request_stale_after_seconds: change_request_stale_after.as_secs(),
            checkouts,
            change_requests,
            subscriptions,
            crew_deliveries,
            decision_ledgers,
            settlement,
        })
    }
}

fn explained_decision_ledgers(status: Option<&ConvoyStatus>) -> Vec<ExplainedDecisionLedger> {
    status
        .into_iter()
        .flat_map(|status| &status.crew_work)
        .flat_map(|(vessel, crew)| {
            crew.iter().filter(|(_, claim)| matches!(claim.phase, CrewWorkPhase::Done | CrewWorkPhase::HandedBack)).map(
                move |(role, claim)| ExplainedDecisionLedger {
                    vessel: vessel.clone(),
                    role: role.clone(),
                    claimed_at: claim.finished_at.map(|at| at.to_rfc3339()),
                    comment_url: claim.decision_ledger_ref.clone(),
                    missing: claim.decision_ledger_ref.is_none(),
                },
            )
        })
        .collect()
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    fn query_subscription(&self, subscriber_id: uuid::Uuid) -> QuerySubscription {
        let state = self.aggregator_projection_state.clone();
        let namespace = self.provisioning_namespace.read().expect("provisioning namespace lock poisoned").clone();
        self.connect_surface(subscriber_id, SurfaceDeclaration::focal_for_namespace(namespace));
        let daemon = self.self_weak.clone();
        QuerySubscription::new(move || {
            state.remove_subscriber(subscriber_id);
            if let Some(daemon) = daemon.upgrade() {
                tokio::spawn(async move {
                    if let Err(error) = daemon.disconnect_surface(subscriber_id).await {
                        warn!(%error, "failed to disconnect in-process surface");
                    }
                });
            }
        })
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let repository_keys = self.repository_keys_by_path.read().await;
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut result = Vec::new();
        for identity in order.iter() {
            if let Some(state) = repos.get(identity) {
                result.push(RepoInfo {
                    identity: state.identity().clone(),
                    repository_key: repository_keys.get(state.preferred_path()).cloned(),
                    path: Some(state.preferred_path().to_path_buf()),
                    name: repo_name(state.preferred_path()),
                    labels: state.labels().clone(),
                    provider_names: state.provider_names(),
                    provider_health: HashMap::new(),
                    loading: false,
                });
            }
        }
        Ok(result)
    }

    async fn execute(&self, command: Command) -> Result<u64, String> {
        self.execute_impl(command, Arc::new(crate::step::UnsupportedRemoteStepExecutor), false, None).await
    }

    async fn execute_query(&self, command: Command, session_id: uuid::Uuid) -> Result<flotilla_protocol::CommandValue, String> {
        use flotilla_protocol::CommandAction;
        match &command.action {
            CommandAction::QueryRepoProviders { repo } => match self.get_repo_providers_internal(repo).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::RepoProviders(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostList {} => match self.list_hosts_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryProjectList {} => match self.list_projects_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::ProjectList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDispatchQueue { project } => match self.dispatch_queue_internal(project.as_deref()).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::DispatchQueue(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostStatus { target_environment_id } => match self.get_host_status_internal(target_environment_id).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::HostStatus(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryHostProviders { target_environment_id } => {
                match self.get_host_providers_internal(target_environment_id).await {
                    Ok(v) => Ok(flotilla_protocol::CommandValue::HostProviders(Box::new(v))),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryFleetHealth {} => match self.fleet_health_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetHealth(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetList {} => match self.fleet_list_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryCrewList { context } => match self.crew_list_internal(context).await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::CrewList(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryFleetReplicaSnapshot {} => match self.fleet_replica_snapshot_internal().await {
                Ok(v) => Ok(flotilla_protocol::CommandValue::FleetReplicaSnapshot(Box::new(v))),
                Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
            },
            CommandAction::QueryDaemonLogs { query } => {
                let generations = self.config.load_daemon_config()?.logging.generations;
                let state_dir = self.config.state_dir().as_path().to_path_buf();
                let query = query.clone();
                let read_result = tokio::task::spawn_blocking(move || crate::log_file::read_daemon_logs(&state_dir, generations, &query))
                    .await
                    .map_err(|error| format!("daemon log reader task failed: {error}"))?;
                match read_result {
                    Ok(lines) => Ok(flotilla_protocol::CommandValue::DaemonLogs { lines }),
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryExplainConvoy { namespace, name } => match self.explain_convoy_internal(namespace.as_deref(), name).await {
                Ok(explanation) => Ok(CommandValue::ConvoyExplanation(Box::new(explanation))),
                Err(message) => Ok(CommandValue::Error { message }),
            },
            CommandAction::QueryResourceList { namespace, kind, include_replicas } => {
                let listed = if *include_replicas {
                    list_resource_kind_including_replicas(&self.resource_backend, namespace, kind).await
                } else {
                    list_resource_kind(&self.resource_backend, namespace, kind).await
                };
                match listed {
                    Ok(v) => {
                        let resource_version = v.value["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
                        let generation = v.value["metadata"]["generation"].as_str().map(ToOwned::to_owned);
                        let records = v.value["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .cloned()
                            .map(|object| resource_record(ResourceRecordType::Current, object, &self.node_id))
                            .collect();
                        Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                            v.kind,
                            v.plural,
                            v.namespace,
                            ResourceCursor::from_position(resource_version, generation),
                            records,
                        ))))
                    }
                    Err(error) => Ok(CommandValue::Error { message: error.to_string() }),
                }
            }
            CommandAction::QueryResourceGet { namespace, kind, name } => {
                // Take the collection cursor before reading the object. A
                // concurrent mutation can then be replayed (at worst as a
                // duplicate) instead of being hidden behind a newer cursor.
                let cursor_list = match list_resource_kind(&self.resource_backend, namespace, kind).await {
                    Ok(listed) => listed,
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let visible = match get_resource_kind_including_replicas(&self.resource_backend, namespace, kind, name).await {
                    Ok(object) => object,
                    Err(ResourceError::NotFound { .. }) => {
                        return Ok(CommandValue::Error { message: format!("resource {kind}/{namespace}/{name} not found") });
                    }
                    Err(error) => return Ok(CommandValue::Error { message: error.to_string() }),
                };
                let resource_version = cursor_list.value["metadata"]["resourceVersion"].as_str().unwrap_or_default().to_string();
                let generation = cursor_list.value["metadata"]["generation"].as_str().map(ToOwned::to_owned);
                let record = resource_record(ResourceRecordType::Current, visible.value, &self.node_id);
                Ok(CommandValue::ResourceRead(Box::new(resource_read_envelope(
                    visible.kind,
                    visible.plural,
                    visible.namespace,
                    ResourceCursor::from_position(resource_version, generation),
                    vec![record],
                ))))
            }
            CommandAction::Attach { reference, host, mode } => {
                let project_context = self.attach_project_context(command.context_repo.as_ref()).await?;
                match self.resolve_attach_with_context(reference, host.as_ref(), false, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        if let Some(binding) = &resolved.binding {
                            if let Err(error) = self.emit_attach_regard(binding, session_id).await {
                                warn!(%error, "failed to emit attach regard");
                            }
                        }
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::AttachTransient { reference, host, mode } => {
                let project_context = self.attach_project_context(command.context_repo.as_ref()).await?;
                match self.resolve_attach_with_context(reference, host.as_ref(), true, *mode, project_context.as_deref()).await {
                    Ok(resolved) => {
                        Ok(flotilla_protocol::CommandValue::AttachCommandResolved { plan: resolved.plan, binding: resolved.binding })
                    }
                    Err(message) => Ok(flotilla_protocol::CommandValue::Error { message }),
                }
            }
            CommandAction::QueryIssues { repo, params, page, count } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                let page = provider.query(&source, params, *page, *count).await?;
                Ok(flotilla_protocol::CommandValue::IssuePage(page))
            }
            CommandAction::QueryIssueFetchByIds { repo, ids } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                let items = provider.fetch_by_ids(&source, ids).await?;
                Ok(flotilla_protocol::CommandValue::IssuesByIds { items })
            }
            CommandAction::QueryIssueOpenInBrowser { repo, id } => {
                let repo_path = self.resolve_repo_selector(repo).await?;
                let (provider, source) = self.get_issue_provider_for_repo(&repo_path).await?;
                provider.open_in_browser(&flotilla_protocol::IssueRef { source, id: id.clone() }).await?;
                Ok(flotilla_protocol::CommandValue::Ok)
            }
            other => Err(format!("execute_query not implemented for this command type: {:?}", std::mem::discriminant(other))),
        }
    }

    async fn observe_focus(&self, surface_id: uuid::Uuid, targets: Vec<ResourceRef>) -> Result<(), String> {
        self.observe_surface_focus(surface_id, targets).await
    }

    async fn cancel(&self, command_id: u64) -> Result<(), String> {
        let guard = self.active_commands.lock().await;
        match guard.get(&command_id) {
            Some(token) => {
                token.cancel();
                Ok(())
            }
            None => Err("no matching active command".into()),
        }
    }

    async fn replay_since(&self, last_seen: &HashMap<StreamKey, u64>) -> Result<Vec<DaemonEvent>, String> {
        let _ = self.refresh_local_host_summary().await;
        Ok(self.host_registry.replay_host_events(last_seen).await)
    }

    async fn subscribe_queries(&self, subscriber_id: uuid::Uuid, queries: &[QueryCursor]) -> Result<Vec<DaemonEvent>, String> {
        let state = self.aggregator_projection_state().await;
        let newly_materialized = state.replace_subscriber(subscriber_id, queries);
        let mut events = Vec::new();
        let mut initial_row_counts = Vec::with_capacity(queries.len());
        for cursor in queries {
            let result_set =
                state.result_set_for(&cursor.query).await.ok_or_else(|| format!("query is not materialized: {}", cursor.query))?;
            initial_row_counts.push((cursor.query.clone(), result_set.rows.len()));
            if newly_materialized.contains(&cursor.query) || cursor.since.is_none_or(|seq| seq != result_set.seq) {
                events.push(DaemonEvent::ResultSet(Box::new(result_set)));
            }
        }
        info!(
            subscriber = %subscriber_id,
            queries = ?queries,
            initial_row_counts = ?initial_row_counts,
            replayed_result_sets = events.len(),
            "query subscription initialized"
        );
        Ok(events)
    }

    async fn unsubscribe_queries(&self, subscriber_id: uuid::Uuid) {
        self.aggregator_projection_state().await.remove_subscriber(subscriber_id);
    }

    async fn fetch_more(&self, query: &flotilla_protocol::QueryId) -> Result<(), String> {
        self.aggregator_projection_state().await.request_fetch_more(query)
    }

    async fn get_status(&self) -> Result<StatusResponse, String> {
        let repos = self.repos.read().await;
        let repo_order = self.repo_order.read().await;
        let mut summaries = Vec::new();

        for identity in repo_order.iter() {
            let Some(state) = repos.get(identity) else { continue };
            summaries.push(RepoSummary {
                path: state.preferred_path().to_path_buf(),
                slug: state.slug().map(str::to_string),
                provider_health: HashMap::new(),
                unmet_requirements: state
                    .unmet()
                    .iter()
                    .map(|(factory, requirement)| crate::convert::unmet_requirement_to_proto(factory, requirement))
                    .collect(),
            });
        }
        Ok(StatusResponse { repos: summaries })
    }

    async fn get_topology(&self) -> Result<TopologyResponse, String> {
        Ok(self.host_registry.get_topology().await)
    }
}
