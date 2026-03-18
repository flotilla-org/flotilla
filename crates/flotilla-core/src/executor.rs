//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandResult`.
//! No UI state mutation — all results are carried in the return value.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::{
    AttachableSetId, CheckoutSelector, CheckoutTarget, Command, CommandAction, CommandResult, HostName, HostPath, ManagedTerminalId,
    PreparedTerminalCommand,
};
use tracing::{debug, error, info, warn};

use crate::{
    attachable::{BindingObjectKind, ProviderBinding, SharedAttachableStore},
    data,
    provider_data::ProviderData,
    providers::{
        registry::ProviderRegistry,
        run,
        terminal::TerminalPool,
        types::{CloudAgentSession, CorrelationKey, WorkspaceConfig},
        workspace::WorkspaceManager,
        CommandRunner,
    },
    step::{Step, StepAction, StepHost, StepOutcome, StepPlan, StepResolver},
    template::{
        WorkspaceTemplate, {self},
    },
};

/// The result of `build_plan`: either an immediate result or a multi-step plan.
pub enum ExecutionPlan {
    /// Command completed synchronously — no steps needed.
    Immediate(CommandResult),
    /// Command requires multiple steps with cancellation support.
    Steps(StepPlan),
}

#[derive(Clone)]
pub struct RepoExecutionContext {
    pub identity: flotilla_protocol::RepoIdentity,
    pub root: PathBuf,
}

#[derive(Clone, Copy)]
enum CheckoutIntent {
    ExistingBranch,
    FreshBranch,
}

/// Build an execution plan for a command.
///
/// Multi-step commands (CreateCheckout, TeleportSession, RemoveCheckout,
/// ArchiveSession, GenerateBranchName) return `ExecutionPlan::Steps` with
/// cancellation points between steps. All other commands delegate to
/// `execute()` and return `ExecutionPlan::Immediate`.
#[allow(clippy::too_many_arguments)]
pub async fn build_plan(
    cmd: Command,
    repo: RepoExecutionContext,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: HostName,
    // TODO(multi-host): When a command is forwarded from another host, this carries
    // the requester's hostname so plan builders can stamp `StepHost::Remote(originator)`
    // on steps that need to run back on the presentation host (e.g. workspace creation
    // after a remote checkout). Passed by `execute_forwarded_command` in server.rs.
    _originating_host: Option<HostName>,
) -> ExecutionPlan {
    let Command { action, .. } = cmd;

    match action {
        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            build_create_checkout_plan(branch, create_branch, intent, issue_ids, repo.root, registry, providers_data, runner, local_host)
                .await
        }

        CommandAction::TeleportSession { session_id, branch, checkout_key } => {
            build_teleport_session_plan(
                session_id,
                branch,
                checkout_key,
                repo.root,
                registry,
                providers_data,
                config_base,
                attachable_store.clone(),
                daemon_socket_path.clone(),
                local_host,
            )
            .await
        }

        CommandAction::RemoveCheckout { checkout, terminal_keys } => {
            match resolve_checkout_branch(&checkout, &providers_data, &local_host) {
                Ok(branch) => {
                    build_remove_checkout_plan(branch, terminal_keys, repo.root, registry, providers_data, attachable_store, local_host)
                }
                Err(message) => ExecutionPlan::Immediate(CommandResult::Error { message }),
            }
        }

        CommandAction::ArchiveSession { session_id } => build_archive_session_plan(session_id, registry, providers_data).await,

        CommandAction::GenerateBranchName { issue_keys } => build_generate_branch_name_plan(issue_keys, registry, providers_data).await,

        action => {
            let result = execute(
                action,
                &repo,
                &registry,
                &providers_data,
                &*runner,
                &config_base,
                &attachable_store,
                daemon_socket_path.as_deref(),
                &local_host,
            )
            .await;
            ExecutionPlan::Immediate(result)
        }
    }
}

/// Build a step plan for `CreateCheckout`.
///
/// Steps:
/// 1. Create the checkout (skipped if it already exists on the local host)
/// 2. Link issues to the branch (skipped if no issue_ids)
/// 3. Create a workspace for the new checkout
///
/// The final step creates a workspace for the new checkout. This is a symbolic
/// step resolved by the `ExecutorStepResolver` at execution time, so it has
/// access to the registry and config without needing pre-refreshed provider data.
#[allow(clippy::too_many_arguments)]
async fn build_create_checkout_plan(
    branch: String,
    create_branch: bool,
    intent: CheckoutIntent,
    issue_ids: Vec<(String, String)>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    runner: Arc<dyn CommandRunner>,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Check if checkout already exists for this branch on the local host.
    let existing_checkout_path: Option<PathBuf> =
        providers_data.checkouts.iter().find_map(
            |(hp, co)| {
                if hp.host == local_host && co.branch == branch {
                    Some(hp.path.clone())
                } else {
                    None
                }
            },
        );

    let mut steps = Vec::new();

    // Step 1: Create checkout
    {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        let runner = Arc::clone(&runner);
        let existing = existing_checkout_path.clone();
        steps.push(Step {
            description: format!("Create checkout for branch {branch}"),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    validate_checkout_target(&repo_root, &branch, intent, &*runner).await?;
                    // If checkout already exists, emit CheckoutCreated so the workspace
                    // step can find the path in prior outcomes.
                    if let Some(path) = existing {
                        if matches!(intent, CheckoutIntent::FreshBranch) {
                            return Err(format!("branch already exists: {branch}"));
                        }
                        return Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch, path }));
                    }
                    let cm = registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
                    let (path, _checkout) = cm.create_checkout(&repo_root, &branch, create_branch).await?;
                    info!(checkout_path = %path.display(), "created checkout");
                    Ok(StepOutcome::CompletedWith(CommandResult::CheckoutCreated { branch, path }))
                })
            })),
        });
    }

    // Step 2: Link issues (only if non-empty)
    if !issue_ids.is_empty() {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let runner = Arc::clone(&runner);
        steps.push(Step {
            description: "Link issues to branch".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    write_branch_issue_links(&repo_root, &branch, &issue_ids, &*runner).await;
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Final step: create workspace for the new checkout.
    steps.push(Step {
        description: "Create workspace".to_string(),
        host: StepHost::Local,
        action: StepAction::CreateWorkspaceForCheckout { label: branch.clone() },
    });

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Build a step plan for `TeleportSession`.
///
/// Steps:
/// 1. Resolve attach command from the session's cloud agent provider
/// 2. Ensure checkout exists (skipped if checkout_key references a known checkout, or no branch)
/// 3. Create workspace with the teleport (attach) command
#[allow(clippy::too_many_arguments)]
async fn build_teleport_session_plan(
    session_id: String,
    branch: Option<String>,
    checkout_key: Option<PathBuf>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    config_base: PathBuf,
    attachable_store: SharedAttachableStore,
    daemon_socket_path: Option<PathBuf>,
    local_host: flotilla_protocol::HostName,
) -> ExecutionPlan {
    // Shared slot for the teleport (attach) command — populated by step 1.
    let teleport_cmd_slot: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));

    // Shared slot for checkout path — pre-populated if checkout_key references a known checkout.
    let checkout_path_slot: Arc<tokio::sync::Mutex<Option<PathBuf>>> = {
        let existing = checkout_key.as_ref().and_then(|key| {
            let host_key = flotilla_protocol::HostPath::new(local_host.clone(), key.clone());
            providers_data.checkouts.get(&host_key).map(|_| key.clone())
        });
        Arc::new(tokio::sync::Mutex::new(existing))
    };

    let mut steps = Vec::new();

    // Step 1: Resolve attach command
    {
        let slot = Arc::clone(&teleport_cmd_slot);
        let session_id = session_id.clone();
        let registry = Arc::clone(&registry);
        let providers_data = Arc::clone(&providers_data);
        steps.push(Step {
            description: format!("Resolve attach command for session {session_id}"),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let cmd = resolve_attach_command(&session_id, &registry, &providers_data).await?;
                    *slot.lock().await = Some(cmd);
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 2: Ensure checkout if needed
    // Only runs when there's no pre-existing checkout and a branch is provided.
    {
        let slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: "Ensure checkout for teleport".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    // Already have a checkout path — skip
                    if slot.lock().await.is_some() {
                        return Ok(StepOutcome::Skipped);
                    }
                    let branch_name = match &branch {
                        Some(b) => b.clone(),
                        None => return Ok(StepOutcome::Skipped),
                    };
                    let cm = registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
                    let (path, _checkout) = cm.create_checkout(&repo_root, &branch_name, false).await?;
                    *slot.lock().await = Some(path);
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 3: Create workspace with teleport command
    {
        let teleport_slot = Arc::clone(&teleport_cmd_slot);
        let path_slot = Arc::clone(&checkout_path_slot);
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        let config_base = config_base.clone();
        let attachable_store = attachable_store.clone();
        let daemon_socket_path = daemon_socket_path.clone();
        steps.push(Step {
            description: "Create workspace with teleport command".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let path =
                        path_slot.lock().await.clone().ok_or_else(|| "Could not determine checkout path for teleport".to_string())?;
                    let teleport_cmd = teleport_slot.lock().await.clone().ok_or_else(|| "Attach command not resolved".to_string())?;
                    let name = branch.as_deref().unwrap_or("session");
                    if let Some(ws_mgr) = registry.workspace_managers.preferred() {
                        let mut config = workspace_config(&repo_root, name, &path, &teleport_cmd, &config_base);
                        if let Some((tp_desc, tp)) = registry.terminal_pools.preferred_with_desc() {
                            resolve_terminal_pool(
                                &mut config,
                                tp.as_ref(),
                                &attachable_store,
                                &tp_desc.implementation,
                                daemon_socket_path.as_deref(),
                            )
                            .await;
                        }
                        // Teleport always creates a new workspace — the attach command is
                        // session-specific, so reusing an existing workspace would attach
                        // to the wrong session.
                        ws_mgr.create_workspace(&config).await?;
                    }
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Build a step plan for `RemoveCheckout`.
///
/// Steps:
/// 1. Remove the checkout via the checkout manager
/// 2. Cascade-delete attachable sets owned by this checkout
/// 3. Clean up correlated terminal sessions (best-effort)
fn build_remove_checkout_plan(
    branch: String,
    terminal_keys: Vec<ManagedTerminalId>,
    repo_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
    attachable_store: SharedAttachableStore,
    local_host: HostName,
) -> ExecutionPlan {
    let mut steps = Vec::new();

    // Step 1: Remove checkout
    {
        let branch = branch.clone();
        let repo_root = repo_root.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: format!("Remove checkout for branch {branch}"),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let cm = registry.checkout_managers.preferred().cloned().ok_or_else(|| "No checkout manager available".to_string())?;
                    cm.remove_checkout(&repo_root, &branch).await?;
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 2: Cascade-delete attachable sets whose checkout matches the removed branch,
    // then tear down any terminal sessions that were members of those sets.
    {
        let branch = branch.clone();
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: "Cascade-delete attachable sets for removed checkout".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    let deleted_checkout_paths: Vec<HostPath> = providers_data
                        .checkouts
                        .iter()
                        .filter(|(hp, co)| co.branch == branch && hp.host == local_host)
                        .map(|(hp, _)| hp.clone())
                        .collect();

                    let mut all_session_refs = Vec::new();
                    let mut any_removed = false;
                    if let Ok(mut store) = attachable_store.lock() {
                        for checkout_path in &deleted_checkout_paths {
                            let set_ids = store.sets_for_checkout(checkout_path);
                            for set_id in set_ids {
                                if let Some(removed) = store.remove_set(&set_id) {
                                    all_session_refs.extend(removed.member_binding_refs);
                                    any_removed = true;
                                }
                            }
                        }
                        if any_removed {
                            if let Err(e) = store.save() {
                                warn!(err = %e, "failed to persist registry after cascade delete");
                            }
                        }
                    }

                    // Best-effort terminal teardown for cascade-removed sessions
                    if let Some(tp) = registry.terminal_pools.preferred() {
                        for session_ref in &all_session_refs {
                            if let Some(terminal_id) = crate::attachable::parse_terminal_session_binding_ref(session_ref) {
                                if let Err(e) = tp.kill_terminal(&terminal_id).await {
                                    warn!(
                                        session = %session_ref,
                                        err = %e,
                                        "failed to kill cascaded terminal session (best-effort)"
                                    );
                                }
                            }
                        }
                    }

                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    // Step 3: Clean up explicitly-passed terminal sessions (best-effort)
    if !terminal_keys.is_empty() {
        let registry = Arc::clone(&registry);
        steps.push(Step {
            description: "Clean up terminal sessions".to_string(),
            host: StepHost::Local,
            action: StepAction::Closure(Box::new(move |_prior| {
                Box::pin(async move {
                    if let Some(tp) = registry.terminal_pools.preferred() {
                        for terminal_id in &terminal_keys {
                            if let Err(e) = tp.kill_terminal(terminal_id).await {
                                warn!(
                                    terminal = %terminal_id,
                                    err = %e,
                                    "failed to kill terminal session (best-effort)"
                                );
                            }
                        }
                    }
                    Ok(StepOutcome::Completed)
                })
            })),
        });
    }

    ExecutionPlan::Steps(StepPlan::new(steps))
}

/// Check if a workspace already exists for `checkout_path` and select it.
/// Returns `true` if an existing workspace was found and selected, `false` otherwise.
/// Logs warnings on errors and returns `false` so callers always fall through to create.
async fn select_existing_workspace(ws_mgr: &dyn WorkspaceManager, checkout_path: &Path) -> bool {
    let existing = match ws_mgr.list_workspaces().await {
        Ok(ws) => ws,
        Err(e) => {
            warn!(err = %e, "failed to check existing workspaces, will create new");
            return false;
        }
    };
    for (ws_ref, ws) in &existing {
        if ws.directories.iter().any(|d| d == checkout_path) {
            info!(%ws_ref, path = %checkout_path.display(), "workspace already exists, selecting");
            if let Err(e) = ws_mgr.select_workspace(ws_ref).await {
                warn!(err = %e, %ws_ref, "failed to select existing workspace, will create new");
                return false;
            }
            return true;
        }
    }
    false
}

fn persist_workspace_binding(
    attachable_store: &SharedAttachableStore,
    provider_name: &str,
    workspace_ref: &str,
    target_host: &HostName,
    checkout_path: &Path,
) {
    let Ok(mut store) = attachable_store.lock() else {
        warn!("attachable store lock poisoned while persisting workspace binding");
        return;
    };
    let (set_id, changed_set) = store
        .ensure_terminal_set_with_change(Some(target_host.clone()), Some(HostPath::new(target_host.clone(), checkout_path.to_path_buf())));
    let changed_binding = store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: provider_name.to_string(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: set_id.to_string(),
        external_ref: workspace_ref.to_string(),
    });
    if changed_set || changed_binding {
        if let Err(err) = store.save() {
            warn!(err = %err, "failed to persist attachable registry after workspace binding update");
        }
    }
}

fn persist_workspace_binding_for_set(
    attachable_store: &SharedAttachableStore,
    provider_name: &str,
    workspace_ref: &str,
    set_id: &AttachableSetId,
    target_host: &HostName,
    checkout_path: &Path,
) {
    let Ok(mut store) = attachable_store.lock() else {
        warn!("attachable store lock poisoned while persisting workspace binding");
        return;
    };
    if !store.registry().sets.contains_key(set_id) {
        store.insert_set(flotilla_protocol::AttachableSet {
            id: set_id.clone(),
            host_affinity: Some(target_host.clone()),
            checkout: Some(HostPath::new(target_host.clone(), checkout_path.to_path_buf())),
            template_identity: None,
            members: Vec::new(),
        });
    }
    let changed_binding = store.replace_binding(ProviderBinding {
        provider_category: "workspace_manager".into(),
        provider_name: provider_name.to_string(),
        object_kind: BindingObjectKind::AttachableSet,
        object_id: set_id.to_string(),
        external_ref: workspace_ref.to_string(),
    });
    if changed_binding {
        if let Err(err) = store.save() {
            warn!(err = %err, "failed to persist attachable registry after workspace binding update");
        }
    }
}

fn ensure_attachable_set_for_checkout(
    attachable_store: &SharedAttachableStore,
    target_host: &HostName,
    checkout_path: &Path,
) -> Option<AttachableSetId> {
    let Ok(mut store) = attachable_store.lock() else {
        warn!("attachable store lock poisoned while ensuring attachable set for checkout");
        return None;
    };

    let checkout = HostPath::new(target_host.clone(), checkout_path.to_path_buf());
    let (set_id, changed) = store.ensure_terminal_set_with_change(Some(target_host.clone()), Some(checkout));
    if changed {
        if let Err(err) = store.save() {
            warn!(err = %err, "failed to persist attachable registry after ensuring attachable set");
        }
    }
    Some(set_id)
}

fn preferred_workspace_manager(registry: &ProviderRegistry) -> Option<(&str, &Arc<dyn WorkspaceManager>)> {
    registry.workspace_managers.preferred_with_desc().map(|(desc, provider)| (desc.implementation.as_str(), provider))
}

/// Core workspace creation logic, shared by the step resolver and the
/// standalone `CreateWorkspaceForCheckout` command.
#[allow(clippy::too_many_arguments)]
async fn create_workspace_for_checkout_impl(
    checkout_path: &Path,
    label: &str,
    repo: &RepoExecutionContext,
    registry: &ProviderRegistry,
    config_base: &Path,
    attachable_store: &SharedAttachableStore,
    daemon_socket_path: Option<&Path>,
    local_host: &HostName,
) -> Result<StepOutcome, String> {
    if let Some((provider_name, ws_mgr)) = preferred_workspace_manager(registry) {
        if select_existing_workspace(ws_mgr.as_ref(), checkout_path).await {
            return Ok(StepOutcome::Completed);
        }
        let mut config = workspace_config(&repo.root, label, checkout_path, "claude", config_base);
        if let Some((tp_desc, tp)) = registry.terminal_pools.preferred_with_desc() {
            resolve_terminal_pool(&mut config, tp.as_ref(), attachable_store, &tp_desc.implementation, daemon_socket_path).await;
        }
        match ws_mgr.create_workspace(&config).await {
            Ok((ws_ref, _workspace)) => {
                persist_workspace_binding(attachable_store, provider_name, &ws_ref, local_host, checkout_path);
                Ok(StepOutcome::Completed)
            }
            Err(e) => Err(e),
        }
    } else {
        Ok(StepOutcome::Skipped)
    }
}

/// Resolves symbolic `StepAction` variants using executor infrastructure.
pub(crate) struct ExecutorStepResolver {
    pub repo: RepoExecutionContext,
    pub registry: Arc<ProviderRegistry>,
    pub config_base: PathBuf,
    pub attachable_store: SharedAttachableStore,
    pub daemon_socket_path: Option<PathBuf>,
    pub local_host: HostName,
}

#[async_trait::async_trait]
impl StepResolver for ExecutorStepResolver {
    async fn resolve(&self, _description: &str, action: StepAction, prior: &[StepOutcome]) -> Result<StepOutcome, String> {
        match action {
            StepAction::Closure(_) => unreachable!("closures handled by stepper directly"),
            StepAction::CreateWorkspaceForCheckout { label } => {
                let path = prior.iter().find_map(|o| match o {
                    StepOutcome::CompletedWith(CommandResult::CheckoutCreated { path, .. }) => Some(path.clone()),
                    _ => None,
                });
                match path {
                    Some(p) => {
                        create_workspace_for_checkout_impl(
                            &p,
                            &label,
                            &self.repo,
                            &self.registry,
                            &self.config_base,
                            &self.attachable_store,
                            self.daemon_socket_path.as_deref(),
                            &self.local_host,
                        )
                        .await
                    }
                    None => Ok(StepOutcome::Skipped),
                }
            }
        }
    }
}

async fn build_archive_session_plan(
    session_id: String,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    let should_run_as_step = providers_data
        .sessions
        .get(session_id.as_str())
        .and_then(|session| session_provider_key(session, &session_id))
        .and_then(|provider_key| registry.cloud_agents.get(provider_key))
        .is_some();

    if !should_run_as_step {
        return ExecutionPlan::Immediate(archive_session_result(&session_id, &registry, &providers_data).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: format!("Archive session {session_id}"),
        host: StepHost::Local,
        action: StepAction::Closure(Box::new(move |_prior| {
            Box::pin(async move {
                match archive_session_result(&session_id, &registry, &providers_data).await {
                    CommandResult::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            })
        })),
    }]))
}

async fn build_generate_branch_name_plan(
    issue_keys: Vec<String>,
    registry: Arc<ProviderRegistry>,
    providers_data: Arc<ProviderData>,
) -> ExecutionPlan {
    if registry.ai_utilities.is_empty() {
        return ExecutionPlan::Immediate(generate_branch_name_result(&issue_keys, &registry, &providers_data).await);
    }

    ExecutionPlan::Steps(StepPlan::new(vec![Step {
        description: "Generate branch name".to_string(),
        host: StepHost::Local,
        action: StepAction::Closure(Box::new(move |_prior| {
            Box::pin(async move {
                match generate_branch_name_result(&issue_keys, &registry, &providers_data).await {
                    CommandResult::Error { message } => Err(message),
                    result => Ok(StepOutcome::CompletedWith(result)),
                }
            })
        })),
    }]))
}
/// Execute a `Command` against the given repo context.
///
/// Commands that are handled at the daemon level (TrackRepoPath, UntrackRepo, Refresh)
/// should not reach this function — the caller should handle them directly.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    action: CommandAction,
    repo: &RepoExecutionContext,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
    runner: &dyn CommandRunner,
    config_base: &Path,
    attachable_store: &SharedAttachableStore,
    daemon_socket_path: Option<&Path>,
    local_host: &HostName,
) -> CommandResult {
    match action {
        CommandAction::CreateWorkspaceForCheckout { checkout_path, label } => {
            let host_key = HostPath::new(local_host.clone(), checkout_path.clone());
            if !providers_data.checkouts.contains_key(&host_key) {
                return CommandResult::Error { message: format!("checkout not found: {}", checkout_path.display()) };
            }
            info!(%label, "entering workspace");
            match create_workspace_for_checkout_impl(
                &checkout_path,
                &label,
                repo,
                registry,
                config_base,
                attachable_store,
                daemon_socket_path,
                local_host,
            )
            .await
            {
                Ok(_) => CommandResult::Ok,
                Err(e) => CommandResult::Error { message: e },
            }
        }

        CommandAction::CreateWorkspaceFromPreparedTerminal { target_host, branch, checkout_path, attachable_set_id, commands } => {
            if let Some((provider_name, ws_mgr)) = preferred_workspace_manager(registry) {
                let wrapped = match wrap_remote_attach_commands(&target_host, &checkout_path, &commands, config_base) {
                    Ok(commands) => commands,
                    Err(message) => return CommandResult::Error { message },
                };
                // The workspace itself is local to the presentation host, so its
                // working directory only needs to be a valid local directory.
                // The wrapped attach commands handle entering the remote checkout path.
                let working_dir = local_workspace_directory(&repo.root, config_base);
                let remote_name = format!("{}@{}", branch, target_host);
                let mut config = workspace_config(&repo.root, &remote_name, &working_dir, "claude", config_base);
                config.resolved_commands = Some(wrapped.into_iter().map(|cmd| (cmd.role, cmd.command)).collect());
                match ws_mgr.create_workspace(&config).await {
                    Ok((ws_ref, _workspace)) => {
                        if let Some(set_id) = attachable_set_id.as_ref() {
                            persist_workspace_binding_for_set(
                                attachable_store,
                                provider_name,
                                &ws_ref,
                                set_id,
                                &target_host,
                                &checkout_path,
                            );
                        }
                    }
                    Err(e) => return CommandResult::Error { message: e },
                }
            }
            CommandResult::Ok
        }

        CommandAction::SelectWorkspace { ws_ref } => {
            info!(%ws_ref, "switching to workspace");
            if let Some(ws_mgr) = registry.workspace_managers.preferred() {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    return CommandResult::Error { message: e };
                }
            }
            CommandResult::Ok
        }

        CommandAction::Checkout { target, issue_ids, .. } => {
            let (branch, create_branch, intent) = match target {
                CheckoutTarget::Branch(branch) => (branch, false, CheckoutIntent::ExistingBranch),
                CheckoutTarget::FreshBranch(branch) => (branch, true, CheckoutIntent::FreshBranch),
            };
            if let Err(message) = validate_checkout_target(&repo.root, &branch, intent, runner).await {
                return CommandResult::Error { message };
            }
            info!(%branch, "creating checkout");
            let checkout_result = if let Some(cm) = registry.checkout_managers.preferred() {
                Some(cm.create_checkout(&repo.root, &branch, create_branch).await)
            } else {
                None
            };
            match checkout_result {
                Some(Ok((checkout_path, _checkout))) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(&repo.root, &branch, &issue_ids, runner).await;
                    }
                    info!(checkout_path = %checkout_path.display(), "created checkout");
                    CommandResult::CheckoutCreated { branch: branch.clone(), path: checkout_path }
                }
                Some(Err(e)) => {
                    error!(err = %e, "create checkout failed");
                    CommandResult::Error { message: e }
                }
                None => CommandResult::Error { message: "No checkout manager available".to_string() },
            }
        }

        CommandAction::PrepareTerminalForCheckout { checkout_path, commands: requested_commands } => {
            let host_key = HostPath::new(local_host.clone(), checkout_path.clone());
            if let Some(co) = providers_data.checkouts.get(&host_key).cloned() {
                let attachable_set_id = ensure_attachable_set_for_checkout(attachable_store, local_host, &checkout_path);
                match prepare_terminal_commands(
                    &repo.root,
                    &co.branch,
                    &checkout_path,
                    registry,
                    config_base,
                    &requested_commands,
                    attachable_store,
                    daemon_socket_path,
                )
                .await
                {
                    Ok(commands) => CommandResult::TerminalPrepared {
                        repo_identity: repo.identity.clone(),
                        target_host: local_host.clone(),
                        branch: co.branch,
                        checkout_path,
                        attachable_set_id,
                        commands,
                    },
                    Err(message) => CommandResult::Error { message },
                }
            } else {
                CommandResult::Error { message: format!("checkout not found: {}", checkout_path.display()) }
            }
        }

        CommandAction::RemoveCheckout { checkout, terminal_keys } => {
            let branch = match resolve_checkout_branch(&checkout, providers_data, local_host) {
                Ok(branch) => branch,
                Err(message) => return CommandResult::Error { message },
            };
            info!(%branch, "removing checkout");
            let result = if let Some(cm) = registry.checkout_managers.preferred() {
                Some(cm.remove_checkout(&repo.root, &branch).await)
            } else {
                None
            };
            match result {
                Some(Ok(())) => {
                    // Cascade: remove attachable sets owned by this checkout
                    let deleted_checkout_paths: Vec<HostPath> =
                        providers_data.checkouts.iter().filter(|(_, co)| co.branch == branch).map(|(hp, _)| hp.clone()).collect();

                    let mut all_session_refs = Vec::new();
                    let mut any_removed = false;
                    if let Ok(mut store) = attachable_store.lock() {
                        for checkout_path in &deleted_checkout_paths {
                            let set_ids = store.sets_for_checkout(checkout_path);
                            for set_id in set_ids {
                                if let Some(removed) = store.remove_set(&set_id) {
                                    all_session_refs.extend(removed.member_binding_refs);
                                    any_removed = true;
                                }
                            }
                        }
                        if any_removed {
                            if let Err(e) = store.save() {
                                warn!(err = %e, "failed to persist registry after cascade delete");
                            }
                        }
                    }

                    // Best-effort terminal teardown for cascade-removed sessions
                    if let Some(tp) = registry.terminal_pools.preferred() {
                        for session_ref in &all_session_refs {
                            if let Some(terminal_id) = crate::attachable::parse_terminal_session_binding_ref(session_ref) {
                                if let Err(e) = tp.kill_terminal(&terminal_id).await {
                                    warn!(
                                        session = %session_ref,
                                        err = %e,
                                        "failed to kill cascaded terminal session (best-effort)"
                                    );
                                }
                            }
                        }
                        // Also kill explicitly-passed terminal keys (existing behavior)
                        for terminal_id in &terminal_keys {
                            if let Err(e) = tp.kill_terminal(terminal_id).await {
                                warn!(
                                    terminal = %terminal_id,
                                    err = %e,
                                    "failed to kill terminal session (best-effort)"
                                );
                            }
                        }
                    }
                    CommandResult::CheckoutRemoved { branch }
                }
                Some(Err(e)) => CommandResult::Error { message: e },
                None => CommandResult::Error { message: "No checkout manager available".to_string() },
            }
        }

        CommandAction::FetchCheckoutStatus { branch, checkout_path, change_request_id } => {
            let info =
                data::fetch_checkout_status(&branch, checkout_path.as_deref(), change_request_id.as_deref(), &repo.root, runner).await;
            CommandResult::CheckoutStatus(info)
        }

        CommandAction::OpenChangeRequest { id } => {
            debug!(%id, "opening change request in browser");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.open_in_browser(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::CloseChangeRequest { id } => {
            debug!(%id, "closing change request");
            if let Some(cr) = registry.change_requests.preferred() {
                let _ = cr.close_change_request(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::OpenIssue { id } => {
            debug!(%id, "opening issue in browser");
            if let Some(it) = registry.issue_trackers.preferred() {
                let _ = it.open_in_browser(&repo.root, &id).await;
            }
            CommandResult::Ok
        }

        CommandAction::LinkIssuesToChangeRequest { change_request_id, issue_ids } => {
            info!(issue_ids = ?issue_ids, %change_request_id, "linking issues to change request");
            let body_result = run!(runner, "gh", &["pr", "view", &change_request_id, "--json", "body", "--jq", ".body",], &repo.root,);
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> = issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    let result = run!(runner, "gh", &["pr", "edit", &change_request_id, "--body", &new_body], &repo.root,);
                    match result {
                        Ok(_) => {
                            info!(%change_request_id, "linked issues to change request");
                            CommandResult::Ok
                        }
                        Err(e) => {
                            error!(err = %e, "failed to edit change request");
                            CommandResult::Error { message: e }
                        }
                    }
                }
                Err(e) => {
                    error!(err = %e, "failed to read change request body");
                    CommandResult::Error { message: e }
                }
            }
        }

        CommandAction::ArchiveSession { session_id } => archive_session_result(&session_id, registry, providers_data).await,

        CommandAction::GenerateBranchName { issue_keys } => generate_branch_name_result(&issue_keys, registry, providers_data).await,

        CommandAction::TeleportSession { session_id, branch, checkout_key } => {
            info!(%session_id, "teleporting to session");
            let teleport_cmd = match resolve_attach_command(&session_id, registry, providers_data).await {
                Ok(cmd) => cmd,
                Err(message) => return CommandResult::Error { message },
            };
            let wt_path = if let Some(ref key) = checkout_key {
                let host_key = flotilla_protocol::HostPath::new(local_host.clone(), key.clone());
                providers_data.checkouts.get(&host_key).map(|_| key.clone())
            } else if let Some(branch_name) = &branch {
                let checkout_result = if let Some(cm) = registry.checkout_managers.preferred() {
                    cm.create_checkout(&repo.root, branch_name, false).await.ok()
                } else {
                    None
                };
                checkout_result.map(|(path, _)| path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Some((provider_name, ws_mgr)) = preferred_workspace_manager(registry) {
                    let mut config = workspace_config(&repo.root, name, &path, &teleport_cmd, config_base);
                    if let Some((tp_desc, tp)) = registry.terminal_pools.preferred_with_desc() {
                        resolve_terminal_pool(&mut config, tp.as_ref(), attachable_store, &tp_desc.implementation, daemon_socket_path)
                            .await;
                    }
                    // Teleport always creates a new workspace — the attach command is
                    // session-specific, so reusing an existing workspace would attach
                    // to the wrong session.
                    match ws_mgr.create_workspace(&config).await {
                        Ok((ws_ref, _workspace)) => {
                            persist_workspace_binding(attachable_store, provider_name, &ws_ref, local_host, &path);
                        }
                        Err(e) => return CommandResult::Error { message: e },
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error { message: "Could not determine checkout path for teleport".to_string() }
            }
        }

        // These are handled at the daemon level (InProcessDaemon / SocketDaemon),
        // not by the per-repo executor. If they reach here, it's a routing bug.
        CommandAction::TrackRepoPath { .. }
        | CommandAction::UntrackRepo { .. }
        | CommandAction::Refresh { .. }
        | CommandAction::SetIssueViewport { .. }
        | CommandAction::FetchMoreIssues { .. }
        | CommandAction::SearchIssues { .. }
        | CommandAction::ClearIssueSearch { .. } => {
            CommandResult::Error { message: "bug: daemon-level command reached per-repo executor".to_string() }
        }
    }
}

async fn archive_session_result(session_id: &str, registry: &ProviderRegistry, providers_data: &ProviderData) -> CommandResult {
    if let Some(session) = providers_data.sessions.get(session_id) {
        info!(%session_id, "archiving session");
        if let Some(key) = session_provider_key(session, session_id) {
            if let Some((_, ca)) = registry.cloud_agents.get(key) {
                match ca.archive_session(session_id).await {
                    Ok(()) => CommandResult::Ok,
                    Err(e) => CommandResult::Error { message: e },
                }
            } else {
                CommandResult::Error { message: format!("No coding agent provider: {key}") }
            }
        } else {
            CommandResult::Error { message: format!("Cannot determine provider for session {session_id}") }
        }
    } else {
        CommandResult::Error { message: format!("session not found: {session_id}") }
    }
}

async fn generate_branch_name_result(issue_keys: &[String], registry: &ProviderRegistry, providers_data: &ProviderData) -> CommandResult {
    let issues: Vec<(String, String)> =
        issue_keys.iter().filter_map(|k| providers_data.issues.get(k.as_str()).map(|issue| (k.clone(), issue.title.clone()))).collect();

    let issue_id_pairs: Vec<(String, String)> = {
        let provider = registry.issue_trackers.preferred_name().map(|s| s.to_string()).unwrap_or_else(|| "issues".to_string());
        issues.iter().map(|(id, _title)| (provider.clone(), id.clone())).collect()
    };

    info!(issue_count = issue_keys.len(), "generating branch name");
    let branch_result = if let Some(ai) = registry.ai_utilities.preferred() {
        let context: Vec<String> = issues.iter().map(|(id, title)| format!("{} #{}", title, id)).collect();
        let prompt_text = if context.len() == 1 { context[0].clone() } else { context.join("; ") };
        Some(ai.generate_branch_name(&prompt_text).await)
    } else {
        None
    };

    match branch_result {
        Some(Ok(name)) => {
            info!(%name, "AI suggested");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
        Some(Err(error)) => {
            warn!(%error, "using fallback branch name after AI failure");
            let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
            let name = fallback.join("-");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
        None => {
            warn!("using fallback branch name without AI provider");
            let fallback: Vec<String> = issues.iter().map(|(id, _)| format!("issue-{}", id)).collect();
            let name = fallback.join("-");
            CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
        }
    }
}

fn resolve_checkout_branch(
    selector: &CheckoutSelector,
    providers_data: &ProviderData,
    local_host: &flotilla_protocol::HostName,
) -> Result<String, String> {
    match selector {
        CheckoutSelector::Path(path) => providers_data
            .checkouts
            .iter()
            .find(|(host_path, _)| host_path.host == *local_host && host_path.path == *path)
            .map(|(_, checkout)| checkout.branch.clone())
            .ok_or_else(|| format!("checkout not found: {}", path.display())),
        CheckoutSelector::Query(query) => {
            let matches: Vec<String> = providers_data
                .checkouts
                .iter()
                .filter(|(host_path, checkout)| {
                    host_path.host == *local_host
                        && (checkout.branch == *query
                            || checkout.branch.contains(query)
                            || host_path.path.to_string_lossy().contains(query))
                })
                .map(|(_, checkout)| checkout.branch.clone())
                .collect();
            match matches.len() {
                0 => Err(format!("checkout not found: {query}")),
                1 => Ok(matches[0].clone()),
                _ => Err(format!("checkout selector is ambiguous: {query}")),
            }
        }
    }
}

async fn validate_checkout_target(
    repo_root: &Path,
    branch: &str,
    intent: CheckoutIntent,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let local_exists = run!(runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")], repo_root).is_ok();
    let remote_exists =
        run!(runner, "git", &["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")], repo_root).is_ok();
    match intent {
        CheckoutIntent::ExistingBranch if local_exists || remote_exists => Ok(()),
        CheckoutIntent::ExistingBranch => Err(format!("branch not found: {branch}")),
        CheckoutIntent::FreshBranch if local_exists || remote_exists => Err(format!("branch already exists: {branch}")),
        CheckoutIntent::FreshBranch => Ok(()),
    }
}

/// Build the env vars to inject into a managed terminal session.
/// Ensures the attachable binding exists (creates it if needed) so the
/// env var is available on the very first attach.
fn build_terminal_env_vars(
    id: &ManagedTerminalId,
    cwd: &Path,
    command: &str,
    attachable_store: &SharedAttachableStore,
    terminal_pool_provider: &str,
    daemon_socket_path: Option<&Path>,
) -> crate::providers::terminal::TerminalEnvVars {
    use crate::attachable::{terminal_session_binding_ref, TerminalPurpose};

    let mut vars = Vec::new();

    let session_name = terminal_session_binding_ref(id);
    match attachable_store.lock() {
        Ok(mut store) => {
            // Ensure the attachable exists before looking up its ID.
            // This creates the binding on first workspace creation so the
            // env var is available immediately, not only after shpool's
            // attach_command .inspect() runs.
            let host = flotilla_protocol::HostName::local();
            let set_checkout = flotilla_protocol::HostPath::new(host.clone(), cwd.to_path_buf());
            let set_id = store.ensure_terminal_set(Some(host), Some(set_checkout));
            let attachable_id = store.ensure_terminal_attachable(
                &set_id,
                "terminal_pool",
                terminal_pool_provider,
                &session_name,
                TerminalPurpose { checkout: id.checkout.clone(), role: id.role.clone(), index: id.index },
                command,
                cwd.to_path_buf(),
                flotilla_protocol::TerminalStatus::Disconnected,
            );
            vars.push(("FLOTILLA_ATTACHABLE_ID".to_string(), attachable_id.to_string()));
            if let Err(e) = store.save() {
                warn!(err = %e, "failed to persist attachable store after env var injection");
            }
        }
        Err(e) => {
            warn!(err = %e, "attachable store lock poisoned in build_terminal_env_vars");
        }
    }

    if let Some(socket) = daemon_socket_path {
        vars.push(("FLOTILLA_DAEMON_SOCKET".to_string(), socket.display().to_string()));
    }

    vars
}

/// Resolve terminal sessions through the pool. Each terminal content entry is
/// ensured running and its attach command is stored in `config.resolved_commands`.
async fn resolve_terminal_pool(
    config: &mut WorkspaceConfig,
    terminal_pool: &dyn TerminalPool,
    attachable_store: &SharedAttachableStore,
    terminal_pool_provider: &str,
    daemon_socket_path: Option<&Path>,
) {
    let tmpl = if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
            warn!(err = %e, "failed to parse workspace template, using default");
            template::default_template()
        })
    } else {
        template::default_template()
    };
    let rendered = tmpl.render(&config.template_vars);
    info!(count = rendered.content.len(), "terminal pool: resolving content entries",);
    let mut resolved = Vec::new();
    for entry in &rendered.content {
        if entry.content_type != "terminal" {
            debug!(
                role = %entry.role,
                content_type = %entry.content_type,
                "skipping non-terminal content",
            );
            continue;
        }
        let count = entry.count.unwrap_or(1);
        for i in 0..count {
            let id = ManagedTerminalId { checkout: config.name.clone(), role: entry.role.clone(), index: i };
            if let Err(e) = terminal_pool.ensure_running(&id, &entry.command, &config.working_directory).await {
                warn!(%id, err = %e, "failed to ensure terminal");
                continue;
            }
            let env_vars = build_terminal_env_vars(
                &id,
                &config.working_directory,
                &entry.command,
                attachable_store,
                terminal_pool_provider,
                daemon_socket_path,
            );
            match terminal_pool.attach_command(&id, &entry.command, &config.working_directory, &env_vars).await {
                Ok(cmd) => {
                    debug!(%id, command = ?entry.command, resolved = ?cmd, "terminal resolved");
                    resolved.push((entry.role.clone(), cmd));
                }
                Err(e) => warn!(%id, err = %e, "failed to get attach command"),
            }
        }
    }
    info!(count = resolved.len(), "terminal pool: resolved commands");
    if !resolved.is_empty() {
        config.resolved_commands = Some(resolved);
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_terminal_commands(
    repo_root: &Path,
    branch: &str,
    checkout_path: &Path,
    registry: &ProviderRegistry,
    config_base: &Path,
    requested_commands: &[PreparedTerminalCommand],
    attachable_store: &SharedAttachableStore,
    daemon_socket_path: Option<&Path>,
) -> Result<Vec<PreparedTerminalCommand>, String> {
    if !requested_commands.is_empty() {
        // The requesting host sent its template's role→command mappings.
        // If a terminal pool is available, wrap each command through it
        // for persistent sessions. Otherwise return as-is for passthrough.
        if let Some((tp_desc, tp)) = registry.terminal_pools.preferred_with_desc() {
            let terminal_pool_provider = tp_desc.implementation.as_str();
            let mut resolved = Vec::new();
            let mut role_index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
            for cmd in requested_commands {
                let index = role_index.entry(cmd.role.clone()).or_insert(0);
                let id = ManagedTerminalId { checkout: branch.to_string(), role: cmd.role.clone(), index: *index };
                *role_index.get_mut(&cmd.role).expect("just inserted") += 1;
                if let Err(e) = tp.ensure_running(&id, &cmd.command, checkout_path).await {
                    warn!(%id, err = %e, "failed to ensure terminal");
                }
                let env_vars =
                    build_terminal_env_vars(&id, checkout_path, &cmd.command, attachable_store, terminal_pool_provider, daemon_socket_path);
                match tp.attach_command(&id, &cmd.command, checkout_path, &env_vars).await {
                    Ok(attach_cmd) => resolved.push(PreparedTerminalCommand { role: cmd.role.clone(), command: attach_cmd }),
                    Err(e) => {
                        warn!(%id, err = %e, "failed to get attach command, using original");
                        resolved.push(cmd.clone());
                    }
                }
            }
            return Ok(resolved);
        }
        return Ok(requested_commands.to_vec());
    }

    // Fallback: read the local template (for backwards compat or local use)
    let mut config = workspace_config(repo_root, branch, checkout_path, "claude", config_base);
    if let Some((tp_desc, tp)) = registry.terminal_pools.preferred_with_desc() {
        resolve_terminal_pool(&mut config, tp.as_ref(), attachable_store, &tp_desc.implementation, daemon_socket_path).await;
    }

    let commands = if let Some(resolved) = config.resolved_commands { resolved } else { render_template_commands(&config) };

    Ok(commands.into_iter().map(|(role, command)| PreparedTerminalCommand { role, command }).collect())
}

fn render_template_commands(config: &WorkspaceConfig) -> Vec<(String, String)> {
    let tmpl = if let Some(ref yaml) = config.template_yaml {
        serde_yml::from_str::<WorkspaceTemplate>(yaml).unwrap_or_else(|e| {
            warn!(err = %e, "failed to parse workspace template, using default");
            template::default_template()
        })
    } else {
        template::default_template()
    };

    let rendered = tmpl.render(&config.template_vars);
    let mut commands = Vec::new();
    for entry in &rendered.content {
        if entry.content_type != "terminal" {
            continue;
        }
        let count = entry.count.unwrap_or(1);
        for _ in 0..count {
            commands.push((entry.role.clone(), entry.command.clone()));
        }
    }
    commands
}

fn wrap_remote_attach_commands(
    target_host: &HostName,
    checkout_path: &Path,
    commands: &[PreparedTerminalCommand],
    config_base: &Path,
) -> Result<Vec<PreparedTerminalCommand>, String> {
    let info = remote_ssh_info(target_host, config_base)?;
    let remote_dir = checkout_path.display().to_string();

    let multiplex_args = if info.multiplex {
        let ctrl_dir = config_base.join("ssh");
        if let Err(e) = std::fs::create_dir_all(&ctrl_dir) {
            tracing::warn!(err = %e, "failed to create SSH control socket directory, disabling multiplexing");
            String::new()
        } else {
            let ctrl_path = ctrl_dir.join("ctrl-%r@%h-%p");
            format!(" -o ControlMaster=auto -o ControlPath={} -o ControlPersist=60", shell_quote(&ctrl_path.display().to_string()),)
        }
    } else {
        String::new()
    };

    Ok(commands
        .iter()
        .map(|entry| {
            let inner = if entry.command.is_empty() {
                // Empty command = open a login shell at the remote directory
                format!("cd {} && exec $SHELL -l", shell_quote(&remote_dir))
            } else {
                format!("cd {} && {}", shell_quote(&remote_dir), entry.command)
            };
            let login_wrapped = format!("$SHELL -l -c \"{}\"", escape_for_double_quotes(&inner));
            PreparedTerminalCommand {
                role: entry.role.clone(),
                command: format!("ssh -t{} {} {}", multiplex_args, shell_quote(&info.target), shell_quote(&login_wrapped)),
            }
        })
        .collect())
}

struct RemoteSshInfo {
    target: String,
    multiplex: bool,
}

fn remote_ssh_info(target_host: &HostName, config_base: &Path) -> Result<RemoteSshInfo, String> {
    let config = crate::config::ConfigStore::with_base(config_base);
    let hosts = config.load_hosts()?;
    let (label, remote) = hosts
        .hosts
        .iter()
        .find(|(_, host)| host.expected_host_name == target_host.as_str())
        .ok_or_else(|| format!("unknown remote host: {target_host}"))?;
    let target = match &remote.user {
        Some(user) => format!("{user}@{}", remote.hostname),
        None => remote.hostname.clone(),
    };
    let multiplex = hosts.resolved_ssh_multiplex(label);
    Ok(RemoteSshInfo { target, multiplex })
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn escape_for_double_quotes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' | '$' | '`' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn session_provider_key<'a>(session: &'a CloudAgentSession, session_id: &str) -> Option<&'a str> {
    session.correlation_keys.iter().find_map(|k| match k {
        CorrelationKey::SessionRef(provider, id) if id == session_id => Some(provider.as_str()),
        _ => None,
    })
}

async fn resolve_attach_command(session_id: &str, registry: &ProviderRegistry, providers_data: &ProviderData) -> Result<String, String> {
    let provider_key = providers_data
        .sessions
        .get(session_id)
        .and_then(|s| session_provider_key(s, session_id))
        .ok_or_else(|| format!("Cannot determine provider for session {session_id}"))?;

    let (_, ca) = registry.cloud_agents.get(provider_key).ok_or_else(|| format!("No coding agent provider: {provider_key}"))?;

    ca.attach_command(session_id).await
}
/// Build a WorkspaceConfig from repo/branch/dir/command.
pub(crate) fn workspace_config(
    repo_root: &Path,
    name: &str,
    working_dir: &Path,
    main_command: &str,
    config_base: &Path,
) -> WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = config_base.join("workspace.yaml");
        std::fs::read_to_string(global_path).ok()
    });
    let mut template_vars = std::collections::HashMap::new();
    template_vars.insert("main_command".to_string(), main_command.to_string());
    WorkspaceConfig {
        name: name.to_string(),
        working_directory: working_dir.to_path_buf(),
        template_vars,
        template_yaml,
        resolved_commands: None,
    }
}

fn local_workspace_directory(repo_root: &Path, config_base: &Path) -> PathBuf {
    if repo_root.exists() {
        return repo_root.to_path_buf();
    }
    if let Some(home) = dirs::home_dir() {
        return home;
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    config_base.to_path_buf()
}

/// Write branch-to-issue links into git config.
async fn write_branch_issue_links(repo_root: &Path, branch: &str, issue_ids: &[(String, String)], runner: &dyn CommandRunner) {
    use std::collections::HashMap;
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(e) = run!(runner, "git", &["config", &key, &value], repo_root) {
            tracing::warn!(err = %e, "failed to write issue link");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use super::*;
    use crate::{
        attachable::{AttachableStore, SharedAttachableStore},
        providers::{
            ai_utility::AiUtility,
            change_request::ChangeRequestTracker,
            coding_agent::CloudAgentService,
            discovery::{ProviderCategory, ProviderDescriptor},
            issue_tracker::IssueTracker,
            testing::MockRunner,
            types::*,
            vcs::CheckoutManager,
            workspace::WorkspaceManager,
        },
    };

    fn desc(name: &str) -> ProviderDescriptor {
        ProviderDescriptor::named(ProviderCategory::Vcs, name)
    }
    use async_trait::async_trait;
    use flotilla_protocol::{HostName, HostPath, RepoSelector};

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::local(), PathBuf::from(path))
    }

    // -----------------------------------------------------------------------
    // Mock providers
    // -----------------------------------------------------------------------

    /// A mock CheckoutManager that returns a canned checkout or error.
    struct MockCheckoutManager {
        create_result: tokio::sync::Mutex<Option<Result<(PathBuf, Checkout), String>>>,
        remove_result: tokio::sync::Mutex<Option<Result<(), String>>>,
    }

    impl MockCheckoutManager {
        fn succeeding(branch: &str, path: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Ok((PathBuf::from(path), Checkout {
                    branch: branch.to_string(),
                    is_main: false,
                    trunk_ahead_behind: None,
                    remote_ahead_behind: None,
                    working_tree: None,
                    last_commit: None,
                    correlation_keys: vec![],
                    association_keys: vec![],
                })))),
                remove_result: tokio::sync::Mutex::new(Some(Ok(()))),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
                remove_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            }
        }
    }

    #[async_trait]
    impl CheckoutManager for MockCheckoutManager {
        async fn list_checkouts(&self, _repo_root: &Path) -> Result<Vec<(PathBuf, Checkout)>, String> {
            Ok(vec![])
        }
        async fn create_checkout(&self, _repo_root: &Path, _branch: &str, _create_branch: bool) -> Result<(PathBuf, Checkout), String> {
            self.create_result.lock().await.take().expect("create_checkout called more than expected")
        }
        async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
            self.remove_result.lock().await.take().expect("remove_checkout called more than expected")
        }
    }

    /// A mock WorkspaceManager that records calls and returns configurable results.
    struct MockWorkspaceManager {
        existing: Vec<(String, Workspace)>,
        create_result: tokio::sync::Mutex<Result<(), String>>,
        select_result: tokio::sync::Mutex<Result<(), String>>,
        created_configs: tokio::sync::Mutex<Vec<WorkspaceConfig>>,
        calls: tokio::sync::Mutex<Vec<String>>,
    }

    impl MockWorkspaceManager {
        fn succeeding() -> Self {
            Self {
                existing: vec![],
                create_result: tokio::sync::Mutex::new(Ok(())),
                select_result: tokio::sync::Mutex::new(Ok(())),
                created_configs: tokio::sync::Mutex::new(Vec::new()),
                calls: tokio::sync::Mutex::new(vec![]),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                existing: vec![],
                create_result: tokio::sync::Mutex::new(Err(msg.to_string())),
                select_result: tokio::sync::Mutex::new(Err(msg.to_string())),
                created_configs: tokio::sync::Mutex::new(Vec::new()),
                calls: tokio::sync::Mutex::new(vec![]),
            }
        }

        fn with_existing(existing: Vec<(String, Workspace)>) -> Self {
            Self {
                existing,
                create_result: tokio::sync::Mutex::new(Ok(())),
                select_result: tokio::sync::Mutex::new(Ok(())),
                created_configs: tokio::sync::Mutex::new(Vec::new()),
                calls: tokio::sync::Mutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl WorkspaceManager for MockWorkspaceManager {
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            self.calls.lock().await.push("list_workspaces".to_string());
            Ok(self.existing.clone())
        }
        async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<(String, Workspace), String> {
            self.created_configs.lock().await.push(config.clone());
            self.calls.lock().await.push(format!("create_workspace:{}", config.name));
            let result = self.create_result.lock().await;
            match &*result {
                Ok(()) => Ok(("mock-ref".to_string(), Workspace {
                    name: config.name.clone(),
                    directories: vec![],
                    correlation_keys: vec![],
                    attachable_set_id: None,
                })),
                Err(e) => Err(e.clone()),
            }
        }
        async fn select_workspace(&self, ws_ref: &str) -> Result<(), String> {
            self.calls.lock().await.push(format!("select_workspace:{ws_ref}"));
            let result = self.select_result.lock().await;
            result.clone()
        }
    }

    /// A mock ChangeRequestTracker provider.
    struct MockChangeRequestTracker;

    #[async_trait]
    impl ChangeRequestTracker for MockChangeRequestTracker {
        async fn list_change_requests(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
            Ok(vec![])
        }
        async fn get_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(String, ChangeRequest), String> {
            Err("not implemented".to_string())
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn close_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_merged_branch_names(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    /// A mock IssueTracker provider.
    struct MockIssueTracker;

    #[async_trait]
    impl IssueTracker for MockIssueTracker {
        async fn list_issues(&self, _repo_root: &Path, _limit: usize) -> Result<Vec<(String, Issue)>, String> {
            Ok(vec![])
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// A mock CloudAgentService provider.
    struct MockCloudAgent {
        archive_result: tokio::sync::Mutex<Result<(), String>>,
        attach_command: String,
    }

    impl MockCloudAgent {
        fn succeeding() -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: "mock-attach-cmd".to_string() }
        }

        fn failing(msg: &str) -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Err(msg.to_string())), attach_command: "mock-attach-cmd".to_string() }
        }

        fn with_attach(attach_command: &str) -> Self {
            Self { archive_result: tokio::sync::Mutex::new(Ok(())), attach_command: attach_command.to_string() }
        }
    }

    #[async_trait]
    impl CloudAgentService for MockCloudAgent {
        async fn list_sessions(&self, _criteria: &RepoCriteria) -> Result<Vec<(String, CloudAgentSession)>, String> {
            Ok(vec![])
        }
        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            let result = self.archive_result.lock().await;
            result.clone()
        }
        async fn attach_command(&self, session_id: &str) -> Result<String, String> {
            Ok(format!("{} {session_id}", self.attach_command))
        }
    }

    /// A mock AiUtility provider.
    struct MockAiUtility {
        result: tokio::sync::Mutex<Result<String, String>>,
    }

    impl MockAiUtility {
        fn succeeding(name: &str) -> Self {
            Self { result: tokio::sync::Mutex::new(Ok(name.to_string())) }
        }

        fn failing(msg: &str) -> Self {
            Self { result: tokio::sync::Mutex::new(Err(msg.to_string())) }
        }
    }

    #[async_trait]
    impl AiUtility for MockAiUtility {
        async fn generate_branch_name(&self, _context: &str) -> Result<String, String> {
            let result = self.result.lock().await;
            result.clone()
        }
    }

    // -----------------------------------------------------------------------
    // Helper to build test fixtures
    // -----------------------------------------------------------------------

    fn empty_registry() -> ProviderRegistry {
        ProviderRegistry::new()
    }

    fn empty_data() -> ProviderData {
        ProviderData::default()
    }

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/test-repo")
    }

    fn config_base() -> PathBuf {
        PathBuf::from("/tmp/test-config")
    }

    fn make_checkout(branch: &str, _path: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn make_session_for(provider: &str, id: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: "test session".to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![CorrelationKey::SessionRef(provider.to_string(), id.to_string())],
            provider_name: String::new(),
            provider_display_name: String::new(),
            item_noun: String::new(),
        }
    }

    fn make_issue(_id: &str, title: &str) -> Issue {
        Issue {
            title: title.to_string(),
            labels: vec![],
            association_keys: vec![],
            provider_name: String::new(),
            provider_display_name: String::new(),
        }
    }

    fn runner_ok() -> MockRunner {
        MockRunner::new(vec![])
    }

    fn repo_selector() -> RepoSelector {
        RepoSelector::Path(repo_root())
    }

    fn local_command(action: CommandAction) -> Command {
        Command { host: None, context_repo: None, action }
    }

    fn local_host() -> HostName {
        HostName::local()
    }

    fn repo_identity() -> flotilla_protocol::RepoIdentity {
        flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
    }

    fn fresh_checkout_action(branch: &str) -> CommandAction {
        CommandAction::Checkout { repo: repo_selector(), target: CheckoutTarget::FreshBranch(branch.to_string()), issue_ids: vec![] }
    }

    fn existing_branch_checkout_action(branch: &str) -> CommandAction {
        CommandAction::Checkout { repo: repo_selector(), target: CheckoutTarget::Branch(branch.to_string()), issue_ids: vec![] }
    }

    fn remove_checkout_action(branch: &str, terminal_keys: Vec<ManagedTerminalId>) -> CommandAction {
        CommandAction::RemoveCheckout { checkout: CheckoutSelector::Query(branch.to_string()), terminal_keys }
    }

    fn test_attachable_store(base: &Path) -> SharedAttachableStore {
        crate::attachable::shared_file_backed_attachable_store(base)
    }

    async fn run_execute(
        action: CommandAction,
        registry: &ProviderRegistry,
        providers_data: &ProviderData,
        runner: &MockRunner,
    ) -> CommandResult {
        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        };
        let config_base = config_base();
        let attachable_store = test_attachable_store(&config_base);
        execute(action, &repo, registry, providers_data, runner, &config_base, &attachable_store, None, &local_host()).await
    }

    fn assert_error_contains(result: CommandResult, expected_substring: &str) {
        match result {
            CommandResult::Error { message } => {
                assert!(message.contains(expected_substring), "expected error containing {expected_substring:?}, got {message:?}");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_error_eq(result: CommandResult, expected: &str) {
        match result {
            CommandResult::Error { message } => assert_eq!(message, expected),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_checkout_created_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutCreated { branch, .. } => {
                assert_eq!(branch, expected_branch);
            }
            other => panic!("expected CheckoutCreated, got {:?}", other),
        }
    }

    fn assert_checkout_status_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutStatus(info) => {
                assert_eq!(info.branch, expected_branch);
            }
            other => panic!("expected CheckoutStatus, got {:?}", other),
        }
    }

    fn assert_checkout_removed_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutRemoved { branch } => {
                assert_eq!(branch, expected_branch);
            }
            other => panic!("expected CheckoutRemoved, got {:?}", other),
        }
    }

    fn assert_branch_name_generated(result: CommandResult, expected_name: &str, expected_issue_ids: &[(&str, &str)]) {
        match result {
            CommandResult::BranchNameGenerated { name, issue_ids } => {
                assert_eq!(name, expected_name);
                let expected_issue_ids: Vec<_> =
                    expected_issue_ids.iter().map(|(provider, id)| (provider.to_string(), id.to_string())).collect();
                assert_eq!(issue_ids, expected_issue_ids);
            }
            other => panic!("expected BranchNameGenerated, got {:?}", other),
        }
    }

    fn assert_ok(result: CommandResult) {
        assert!(matches!(result, CommandResult::Ok));
    }

    // -----------------------------------------------------------------------
    // Tests: CreateWorkspaceForCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_workspace_for_checkout_not_found() {
        let registry = empty_registry();
        let data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
                .await;

        assert_error_contains(result, "checkout not found");
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_success_without_ws_manager() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let path = PathBuf::from("/repo/wt-feat");
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
                .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_uses_provider_from_session_ref() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("wrong provider")));
        registry.cloud_agents.insert("cursor", desc("cursor"), Arc::new(MockCloudAgent::succeeding()));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_success_with_ws_manager() {
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let path = PathBuf::from("/repo/wt-feat");
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
                .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_persists_workspace_binding() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let mut data = empty_data();
        let checkout_path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        };

        let result = execute(
            CommandAction::CreateWorkspaceForCheckout { checkout_path: checkout_path.clone(), label: "feat".into() },
            &repo,
            &registry,
            &data,
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let store = AttachableStore::with_base(temp.path());
        let object_id = store
            .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
            .expect("workspace binding should exist");
        let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
        assert_eq!(set.checkout, Some(HostPath::new(local_host(), checkout_path)));
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws creation failed")));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let path = PathBuf::from("/repo/wt-feat");
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path: path, label: "feat".into() }, &registry, &data, &runner)
                .await;

        assert_error_eq(result, "ws creation failed");
    }
    #[tokio::test]
    async fn prepare_terminal_for_checkout_returns_terminal_commands() {
        let registry = empty_registry();
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
            &registry,
            &data,
            &runner,
        )
        .await;

        match result {
            CommandResult::TerminalPrepared { repo_identity, target_host, branch, checkout_path, attachable_set_id, commands } => {
                assert_eq!(repo_identity, flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
                assert_eq!(target_host, HostName::local());
                assert_eq!(branch, "feat");
                assert_eq!(checkout_path, path);
                assert!(attachable_set_id.is_some(), "prepare should allocate an attachable set");
                assert_eq!(commands, vec![PreparedTerminalCommand { role: "main".into(), command: "claude".into() }]);
            }
            other => panic!("expected TerminalPrepared, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_terminal_for_checkout_includes_attachable_set_id_when_present() {
        let registry = empty_registry();
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        {
            let mut store = attachable_store.lock().expect("store lock");
            let ensured_set_id = store.ensure_terminal_set(Some(local_host()), Some(HostPath::new(local_host(), path.clone())));
            store.save().expect("save attachable store");
            assert_eq!(
                store.registry().sets.get(&ensured_set_id).and_then(|set| set.checkout.clone()),
                Some(HostPath::new(local_host(), path.clone()))
            );
        }

        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        };

        let result = execute(
            CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
            &repo,
            &registry,
            &data,
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        match result {
            CommandResult::TerminalPrepared { attachable_set_id, .. } => {
                let set_id = attachable_set_id.expect("attachable set id");
                let store = AttachableStore::with_base(temp.path());
                assert!(store.registry().sets.contains_key(&set_id), "prepare should reuse persisted set");
            }
            other => panic!("expected TerminalPrepared, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prepare_terminal_for_checkout_creates_and_persists_attachable_set() {
        let registry = empty_registry();
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        };

        let result = execute(
            CommandAction::PrepareTerminalForCheckout { checkout_path: path.clone(), commands: vec![] },
            &repo,
            &registry,
            &data,
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        let set_id = match result {
            CommandResult::TerminalPrepared { attachable_set_id, .. } => attachable_set_id.expect("attachable set id"),
            other => panic!("expected TerminalPrepared, got {other:?}"),
        };

        let store = AttachableStore::with_base(temp.path());
        let set = store.registry().sets.get(&set_id).expect("set should exist");
        assert_eq!(set.checkout, Some(HostPath::new(local_host(), path)));
        assert!(temp.path().join("attachables").join("registry.json").exists(), "registry should be written");
    }

    #[tokio::test]
    async fn create_workspace_from_prepared_terminal_wraps_remote_commands_in_ssh() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo_root = temp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("create repo root");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root.clone(),
        };
        let result = execute(
            CommandAction::CreateWorkspaceFromPreparedTerminal {
                target_host: HostName::new("desktop"),
                branch: "feat".into(),
                checkout_path: PathBuf::from("/remote/feat"),
                attachable_set_id: None,
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
            },
            &repo,
            &registry,
            &empty_data(),
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let created = workspace_manager.created_configs.lock().await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].working_directory, repo_root);
        let resolved = created[0].resolved_commands.as_ref().expect("resolved commands");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "main");
        assert!(resolved[0].1.contains("ssh -t"));
        assert!(resolved[0].1.contains("desktop.local"));
        assert!(resolved[0].1.contains("/remote/feat"));
        assert!(resolved[0].1.contains("bash -l"));
        assert!(resolved[0].1.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
    }

    #[tokio::test]
    async fn create_workspace_from_prepared_terminal_prefixes_name_with_host() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo_root = temp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("create repo root");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root,
        };
        let result = execute(
            CommandAction::CreateWorkspaceFromPreparedTerminal {
                target_host: HostName::new("desktop"),
                branch: "feat".into(),
                checkout_path: PathBuf::from("/remote/feat"),
                attachable_set_id: None,
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }],
            },
            &repo,
            &registry,
            &empty_data(),
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let created = workspace_manager.created_configs.lock().await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].name, "feat@desktop", "workspace name should be branch@host");
    }

    #[tokio::test]
    async fn create_workspace_from_prepared_terminal_persists_remote_attachable_set_binding() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo_root = temp.path().join("repo");
        std::fs::create_dir_all(&repo_root).expect("create repo root");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root,
        };
        let set_id = flotilla_protocol::AttachableSetId::new("set-remote");
        let result = execute(
            CommandAction::CreateWorkspaceFromPreparedTerminal {
                target_host: HostName::new("desktop"),
                branch: "feat".into(),
                checkout_path: PathBuf::from("/remote/feat"),
                attachable_set_id: Some(set_id.clone()),
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }],
            },
            &repo,
            &registry,
            &empty_data(),
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let store = AttachableStore::with_base(temp.path());
        let object_id = store
            .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
            .expect("workspace binding should exist");
        assert_eq!(object_id, set_id.as_str());
        let set = store.registry().sets.get(&set_id).expect("set should exist");
        assert_eq!(set.checkout, Some(HostPath::new(HostName::new("desktop"), PathBuf::from("/remote/feat"))));
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_selects_existing_workspace() {
        let checkout_path = PathBuf::from("/repo/wt-feat");
        let existing_workspace = Workspace {
            name: "feat".to_string(),
            directories: vec![checkout_path.clone()],
            correlation_keys: vec![],
            attachable_set_id: None,
        };
        let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:42".to_string(), existing_workspace)]));

        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::CreateWorkspaceForCheckout { checkout_path, label: "feat".into() }, &registry, &data, &runner).await;

        assert_ok(result);
        let calls = ws_mgr.calls.lock().await;
        assert!(calls.contains(&"list_workspaces".to_string()), "should call list_workspaces, got: {calls:?}");
        assert!(calls.contains(&"select_workspace:workspace:42".to_string()), "should select existing workspace, got: {calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("create_workspace")), "should NOT create workspace, got: {calls:?}");
    }

    #[tokio::test]
    async fn checkout_action_does_not_create_workspace() {
        let checkout_path = PathBuf::from("/repo/wt-feat-x");
        let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:99".to_string(), Workspace {
            name: "feat-x".to_string(),
            directories: vec![checkout_path.clone()],
            correlation_keys: vec![],
            attachable_set_id: None,
        })]));

        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

        let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

        assert_checkout_created_branch(result, "feat-x");
        let calls = ws_mgr.calls.lock().await;
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("list_workspaces") || c.starts_with("select_workspace") || c.starts_with("create_workspace")),
            "checkout should not touch workspaces, got: {calls:?}"
        );
    }

    #[tokio::test]
    async fn create_workspace_from_prepared_terminal_uses_local_fallback_for_remote_only_repo() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: PathBuf::from("<remote>/desktop/home/dev/repo"),
        };
        let result = execute(
            CommandAction::CreateWorkspaceFromPreparedTerminal {
                target_host: HostName::new("desktop"),
                branch: "feat".into(),
                checkout_path: PathBuf::from("/remote/feat"),
                attachable_set_id: None,
                commands: vec![PreparedTerminalCommand { role: "main".into(), command: "bash -l".into() }],
            },
            &repo,
            &registry,
            &empty_data(),
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let created = workspace_manager.created_configs.lock().await;
        assert_eq!(created.len(), 1);
        assert!(!created[0].working_directory.to_string_lossy().starts_with("<remote>/"));
        assert!(created[0].working_directory.exists(), "fallback working directory should exist");
        let resolved = created[0].resolved_commands.as_ref().expect("resolved commands");
        assert!(resolved[0].1.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", resolved[0].1);
    }

    #[tokio::test]
    async fn teleport_session_creates_workspace_even_when_one_exists() {
        // Teleport must always create a new workspace because the attach command
        // is session-specific. Reusing an existing workspace would attach to
        // whatever session was there before, not the requested one.
        let checkout_path = PathBuf::from("/repo/wt-feat");
        let existing_workspace = Workspace {
            name: "feat".to_string(),
            directories: vec![checkout_path.clone()],
            correlation_keys: vec![],
            attachable_set_id: None,
        };
        let ws_mgr = Arc::new(MockWorkspaceManager::with_existing(vec![("workspace:77".to_string(), existing_workspace)]));

        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.workspace_managers.insert("cmux", desc("cmux"), ws_mgr.clone());
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: Some("feat".to_string()),
                checkout_key: Some(checkout_path),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
        let calls = ws_mgr.calls.lock().await;
        assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "teleport should always create a new workspace, got: {calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("select_workspace")), "teleport should NOT select existing workspace, got: {calls:?}");
    }

    #[tokio::test]
    async fn teleport_session_persists_workspace_binding() {
        let workspace_manager = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&workspace_manager) as Arc<dyn WorkspaceManager>);
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();
        let temp = tempfile::tempdir().expect("tempdir");
        let attachable_store = test_attachable_store(temp.path());
        let repo = RepoExecutionContext {
            identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            root: repo_root(),
        };

        let result = execute(
            CommandAction::TeleportSession {
                session_id: "sess-1".into(),
                branch: Some("feat".into()),
                checkout_key: Some(PathBuf::from("/repo/wt-feat")),
            },
            &repo,
            &registry,
            &data,
            &runner,
            temp.path(),
            &attachable_store,
            None,
            &local_host(),
        )
        .await;

        assert_ok(result);
        let store = AttachableStore::with_base(temp.path());
        let object_id = store
            .lookup_binding("workspace_manager", "cmux", BindingObjectKind::AttachableSet, "mock-ref")
            .expect("workspace binding should exist");
        let set = store.registry().sets.values().find(|set| set.id.as_str() == object_id).expect("set should exist");
        assert_eq!(set.checkout, Some(HostPath::new(local_host(), PathBuf::from("/repo/wt-feat"))));
    }
    // -----------------------------------------------------------------------
    // Tests: SelectWorkspace
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn select_workspace_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_success() {
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let runner = runner_ok();

        let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "my-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_failure() {
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("select failed")));
        let runner = runner_ok();

        let result = run_execute(CommandAction::SelectWorkspace { ws_ref: "bad-ws".to_string() }, &registry, &empty_data(), &runner).await;

        assert_error_eq(result, "select failed");
    }

    // -----------------------------------------------------------------------
    // Tests: CreateCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_checkout_no_manager() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

        let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn create_checkout_success() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

        let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_with_issue_ids_writes_git_config() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        // Two validation probes (branch absent locally/remotely), then the git config write.
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string()), Ok(String::new())]);

        let result = run_execute(
            CommandAction::Checkout {
                repo: repo_selector(),
                target: CheckoutTarget::FreshBranch("feat-x".to_string()),
                issue_ids: vec![("github".to_string(), "42".to_string())],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("branch already exists")));
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

        let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

        assert_error_eq(result, "branch already exists");
    }

    #[tokio::test]
    async fn create_checkout_success_ws_manager_fails_still_returns_created() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
        let runner = MockRunner::new(vec![Err("missing".to_string()), Err("missing".to_string())]);

        let result = run_execute(fresh_checkout_action("feat-x"), &registry, &empty_data(), &runner).await;

        // Workspace failure is logged but checkout still reports success
        assert_checkout_created_branch(result, "feat-x");
    }

    // -----------------------------------------------------------------------
    // Tests: RemoveCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn remove_checkout_no_manager() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
        let runner = runner_ok();

        let result = run_execute(remove_checkout_action("old", vec![]), &registry, &data, &runner).await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn remove_checkout_success() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
        let runner = runner_ok();

        let result = run_execute(remove_checkout_action("old", vec![]), &registry, &data, &runner).await;

        assert_checkout_removed_branch(result, "old");
    }

    #[tokio::test]
    async fn remove_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::failing("cannot remove trunk")));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-main"), make_checkout("main", "/repo/wt-main"));
        let runner = runner_ok();

        let result = run_execute(remove_checkout_action("main", vec![]), &registry, &data, &runner).await;

        assert_error_eq(result, "cannot remove trunk");
    }

    // -----------------------------------------------------------------------
    // Tests: RemoveCheckout — terminal cleanup
    // -----------------------------------------------------------------------

    struct MockTerminalPool {
        killed: tokio::sync::Mutex<Vec<ManagedTerminalId>>,
    }

    #[async_trait]
    impl TerminalPool for MockTerminalPool {
        async fn list_terminals(&self) -> Result<Vec<flotilla_protocol::ManagedTerminal>, String> {
            Ok(vec![])
        }
        async fn ensure_running(&self, _id: &ManagedTerminalId, _cmd: &str, _cwd: &Path) -> Result<(), String> {
            Ok(())
        }
        async fn attach_command(
            &self,
            _id: &ManagedTerminalId,
            _cmd: &str,
            _cwd: &Path,
            _env_vars: &crate::providers::terminal::TerminalEnvVars,
        ) -> Result<String, String> {
            Ok(String::new())
        }
        async fn kill_terminal(&self, id: &ManagedTerminalId) -> Result<(), String> {
            self.killed.lock().await.push(id.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn remove_checkout_kills_correlated_terminals() {
        let terminal_id = ManagedTerminalId { checkout: "feat-x".into(), role: "shell".into(), index: 0 };
        let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });

        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

        let runner = runner_ok();
        let result = run_execute(remove_checkout_action("feat-x", vec![terminal_id.clone()]), &registry, &data, &runner).await;

        assert_checkout_removed_branch(result, "feat-x");
        let killed = mock_pool.killed.lock().await;
        assert_eq!(killed.len(), 1);
        assert_eq!(killed[0], terminal_id);
    }

    #[tokio::test]
    async fn remove_checkout_cascades_attachable_set_deletion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_base = temp.path().to_path_buf();
        let attachable_store = test_attachable_store(&config_base);
        let host = local_host();

        // Pre-populate the store with a set and members
        {
            let mut store = attachable_store.lock().expect("lock store");
            let checkout_path = HostPath::new(host.clone(), "/repo/wt-feat-x");
            let set_id = store.ensure_terminal_set(Some(host.clone()), Some(checkout_path));
            store.ensure_terminal_attachable(
                &set_id,
                "terminal_pool",
                "shpool",
                "flotilla/feat-x/shell/0",
                crate::attachable::TerminalPurpose { checkout: "feat-x".into(), role: "shell".into(), index: 0 },
                "bash",
                PathBuf::from("/repo/wt-feat-x"),
                flotilla_protocol::TerminalStatus::Running,
            );
        }

        let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.terminal_pools.insert("shpool", desc("shpool"), Arc::clone(&mock_pool) as Arc<dyn TerminalPool>);
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));

        let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };
        let runner = runner_ok();
        let result = execute(
            remove_checkout_action("feat-x", vec![]),
            &repo,
            &registry,
            &data,
            &runner,
            &config_base,
            &attachable_store,
            None,
            &host,
        )
        .await;

        assert_checkout_removed_branch(result, "feat-x");

        // Verify set and members were removed from store
        {
            let store = attachable_store.lock().expect("lock store");
            assert!(store.registry().sets.is_empty(), "set should be removed");
            assert!(store.registry().attachables.is_empty(), "attachables should be removed");
            assert!(store.registry().bindings.is_empty(), "bindings should be removed");
        }

        // Verify terminal was killed via cascade
        let killed = mock_pool.killed.lock().await;
        assert_eq!(killed.len(), 1, "cascade should kill the terminal");
        assert_eq!(killed[0].checkout, "feat-x");
    }

    // -----------------------------------------------------------------------
    // Tests: FetchCheckoutStatus
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_checkout_status_returns_checkout_status() {
        let registry = empty_registry();
        // fetch_checkout_status runs multiple git/gh commands concurrently via
        // tokio::join!. Provide enough error responses for all subprocess calls:
        //   - git rev-parse (upstream) -> Err
        //   - git rev-parse (origin/HEAD) -> Err
        //   - git status --porcelain -> Err
        //   - gh pr view -> Err
        let runner = MockRunner::new(vec![Err("err".to_string()), Err("err".to_string()), Err("err".to_string()), Err("err".to_string())]);

        let result = run_execute(
            CommandAction::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: Some("42".to_string()),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_status_branch(result, "feat");
    }

    #[tokio::test]
    async fn fetch_checkout_status_populates_uncommitted_files() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Err("err".to_string()),
            Err("err".to_string()),
            Ok(" M src/main.rs\n?? TODO.txt\n".to_string()),
            Err("err".to_string()),
        ]);

        let result = run_execute(
            CommandAction::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: None,
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        match result {
            CommandResult::CheckoutStatus(info) => {
                assert!(info.has_uncommitted);
                assert_eq!(info.uncommitted_files, vec![" M src/main.rs".to_string(), "?? TODO.txt".to_string(),]);
            }
            other => panic!("expected CheckoutStatus, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Tests: OpenChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_change_request_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(CommandAction::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_change_request_with_provider() {
        let mut registry = empty_registry();
        registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
        let runner = runner_ok();

        let result = run_execute(CommandAction::OpenChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: CloseChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn close_change_request_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(CommandAction::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn close_change_request_with_provider() {
        let mut registry = empty_registry();
        registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
        let runner = runner_ok();

        let result = run_execute(CommandAction::CloseChangeRequest { id: "42".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: OpenIssue
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_issue_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(CommandAction::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_issue_with_provider() {
        let mut registry = empty_registry();
        registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
        let runner = runner_ok();

        let result = run_execute(CommandAction::OpenIssue { id: "10".to_string() }, &registry, &empty_data(), &runner).await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: LinkIssuesToChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn link_issues_success_with_existing_body() {
        let registry = empty_registry();
        // First call: gh pr view returns existing body
        // Second call: gh pr edit succeeds
        let runner = MockRunner::new(vec![Ok("Existing PR body".to_string()), Ok(String::new())]);

        let result = run_execute(
            CommandAction::LinkIssuesToChangeRequest {
                change_request_id: "55".to_string(),
                issue_ids: vec!["10".to_string(), "20".to_string()],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_success_with_empty_body() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Ok("  \n".to_string()), // empty/whitespace body
            Ok(String::new()),      // edit succeeds
        ]);

        let result = run_execute(
            CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_view_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

        let result = run_execute(
            CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "gh not found");
    }

    #[tokio::test]
    async fn link_issues_edit_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Ok("body text".to_string()), Err("permission denied".to_string())]);

        let result = run_execute(
            CommandAction::LinkIssuesToChangeRequest { change_request_id: "55".to_string(), issue_ids: vec!["10".to_string()] },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "permission denied");
    }

    // -----------------------------------------------------------------------
    // Tests: ArchiveSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn archive_session_not_found() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::ArchiveSession { session_id: "nonexistent".to_string() }, &registry, &empty_data(), &runner).await;

        assert_error_contains(result, "session not found");
    }

    #[tokio::test]
    async fn archive_session_no_agent_provider() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_error_contains(result, "No coding agent provider: claude");
    }

    #[tokio::test]
    async fn archive_session_success() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_agent_fails() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::failing("archive failed")));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, &registry, &data, &runner).await;

        assert_error_eq(result, "archive failed");
    }

    // -----------------------------------------------------------------------
    // Tests: GenerateBranchName
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn generate_branch_name_ai_success() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login")));
        registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

        assert_branch_name_generated(result, "feat/add-login", &[("github", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_ai_failure_uses_fallback() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::failing("API error")));
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, &registry, &data, &runner).await;

        assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_no_ai_provider_uses_fallback() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.issues.insert("7".to_string(), make_issue("7", "Fix bug"));
        let runner = runner_ok();

        let result = run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["7".to_string()] }, &registry, &data, &runner).await;

        // No issue tracker registered, defaults to "issues"
        assert_branch_name_generated(result, "issue-7", &[("issues", "7")]);
    }

    #[tokio::test]
    async fn generate_branch_name_multiple_issues() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/login-and-signup")));
        registry.issue_trackers.insert("github", desc("github"), Arc::new(MockIssueTracker));
        let mut data = empty_data();
        data.issues.insert("1".to_string(), make_issue("1", "Login feature"));
        data.issues.insert("2".to_string(), make_issue("2", "Signup feature"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::GenerateBranchName { issue_keys: vec!["1".to_string(), "2".to_string()] },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_branch_name_generated(result, "feat/login-and-signup", &[("github", "1"), ("github", "2")]);
    }

    #[tokio::test]
    async fn generate_branch_name_unknown_issue_key() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result =
            run_execute(CommandAction::GenerateBranchName { issue_keys: vec!["nonexistent".to_string()] }, &registry, &data, &runner).await;

        // No issues found, so empty fallback
        assert_branch_name_generated(result, "", &[]);
    }

    // -----------------------------------------------------------------------
    // Tests: TeleportSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn teleport_session_with_checkout_key() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert(
            "claude",
            desc("claude"),
            Arc::new(MockCloudAgent::with_attach("claude --teleport")), // base; mock appends session_id
        );
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_uses_provider_specific_attach_command() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::with_attach("claude --teleport")));
        registry.cloud_agents.insert("cursor", desc("cursor"), Arc::new(MockCloudAgent::with_attach("agent --resume")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("cursor", "sess-1"));
        let runner = runner_ok();

        let attach = resolve_attach_command("sess-1", &registry, &data).await.expect("resolve attach command");
        assert_eq!(attach, "agent --resume sess-1");

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_with_branch_creates_checkout() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat", "/repo/wt-feat")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: None },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_no_path_no_branch() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: None },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_contains(result, "Could not determine checkout path");
    }

    #[tokio::test]
    async fn teleport_session_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::failing("ws failed")));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_eq(result, "ws failed");
    }

    #[tokio::test]
    async fn teleport_session_uses_session_as_name_when_no_branch() {
        // When checkout_key is present but branch is None, uses "session" as name.
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: None, checkout_key: Some(path) },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: Daemon-level commands rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn daemon_level_commands_return_error() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let daemon_commands = vec![
            CommandAction::TrackRepoPath { path: PathBuf::from("/repo") },
            CommandAction::UntrackRepo { repo: RepoSelector::Path(PathBuf::from("/repo")) },
            CommandAction::Refresh { repo: None },
            CommandAction::SetIssueViewport { repo: RepoSelector::Path(PathBuf::from("/repo")), visible_count: 10 },
            CommandAction::FetchMoreIssues { repo: RepoSelector::Path(PathBuf::from("/repo")), desired_count: 20 },
            CommandAction::SearchIssues { repo: RepoSelector::Path(PathBuf::from("/repo")), query: "bug".to_string() },
            CommandAction::ClearIssueSearch { repo: RepoSelector::Path(PathBuf::from("/repo")) },
        ];

        for cmd in daemon_commands {
            let result = run_execute(cmd, &registry, &data, &runner).await;
            assert_error_contains(result, "daemon-level command");
        }
    }

    // -----------------------------------------------------------------------
    // Tests: workspace_config helper
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_config_builds_correct_struct() {
        let config = workspace_config(Path::new("/nonexistent-repo"), "my-branch", Path::new("/repo/wt"), "claude", &config_base());

        assert_eq!(config.name, "my-branch");
        assert_eq!(config.working_directory, PathBuf::from("/repo/wt"));
        assert_eq!(config.template_vars.get("main_command"), Some(&"claude".to_string()));
        assert!(config.template_yaml.is_none(), "no template file should exist at test paths");
    }

    // -----------------------------------------------------------------------
    // Helper to run build_plan with Arc-wrapped arguments
    // -----------------------------------------------------------------------

    async fn run_build_plan(
        action: CommandAction,
        registry: ProviderRegistry,
        providers_data: ProviderData,
        runner: MockRunner,
    ) -> ExecutionPlan {
        let config_base = config_base();
        build_plan(
            local_command(action),
            RepoExecutionContext {
                identity: flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
                root: repo_root(),
            },
            Arc::new(registry),
            Arc::new(providers_data),
            Arc::new(runner),
            config_base.clone(),
            test_attachable_store(&config_base),
            None,
            local_host(),
            None,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Tests: build_plan
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn build_plan_create_checkout_returns_steps() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let data = empty_data();
        let runner = runner_ok();

        let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                assert_eq!(step_plan.steps.len(), 2, "checkout + workspace steps");
                assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
                assert_eq!(step_plan.steps[1].description, "Create workspace");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_create_checkout_skips_existing() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        // Pre-populate with an existing checkout for the branch
        data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));
        let runner = runner_ok();

        let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, data, runner).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                assert_eq!(step_plan.steps.len(), 2, "checkout + workspace steps");
                assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
                assert_eq!(step_plan.steps[1].description, "Create workspace");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn checkout_plan_includes_workspace_step() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));

        let plan = run_build_plan(fresh_checkout_action("feat-x"), registry, empty_data(), runner_ok()).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                assert_eq!(step_plan.steps.len(), 2, "expected checkout + workspace steps");
                assert_eq!(step_plan.steps[0].description, "Create checkout for branch feat-x");
                assert_eq!(step_plan.steps[1].description, "Create workspace");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps"),
        }
    }

    #[tokio::test]
    async fn checkout_plan_end_to_end_creates_workspace() {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;

        use crate::step::run_step_plan;

        let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
        let registry = Arc::new(registry);
        let runner = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
        let cb = config_base();
        let attachable = test_attachable_store(&cb);
        let lh = local_host();
        let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

        let plan = build_plan(
            local_command(fresh_checkout_action("feat-x")),
            RepoExecutionContext { identity: repo_identity(), root: repo_root() },
            Arc::clone(&registry),
            Arc::new(empty_data()),
            runner,
            cb.clone(),
            attachable.clone(),
            None,
            lh.clone(),
            None,
        )
        .await;

        let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
        let resolver = ExecutorStepResolver {
            repo,
            registry,
            config_base: cb,
            attachable_store: attachable,
            daemon_socket_path: None,
            local_host: lh.clone(),
        };

        let result = match plan {
            ExecutionPlan::Steps(step_plan) => {
                run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await
            }
            _ => panic!("expected steps"),
        };

        assert!(matches!(result, CommandResult::CheckoutCreated { .. }));

        let calls = ws_mgr.calls.lock().await;
        assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should create workspace from prior outcome: {calls:?}");
    }

    #[tokio::test]
    async fn checkout_plan_creates_workspace_for_preexisting_checkout() {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;

        use crate::step::run_step_plan;

        let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = ProviderRegistry::new();
        // No checkout manager needed — checkout already exists
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
        let registry = Arc::new(registry);
        // validate_checkout_target needs 2 responses: local ref check (Ok), remote ref check
        let runner = Arc::new(MockRunner::new(vec![Ok("".into()), Err("missing".into())]));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat-x"), make_checkout("feat-x", "/repo/wt-feat-x"));
        let cb = config_base();
        let attachable = test_attachable_store(&cb);
        let lh = local_host();
        let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

        let plan = build_plan(
            local_command(existing_branch_checkout_action("feat-x")),
            RepoExecutionContext { identity: repo_identity(), root: repo_root() },
            Arc::clone(&registry),
            Arc::new(data),
            runner,
            cb.clone(),
            attachable.clone(),
            None,
            lh.clone(),
            None,
        )
        .await;

        let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
        let resolver = ExecutorStepResolver {
            repo,
            registry,
            config_base: cb,
            attachable_store: attachable,
            daemon_socket_path: None,
            local_host: lh.clone(),
        };

        let result = match plan {
            ExecutionPlan::Steps(step_plan) => {
                run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await
            }
            _ => panic!("expected steps"),
        };

        assert!(
            matches!(result, CommandResult::CheckoutCreated { ref branch, .. } if branch == "feat-x"),
            "should return CheckoutCreated for pre-existing checkout, got: {result:?}"
        );
        let calls = ws_mgr.calls.lock().await;
        assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should create workspace for pre-existing checkout: {calls:?}");
    }

    #[tokio::test]
    async fn checkout_plan_preserves_checkout_created_when_workspace_step_fails() {
        use tokio::sync::broadcast;
        use tokio_util::sync::CancellationToken;

        use crate::step::run_step_plan;

        let ws_mgr = Arc::new(MockWorkspaceManager::failing("ws failed"));
        let mut registry = ProviderRegistry::new();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);
        let registry = Arc::new(registry);
        let runner = Arc::new(MockRunner::new(vec![Err("missing".into()), Err("missing".into())]));
        let cb = config_base();
        let attachable = test_attachable_store(&cb);
        let lh = local_host();
        let repo = RepoExecutionContext { identity: repo_identity(), root: repo_root() };

        let plan = build_plan(
            local_command(fresh_checkout_action("feat-x")),
            RepoExecutionContext { identity: repo_identity(), root: repo_root() },
            Arc::clone(&registry),
            Arc::new(empty_data()),
            runner,
            cb.clone(),
            attachable.clone(),
            None,
            lh.clone(),
            None,
        )
        .await;

        let (cancel, tx) = (CancellationToken::new(), broadcast::channel(64).0);
        let resolver = ExecutorStepResolver {
            repo,
            registry,
            config_base: cb,
            attachable_store: attachable,
            daemon_socket_path: None,
            local_host: lh.clone(),
        };

        let result = match plan {
            ExecutionPlan::Steps(step_plan) => {
                run_step_plan(step_plan, 1, lh, repo_identity(), repo_root(), cancel, tx, Some(&resolver)).await
            }
            _ => panic!("expected steps"),
        };

        assert_eq!(result, CommandResult::CheckoutCreated { branch: "feat-x".into(), path: PathBuf::from("/repo/wt-feat-x") });
    }

    #[tokio::test]
    async fn build_plan_teleport_session_returns_steps() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::new(MockWorkspaceManager::succeeding()));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat", "/repo/wt-feat"));
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let plan = run_build_plan(
            CommandAction::TeleportSession { session_id: "sess-1".to_string(), branch: Some("feat".to_string()), checkout_key: Some(path) },
            registry,
            data,
            runner,
        )
        .await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // 3 steps: resolve attach, ensure checkout, create workspace
                assert_eq!(step_plan.steps.len(), 3, "expected 3 steps, got {}", step_plan.steps.len());
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_remove_checkout_returns_steps() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert("wt", desc("wt"), Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")));
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-old"), make_checkout("old", "/repo/wt-old"));
        let runner = runner_ok();

        let plan = run_build_plan(remove_checkout_action("old", vec![]), registry, data, runner).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                // At least 1 step: remove checkout
                assert!(!step_plan.steps.is_empty(), "expected at least 1 step");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_archive_session_returns_steps() {
        let mut registry = empty_registry();
        registry.cloud_agents.insert("claude", desc("claude"), Arc::new(MockCloudAgent::succeeding()));
        let mut data = empty_data();
        data.sessions.insert("sess-1".to_string(), make_session_for("claude", "sess-1"));
        let runner = runner_ok();

        let plan = run_build_plan(CommandAction::ArchiveSession { session_id: "sess-1".to_string() }, registry, data, runner).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                assert_eq!(step_plan.steps.len(), 1, "expected a single archive step");
                assert_eq!(step_plan.steps[0].description, "Archive session sess-1");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_generate_branch_name_returns_steps() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert("claude", desc("claude"), Arc::new(MockAiUtility::succeeding("feat/add-login")));
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
        let runner = runner_ok();

        let plan = run_build_plan(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, registry, data, runner).await;

        match plan {
            ExecutionPlan::Steps(step_plan) => {
                assert_eq!(step_plan.steps.len(), 1, "expected a single branch-name step");
                assert_eq!(step_plan.steps[0].description, "Generate branch name");
            }
            ExecutionPlan::Immediate(_) => panic!("expected Steps, got Immediate"),
        }
    }

    #[tokio::test]
    async fn build_plan_archive_session_missing_session_returns_immediate_error() {
        let registry = empty_registry();
        let runner = runner_ok();

        let plan =
            run_build_plan(CommandAction::ArchiveSession { session_id: "missing".to_string() }, registry, empty_data(), runner).await;

        match plan {
            ExecutionPlan::Immediate(CommandResult::Error { message }) => {
                assert!(message.contains("session not found"), "unexpected message: {message}");
            }
            ExecutionPlan::Immediate(other) => panic!("expected Error result, got {other:?}"),
            ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
        }
    }

    #[tokio::test]
    async fn build_plan_generate_branch_name_without_ai_returns_immediate_fallback() {
        let mut data = empty_data();
        data.issues.insert("42".to_string(), make_issue("42", "Add login feature"));
        let runner = runner_ok();

        let plan =
            run_build_plan(CommandAction::GenerateBranchName { issue_keys: vec!["42".to_string()] }, empty_registry(), data, runner).await;

        match plan {
            ExecutionPlan::Immediate(CommandResult::BranchNameGenerated { name, issue_ids }) => {
                assert_eq!(name, "issue-42");
                assert_eq!(issue_ids, vec![("issues".to_string(), "42".to_string())]);
            }
            ExecutionPlan::Immediate(other) => panic!("expected BranchNameGenerated, got {other:?}"),
            ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
        }
    }

    #[tokio::test]
    async fn build_plan_simple_command_returns_immediate() {
        let mut registry = empty_registry();
        registry.change_requests.insert("github", desc("github"), Arc::new(MockChangeRequestTracker));
        let runner = runner_ok();

        let plan = run_build_plan(CommandAction::OpenChangeRequest { id: "42".to_string() }, registry, empty_data(), runner).await;

        match plan {
            ExecutionPlan::Immediate(result) => {
                assert_ok(result);
            }
            ExecutionPlan::Steps(_) => panic!("expected Immediate, got Steps"),
        }
    }

    // -----------------------------------------------------------------------
    // Tests: resolve_checkout_branch
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_checkout_branch_path_found() {
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-branch", "/repo/wt-feat"));
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/repo/wt-feat")), &data, &local_host);

        assert_eq!(result.expect("path lookup should succeed"), "feat-branch");
    }

    #[test]
    fn resolve_checkout_branch_path_not_found() {
        let data = empty_data();
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Path(PathBuf::from("/nonexistent")), &data, &local_host);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("checkout not found"));
    }

    #[test]
    fn resolve_checkout_branch_query_exact_match() {
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login", "/repo/wt-feat"));
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Query("feat-login".to_string()), &data, &local_host);

        assert_eq!(result.expect("exact query should match"), "feat-login");
    }

    #[test]
    fn resolve_checkout_branch_query_substring_match() {
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login-page", "/repo/wt-feat"));
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Query("login".to_string()), &data, &local_host);

        assert_eq!(result.expect("substring query should match"), "feat-login-page");
    }

    #[test]
    fn resolve_checkout_branch_query_not_found() {
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat"), make_checkout("feat-login", "/repo/wt-feat"));
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Query("nonexistent".to_string()), &data, &local_host);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("checkout not found"));
    }

    #[test]
    fn resolve_checkout_branch_query_ambiguous() {
        let mut data = empty_data();
        data.checkouts.insert(hp("/repo/wt-feat-a"), make_checkout("feat-a", "/repo/wt-feat-a"));
        data.checkouts.insert(hp("/repo/wt-feat-b"), make_checkout("feat-b", "/repo/wt-feat-b"));
        let local_host = HostName::local();

        let result = resolve_checkout_branch(&CheckoutSelector::Query("feat".to_string()), &data, &local_host);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ambiguous"));
    }

    // -----------------------------------------------------------------------
    // Tests: resolve_terminal_pool
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resolve_terminal_pool_no_template_uses_default() {
        let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
        let mut config = WorkspaceConfig {
            name: "test-branch".to_string(),
            working_directory: PathBuf::from("/repo/wt"),
            template_vars: [("main_command".to_string(), "claude".to_string())].into_iter().collect(),
            template_yaml: None,
            resolved_commands: None,
        };

        resolve_terminal_pool(&mut config, mock_pool.as_ref(), &crate::attachable::shared_in_memory_attachable_store(), "shpool", None)
            .await;

        // Default template has one "main" terminal entry
        assert!(config.resolved_commands.is_some());
        let commands = config.resolved_commands.expect("default template should produce resolved commands");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "main");
    }

    #[tokio::test]
    async fn resolve_terminal_pool_skips_non_terminal_content() {
        let mock_pool = Arc::new(MockTerminalPool { killed: tokio::sync::Mutex::new(vec![]) });
        let yaml = r#"
content:
  - role: docs
    type: webview
    command: "http://localhost:3000"
"#;
        let mut config = WorkspaceConfig {
            name: "test-branch".to_string(),
            working_directory: PathBuf::from("/repo/wt"),
            template_vars: std::collections::HashMap::new(),
            template_yaml: Some(yaml.to_string()),
            resolved_commands: None,
        };

        resolve_terminal_pool(&mut config, mock_pool.as_ref(), &crate::attachable::shared_in_memory_attachable_store(), "shpool", None)
            .await;

        // All content entries were non-terminal, so resolved_commands stays None
        assert!(config.resolved_commands.is_none());
    }

    // -----------------------------------------------------------------------
    // Tests: build_terminal_env_vars
    // -----------------------------------------------------------------------

    #[test]
    fn build_terminal_env_vars_creates_binding_and_populates_both_vars() {
        let store = crate::attachable::shared_in_memory_attachable_store();
        let id = ManagedTerminalId { checkout: "feat".into(), role: "agent".into(), index: 0 };
        let cwd = std::path::Path::new("/repo/feat");
        let socket = std::path::PathBuf::from("/tmp/flotilla.sock");

        let vars = build_terminal_env_vars(&id, cwd, "claude", &store, "shpool", Some(&socket));

        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].0, "FLOTILLA_ATTACHABLE_ID");
        assert!(!vars[0].1.is_empty());
        assert_eq!(vars[1].0, "FLOTILLA_DAEMON_SOCKET");
        assert_eq!(vars[1].1, "/tmp/flotilla.sock");

        // Calling again returns the same attachable ID (idempotent)
        let vars2 = build_terminal_env_vars(&id, cwd, "claude", &store, "shpool", Some(&socket));
        assert_eq!(vars[0].1, vars2[0].1);
    }

    #[test]
    fn build_terminal_env_vars_without_socket_only_has_attachable_id() {
        let store = crate::attachable::shared_in_memory_attachable_store();
        let id = ManagedTerminalId { checkout: "feat".into(), role: "shell".into(), index: 0 };
        let vars = build_terminal_env_vars(&id, std::path::Path::new("/repo"), "$SHELL", &store, "shpool", None);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].0, "FLOTILLA_ATTACHABLE_ID");
    }

    // -----------------------------------------------------------------------
    // Tests: write_branch_issue_links
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn write_branch_issue_links_single_provider_multiple_issues() {
        let runner = MockRunner::new(vec![Ok(String::new())]);
        let issue_ids = vec![("github".to_string(), "10".to_string()), ("github".to_string(), "20".to_string())];

        write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

        assert_eq!(runner.remaining(), 0, "single provider should consume exactly 1 response");
    }

    #[tokio::test]
    async fn write_branch_issue_links_multiple_providers() {
        let runner = MockRunner::new(vec![Ok(String::new()), Ok(String::new())]);
        let issue_ids = vec![("github".to_string(), "10".to_string()), ("jira".to_string(), "PROJ-5".to_string())];

        write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

        assert_eq!(runner.remaining(), 0, "two providers should consume exactly 2 responses");
    }

    #[tokio::test]
    async fn write_branch_issue_links_git_error_tolerated() {
        let runner = MockRunner::new(vec![Err("git config failed".to_string())]);
        let issue_ids = vec![("github".to_string(), "10".to_string())];

        write_branch_issue_links(&repo_root(), "feat-x", &issue_ids, &runner).await;

        assert_eq!(runner.remaining(), 0, "should still consume the response even on error");
    }

    #[tokio::test]
    async fn write_branch_issue_links_empty_is_noop() {
        let runner = MockRunner::new(vec![]);

        write_branch_issue_links(&repo_root(), "feat-x", &[], &runner).await;

        assert_eq!(runner.remaining(), 0, "empty issue_ids should make zero calls");
    }

    // -----------------------------------------------------------------------
    // Tests: validate_checkout_target
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn validate_fresh_branch_succeeds_when_neither_exists() {
        // local check -> Err (not found), remote check -> Err (not found)
        let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

        let result = validate_checkout_target(&repo_root(), "new-branch", CheckoutIntent::FreshBranch, &runner).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_fresh_branch_fails_when_local_exists() {
        // local check -> Ok (found), remote check -> Err (not found)
        let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

        let result = validate_checkout_target(&repo_root(), "existing", CheckoutIntent::FreshBranch, &runner).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn validate_fresh_branch_fails_when_remote_exists() {
        // local check -> Err (not found), remote check -> Ok (found)
        let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

        let result = validate_checkout_target(&repo_root(), "remote-only", CheckoutIntent::FreshBranch, &runner).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[tokio::test]
    async fn validate_existing_branch_succeeds_when_local_exists() {
        // local check -> Ok (found), remote check -> Err (not found)
        let runner = MockRunner::new(vec![Ok(String::new()), Err("not found".to_string())]);

        let result = validate_checkout_target(&repo_root(), "local-branch", CheckoutIntent::ExistingBranch, &runner).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_existing_branch_succeeds_when_remote_exists() {
        // local check -> Err (not found), remote check -> Ok (found)
        let runner = MockRunner::new(vec![Err("not found".to_string()), Ok(String::new())]);

        let result = validate_checkout_target(&repo_root(), "remote-branch", CheckoutIntent::ExistingBranch, &runner).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn validate_existing_branch_fails_when_neither_exists() {
        // local check -> Err (not found), remote check -> Err (not found)
        let runner = MockRunner::new(vec![Err("not found".to_string()), Err("not found".to_string())]);

        let result = validate_checkout_target(&repo_root(), "ghost-branch", CheckoutIntent::ExistingBranch, &runner).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("branch not found"));
    }

    #[test]
    fn wrap_remote_attach_commands_uses_login_shell() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "claude".into() }];
        let result =
            wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "main");
        assert!(result[0].command.contains("$SHELL -l -c"), "expected login shell wrapper, got: {}", result[0].command);
        assert!(result[0].command.contains("ssh -t"), "expected ssh -t, got: {}", result[0].command);
        assert!(result[0].command.contains("desktop.local"), "expected host, got: {}", result[0].command);
        assert!(result[0].command.contains("/home/dev/project"), "expected remote dir, got: {}", result[0].command);
        assert!(result[0].command.contains("claude"), "expected command, got: {}", result[0].command);
    }

    #[test]
    fn escape_for_double_quotes_handles_special_chars() {
        assert_eq!(escape_for_double_quotes("hello"), "hello");
        assert_eq!(escape_for_double_quotes(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape_for_double_quotes("$HOME"), r"\$HOME");
        assert_eq!(escape_for_double_quotes("a`cmd`b"), r"a\`cmd\`b");
        assert_eq!(escape_for_double_quotes(r"back\slash"), r"back\\slash");
        assert_eq!(escape_for_double_quotes(""), "");
        assert_eq!(
            escape_for_double_quotes("shpool --socket /tmp/s.sock attach flotilla/feat/main/0"),
            "shpool --socket /tmp/s.sock attach flotilla/feat/main/0"
        );
    }

    #[test]
    fn wrap_remote_attach_commands_includes_multiplex_args() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
        let result =
            wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

        // Default is multiplex=true
        assert!(result[0].command.contains("ControlMaster=auto"), "expected ControlMaster, got: {}", result[0].command);
        assert!(result[0].command.contains("ControlPersist=60"), "expected ControlPersist, got: {}", result[0].command);
    }

    #[test]
    fn wrap_remote_attach_commands_omits_multiplex_when_disabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[ssh]\nmultiplex = false\n\n[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\n",
        )
        .expect("write hosts config");

        let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
        let result =
            wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

        assert!(!result[0].command.contains("ControlMaster"), "should not have ControlMaster when disabled, got: {}", result[0].command);
    }

    #[test]
    fn wrap_remote_attach_commands_per_host_multiplex_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[ssh]\nmultiplex = true\n\n[hosts.desktop]\nhostname = \"desktop.local\"\nexpected_host_name = \"desktop\"\ndaemon_socket = \"/tmp/flotilla.sock\"\nssh_multiplex = false\n",
        )
        .expect("write hosts config");

        let commands = vec![PreparedTerminalCommand { role: "main".into(), command: "bash".into() }];
        let result =
            wrap_remote_attach_commands(&HostName::new("desktop"), &PathBuf::from("/home/dev/project"), &commands, temp.path()).unwrap();

        assert!(!result[0].command.contains("ControlMaster"), "per-host override should disable multiplex, got: {}", result[0].command);
    }

    // -----------------------------------------------------------------------
    // Tests: ExecutorStepResolver
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn executor_step_resolver_creates_workspace() {
        let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);

        let config_base = config_base();
        let resolver = ExecutorStepResolver {
            repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
            registry: Arc::new(registry),
            config_base: config_base.clone(),
            attachable_store: test_attachable_store(&config_base),
            daemon_socket_path: None,
            local_host: local_host(),
        };

        let prior = vec![StepOutcome::CompletedWith(CommandResult::CheckoutCreated {
            branch: "feat".into(),
            path: PathBuf::from("/repo/wt-feat"),
        })];
        let action = StepAction::CreateWorkspaceForCheckout { label: "feat".into() };
        let outcome = resolver.resolve("create workspace", action, &prior).await;
        assert!(outcome.is_ok(), "resolve should succeed: {outcome:?}");

        let calls = ws_mgr.calls.lock().await;
        assert!(calls.iter().any(|c| c.starts_with("create_workspace")), "should call create_workspace, got: {calls:?}");
    }

    #[tokio::test]
    async fn executor_step_resolver_skips_when_no_checkout_path() {
        let ws_mgr = Arc::new(MockWorkspaceManager::succeeding());
        let mut registry = empty_registry();
        registry.workspace_managers.insert("cmux", desc("cmux"), Arc::clone(&ws_mgr) as Arc<dyn WorkspaceManager>);

        let config_base = config_base();
        let resolver = ExecutorStepResolver {
            repo: RepoExecutionContext { identity: repo_identity(), root: repo_root() },
            registry: Arc::new(registry),
            config_base: config_base.clone(),
            attachable_store: test_attachable_store(&config_base),
            daemon_socket_path: None,
            local_host: local_host(),
        };

        let action = StepAction::CreateWorkspaceForCheckout { label: "feat".into() };
        let outcome = resolver.resolve("create workspace", action, &[]).await;
        assert!(matches!(outcome, Ok(StepOutcome::Skipped)), "should skip when no prior CheckoutCreated outcome: {outcome:?}");

        let calls = ws_mgr.calls.lock().await;
        assert!(calls.is_empty(), "should not call workspace manager when no checkout path in prior outcomes, got: {calls:?}");
    }
}
