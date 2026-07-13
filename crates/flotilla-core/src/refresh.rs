use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    future::Future,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_protocol::{
    qualified_path::{HostId, PathQualifier, QualifiedPath},
    CorrelationKey, EnvironmentId, HostName,
};
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
    time::Instant,
};

use crate::{
    attachable::{BindingObjectKind, SharedAttachableStore},
    data::{self, CorrelationResult, RefreshError},
    path_context::ExecutionEnvironmentPath,
    provider_data::ProviderData,
    providers::{correlation::CorrelatedGroup, registry::ProviderRegistry, types::RepoCriteria},
};

/// Result of a single background refresh cycle.
#[derive(Debug, Clone)]
pub struct RefreshSnapshot {
    pub providers: Arc<ProviderData>,
    pub work_items: Vec<CorrelationResult>,
    pub correlation_groups: Vec<CorrelatedGroup>,
    pub errors: Vec<RefreshError>,
    pub provider_health: HashMap<(&'static str, String), bool>,
}

impl Default for RefreshSnapshot {
    fn default() -> Self {
        Self {
            providers: Arc::new(ProviderData::default()),
            work_items: Vec::new(),
            correlation_groups: Vec::new(),
            errors: Vec::new(),
            provider_health: HashMap::new(),
        }
    }
}

pub struct RepoRefreshHandle {
    pub refresh_trigger: Arc<Notify>,
    pub snapshot_rx: watch::Receiver<Arc<RefreshSnapshot>>,
    _task_handle: JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct RefreshCadence {
    interval: Duration,
    offset: Duration,
}

#[derive(Clone, Copy)]
enum RefreshKind {
    Full,
    Fast,
    Slow,
}

#[derive(Clone, Copy)]
pub(crate) struct RefreshSchedule {
    startup_offset: Duration,
    fast: RefreshCadence,
    slow: RefreshCadence,
}

impl RefreshSchedule {
    pub(crate) fn for_repo(repo_root: &Path, fast_interval: Duration, slow_interval: Duration) -> Self {
        let mut hasher = DefaultHasher::new();
        repo_root.hash(&mut hasher);
        let hash = hasher.finish();
        Self {
            startup_offset: stagger_offset(hash.rotate_left(41), Duration::from_secs(1)),
            fast: RefreshCadence { interval: fast_interval, offset: stagger_offset(hash, fast_interval) },
            slow: RefreshCadence { interval: slow_interval, offset: stagger_offset(hash.rotate_left(17), slow_interval) },
        }
    }

    #[cfg(test)]
    fn without_stagger(fast_interval: Duration, slow_interval: Duration) -> Self {
        Self {
            startup_offset: Duration::ZERO,
            fast: RefreshCadence { interval: fast_interval, offset: Duration::ZERO },
            slow: RefreshCadence { interval: slow_interval, offset: Duration::ZERO },
        }
    }
}

impl From<Duration> for RefreshSchedule {
    fn from(interval: Duration) -> Self {
        Self::without_stagger_for_interval(interval)
    }
}

impl RefreshSchedule {
    fn without_stagger_for_interval(interval: Duration) -> Self {
        Self {
            startup_offset: Duration::ZERO,
            fast: RefreshCadence { interval, offset: Duration::ZERO },
            slow: RefreshCadence { interval, offset: Duration::ZERO },
        }
    }
}

fn stagger_offset(hash: u64, interval: Duration) -> Duration {
    let interval_nanos = interval.as_nanos();
    if interval_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos((u128::from(hash) % interval_nanos) as u64)
}

fn first_tick(cadence: RefreshCadence) -> Instant {
    Instant::now() + cadence.interval + cadence.offset
}

pub(crate) fn normalize_checkout_publication(path: QualifiedPath, host_id: Option<&HostId>, _host_name: &HostName) -> QualifiedPath {
    match host_id {
        Some(host_id) => QualifiedPath::host(host_id.clone(), path.path),
        None => match path.qualifier {
            PathQualifier::Host(host) => QualifiedPath::host(host, path.path),
            PathQualifier::Environment(env) => QualifiedPath::environment(env, path.path),
            PathQualifier::HostName(host) => QualifiedPath::from_host_name(&host, path.path),
        },
    }
}

pub(crate) fn normalize_checkout_correlation_keys(
    keys: Vec<CorrelationKey>,
    host_id: Option<&HostId>,
    host_name: &HostName,
) -> Vec<CorrelationKey> {
    keys.into_iter()
        .map(|key| match key {
            CorrelationKey::CheckoutPath(path) => CorrelationKey::CheckoutPath(normalize_checkout_publication(path, host_id, host_name)),
            other => other,
        })
        .collect()
}

impl RepoRefreshHandle {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        environment_id: Option<EnvironmentId>,
        host_id: Option<HostId>,
        attachable_store: SharedAttachableStore,
        agent_state_store: crate::agents::SharedAgentStateStore,
        interval: Duration,
    ) -> Self {
        Self::spawn_with_schedule(
            repo_root,
            registry,
            criteria,
            environment_id,
            host_id,
            attachable_store,
            agent_state_store,
            interval.into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_with_schedule(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        environment_id: Option<EnvironmentId>,
        host_id: Option<HostId>,
        attachable_store: SharedAttachableStore,
        agent_state_store: crate::agents::SharedAgentStateStore,
        schedule: RefreshSchedule,
    ) -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());
        let trigger = refresh_trigger.clone();

        let task_handle = tokio::spawn(async move {
            if !schedule.startup_offset.is_zero() {
                tokio::time::sleep(schedule.startup_offset).await;
            }
            let mut provider_data = ProviderData::default();
            let mut fast_errors = Vec::new();
            let mut slow_errors = Vec::new();
            let initial = run_refresh_cycle(
                RefreshKind::Full,
                &mut provider_data,
                &mut fast_errors,
                &mut slow_errors,
                &repo_root,
                &registry,
                &criteria,
                environment_id.as_ref(),
                host_id.as_ref(),
                &attachable_store,
                &agent_state_store,
            )
            .await;
            if snapshot_tx.send(initial).is_err() {
                return;
            }

            let mut fast_timer = tokio::time::interval_at(first_tick(schedule.fast), schedule.fast.interval);
            fast_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut slow_timer = tokio::time::interval_at(first_tick(schedule.slow), schedule.slow.interval);
            slow_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let kind = tokio::select! {
                    _ = fast_timer.tick() => RefreshKind::Fast,
                    _ = slow_timer.tick() => RefreshKind::Slow,
                    _ = trigger.notified() => RefreshKind::Full,
                };
                let snapshot = run_refresh_cycle(
                    kind,
                    &mut provider_data,
                    &mut fast_errors,
                    &mut slow_errors,
                    &repo_root,
                    &registry,
                    &criteria,
                    environment_id.as_ref(),
                    host_id.as_ref(),
                    &attachable_store,
                    &agent_state_store,
                )
                .await;

                // Publish — receivers will see has_changed().
                // Break if receiver is dropped (handle dropped without Drop running).
                if snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });

        Self { refresh_trigger, snapshot_rx, _task_handle: task_handle }
    }

    /// Create a dormant refresh handle that never polls providers.
    ///
    /// Used for virtual (remote-only) repos where provider data arrives
    /// via PeerData messages rather than local filesystem polling.
    pub fn idle() -> Self {
        let (_snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());

        // Spawn a task that just parks forever — it will be aborted on Drop.
        let task_handle = tokio::spawn(std::future::pending::<()>());

        Self { refresh_trigger, snapshot_rx, _task_handle: task_handle }
    }

    pub fn trigger_refresh(&self) {
        self.refresh_trigger.notify_one();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_refresh_cycle(
    kind: RefreshKind,
    provider_data: &mut ProviderData,
    fast_errors: &mut Vec<RefreshError>,
    slow_errors: &mut Vec<RefreshError>,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
    environment_id: Option<&EnvironmentId>,
    host_id: Option<&HostId>,
    attachable_store: &SharedAttachableStore,
    agent_state_store: &crate::agents::SharedAgentStateStore,
) -> Arc<RefreshSnapshot> {
    match kind {
        RefreshKind::Full => {
            let mut fast_data = provider_data.clone();
            let mut slow_data = provider_data.clone();
            let (new_fast_errors, new_slow_errors) = tokio::join!(
                refresh_fast_providers(&mut fast_data, repo_root, registry, environment_id, host_id, attachable_store, agent_state_store,),
                refresh_slow_providers(&mut slow_data, repo_root, registry, criteria, host_id),
            );
            install_fast_provider_data(provider_data, fast_data);
            install_slow_provider_data(provider_data, slow_data);
            *fast_errors = new_fast_errors;
            *slow_errors = new_slow_errors;
        }
        RefreshKind::Fast => {
            *fast_errors =
                refresh_fast_providers(provider_data, repo_root, registry, environment_id, host_id, attachable_store, agent_state_store)
                    .await;
        }
        RefreshKind::Slow => {
            *slow_errors = refresh_slow_providers(provider_data, repo_root, registry, criteria, host_id).await;
        }
    }
    let errors: Vec<_> = fast_errors.iter().chain(slow_errors.iter()).cloned().collect();
    let provider_health = compute_provider_health(registry, &errors);
    let providers = Arc::new(provider_data.clone());
    let (work_items, correlation_groups) = data::correlate(&providers);
    Arc::new(RefreshSnapshot { providers, work_items, correlation_groups, errors, provider_health })
}

fn install_fast_provider_data(target: &mut ProviderData, source: ProviderData) {
    target.checkouts = source.checkouts;
    target.workspaces = source.workspaces;
    target.managed_terminals = source.managed_terminals;
    target.attachable_sets = source.attachable_sets;
    target.agents = source.agents;
}

fn install_slow_provider_data(target: &mut ProviderData, source: ProviderData) {
    target.change_requests = source.change_requests;
    target.sessions = source.sessions;
    target.branches = source.branches;
}

impl Drop for RepoRefreshHandle {
    fn drop(&mut self) {
        self._task_handle.abort();
    }
}

/// Collect results from parallel provider requests, separating successes from errors.
async fn collect_named_results<T, Fut>(requests: Vec<(String, Fut)>) -> (Vec<T>, Vec<(String, String)>)
where
    Fut: Future<Output = Result<Vec<T>, String>>,
{
    let results = futures::future::join_all(requests.into_iter().map(|(name, fut)| async move { (name, fut.await) })).await;

    let mut entries = Vec::new();
    let mut errs = Vec::new();
    for (name, result) in results {
        match result {
            Ok(mut items) => entries.append(&mut items),
            Err(e) => errs.push((name, e)),
        }
    }
    (entries, errs)
}

fn provider_has_error(errors: &[RefreshError], provider: &str, categories: &[&str]) -> bool {
    errors.iter().any(|e| categories.contains(&e.category) && e.provider == provider)
}

fn insert_category_health<I>(
    health: &mut HashMap<(&'static str, String), bool>,
    errors: &[RefreshError],
    health_category: &'static str,
    provider_names: I,
    error_categories: &[&str],
) where
    I: IntoIterator<Item = String>,
{
    for name in provider_names {
        let has_error = provider_has_error(errors, &name, error_categories);
        health.insert((health_category, name), !has_error);
    }
}

/// Fetch all provider data into the given ProviderData struct.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn refresh_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
    environment_id: Option<&EnvironmentId>,
    host_id: Option<&HostId>,
    attachable_store: &SharedAttachableStore,
    agent_state_store: &crate::agents::SharedAgentStateStore,
) -> Vec<RefreshError> {
    let mut errors = refresh_fast_providers(pd, repo_root, registry, environment_id, host_id, attachable_store, agent_state_store).await;
    errors.extend(refresh_slow_providers(pd, repo_root, registry, criteria, host_id).await);
    errors
}

fn collect_errors(errors: &mut Vec<RefreshError>, category: &'static str, provider_errors: Vec<(String, String)>) {
    for (provider, message) in provider_errors {
        errors.push(RefreshError { category, provider, message });
    }
}

#[allow(clippy::too_many_arguments)]
async fn refresh_fast_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    environment_id: Option<&EnvironmentId>,
    host_id: Option<&HostId>,
    attachable_store: &SharedAttachableStore,
    agent_state_store: &crate::agents::SharedAgentStateStore,
) -> Vec<RefreshError> {
    let mut errors = Vec::new();
    let ee_root = ExecutionEnvironmentPath::new(repo_root);

    let checkouts_fut = async {
        if let Some((desc, cm)) = registry.checkout_managers.preferred_with_desc() {
            let name = desc.display_name.clone();
            match cm.list_checkouts(&ee_root).await {
                Ok(entries) => (entries, vec![]),
                Err(e) => (vec![], vec![(name, e)]),
            }
        } else {
            (vec![], vec![])
        }
    };

    let ws_fut = async {
        if let Some((desc, ws_mgr)) = registry.presentation_managers.preferred_with_desc() {
            let name = desc.display_name.clone();
            match ws_mgr.list_workspaces().await {
                Ok(entries) => (entries, vec![]),
                Err(e) => (vec![], vec![(name, e)]),
            }
        } else {
            (vec![], vec![])
        }
    };

    let terminal_manager = registry.terminal_pools.preferred_with_desc().map(|(desc, tp)| {
        let tm =
            crate::terminal_manager::TerminalManager::new(Arc::clone(tp), attachable_store.clone(), flotilla_protocol::HostName::local());
        (desc.display_name.clone(), tm)
    });
    let tp_fut = async {
        match &terminal_manager {
            Some((name, tm)) => match tm.refresh().await {
                Ok(_) => vec![],
                Err(e) => vec![(name.clone(), e)],
            },
            None => vec![],
        }
    };

    let ((checkouts, checkout_errors), (workspaces, ws_errors), tp_errors) = tokio::join!(checkouts_fut, ws_fut, tp_fut);

    let local_host = HostName::local();
    pd.checkouts = checkouts
        .into_iter()
        .map(|(path, mut co)| {
            if co.environment_id.is_none() {
                co.environment_id = environment_id.cloned();
            }
            let publication =
                normalize_checkout_publication(QualifiedPath::from_host_name(&local_host, path.as_path()), host_id, &local_host);
            co.correlation_keys = normalize_checkout_correlation_keys(co.correlation_keys, host_id, &local_host);
            (publication, co)
        })
        .collect();
    collect_errors(&mut errors, "checkouts", checkout_errors);

    pd.workspaces = workspaces.into_iter().collect();
    for workspace in pd.workspaces.values_mut() {
        workspace.correlation_keys = normalize_checkout_correlation_keys(workspace.correlation_keys.clone(), host_id, &local_host);
    }
    collect_errors(&mut errors, "workspaces", ws_errors);

    collect_errors(&mut errors, "terminals", tp_errors);

    project_attachable_data(pd, registry, attachable_store);
    project_agent_data(pd, agent_state_store);

    errors
}

async fn refresh_slow_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
    host_id: Option<&HostId>,
) -> Vec<RefreshError> {
    let mut errors = Vec::new();
    let ee_root = ExecutionEnvironmentPath::new(repo_root);
    let cr_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_change_requests(repo_root, 20))).collect(),
    );
    let sessions_fut = collect_named_results(
        registry.cloud_agents.iter().map(|(desc, agent)| (desc.display_name.clone(), agent.list_sessions(criteria))).collect(),
    );
    let branches_fut = collect_named_results(
        registry.vcs.iter().map(|(desc, vcs)| (desc.display_name.clone(), vcs.list_remote_branches(&ee_root))).collect(),
    );
    let merged_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_merged_branch_names(repo_root, 50))).collect(),
    );
    let ((change_requests, cr_errors), (sessions, session_errors), (branches, branch_errors), (merged, merged_errors)) =
        tokio::join!(cr_fut, sessions_fut, branches_fut, merged_fut);

    let local_host = HostName::local();
    pd.change_requests = change_requests.into_iter().collect();
    for change_request in pd.change_requests.values_mut() {
        change_request.correlation_keys =
            normalize_checkout_correlation_keys(change_request.correlation_keys.clone(), host_id, &local_host);
    }
    collect_errors(&mut errors, "PRs", cr_errors);

    pd.sessions = sessions.into_iter().collect();
    for session in pd.sessions.values_mut() {
        session.correlation_keys = normalize_checkout_correlation_keys(session.correlation_keys.clone(), host_id, &local_host);
    }
    collect_errors(&mut errors, "sessions", session_errors);

    use flotilla_protocol::delta::{Branch, BranchStatus};
    pd.branches.clear();
    collect_errors(&mut errors, "branches", branch_errors);
    collect_errors(&mut errors, "merged", merged_errors);
    for name in branches {
        pd.branches.insert(name, Branch { status: BranchStatus::Remote });
    }
    for name in merged {
        pd.branches.insert(name, Branch { status: BranchStatus::Merged });
    }

    errors
}

fn project_attachable_data(pd: &mut ProviderData, registry: &ProviderRegistry, attachable_store: &SharedAttachableStore) {
    let workspace_provider = registry.presentation_managers.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let Ok(mut store) = attachable_store.lock() else {
        tracing::warn!("attachable store lock poisoned while projecting provider data");
        return;
    };
    pd.managed_terminals.clear();

    if let Some(provider_name) = workspace_provider.as_deref() {
        for (ws_ref, workspace) in &mut pd.workspaces {
            let Some(set_id) = store.lookup_binding("workspace_manager", provider_name, BindingObjectKind::AttachableSet, ws_ref.as_str())
            else {
                continue;
            };
            let set_id = flotilla_protocol::AttachableSetId::new(set_id.to_string());
            workspace.attachable_set_id = Some(set_id.clone());
        }
    }

    // Prune stale workspace bindings within the provider's declared scope.
    // Skip when workspace list is empty — it may indicate a list failure,
    // and pruning would incorrectly delete all bindings.
    if !pd.workspaces.is_empty() {
        if let Some((desc, ws_mgr)) = registry.presentation_managers.preferred_with_desc() {
            let provider_name = &desc.implementation;
            let scope_prefix = ws_mgr.binding_scope_prefix();
            let live_ws_refs: std::collections::HashSet<&str> = pd.workspaces.keys().map(|s| s.as_str()).collect();

            let stale_refs: Vec<String> = store
                .registry()
                .bindings
                .iter()
                .filter(|b| {
                    b.provider_category == "workspace_manager"
                        && b.provider_name == *provider_name
                        && b.object_kind == BindingObjectKind::AttachableSet
                        && b.external_ref.starts_with(&scope_prefix)
                        && !live_ws_refs.contains(b.external_ref.as_str())
                })
                .map(|b| b.external_ref.clone())
                .collect();

            for stale_ref in &stale_refs {
                tracing::info!(external_ref = %stale_ref, provider = %provider_name, "pruning stale workspace binding");
                store.remove_binding_object("workspace_manager", provider_name, BindingObjectKind::AttachableSet, stale_ref);
            }
            if !stale_refs.is_empty() {
                if let Err(err) = store.save() {
                    tracing::warn!(err = %err, "failed to save after pruning stale workspace bindings");
                }
            }
        }
    }

    // Set selection: project sets whose checkout matches a repo checkout
    let checkout_paths: std::collections::HashSet<PathBuf> = pd.checkouts.keys().map(|path| path.path.clone()).collect();
    pd.attachable_sets = store
        .registry()
        .sets
        .iter()
        .filter(|(_, set)| set.checkout.as_ref().is_some_and(|co| checkout_paths.contains(&co.path)))
        .map(|(id, set)| (id.clone(), set.clone()))
        .collect();

    // Build managed_terminals from the attachable store for projected sets
    for (attachable_id, attachable) in &store.registry().attachables {
        if !pd.attachable_sets.contains_key(&attachable.set_id) {
            continue;
        }
        match &attachable.content {
            crate::attachable::AttachableContent::Terminal(t) => {
                pd.managed_terminals.insert(attachable_id.clone(), flotilla_protocol::ManagedTerminal {
                    set_id: attachable.set_id.clone(),
                    role: t.purpose.role.clone(),
                    command: t.command.clone(),
                    working_directory: t.working_directory.clone().into_path_buf(),
                    status: t.status.clone(),
                });
            }
        }
    }
}

fn project_agent_data(pd: &mut ProviderData, agent_state_store: &crate::agents::SharedAgentStateStore) {
    let Ok(store) = agent_state_store.lock() else {
        tracing::warn!("agent state store lock poisoned while projecting agent data");
        return;
    };
    pd.agents.clear();
    for (attachable_id, entry) in store.list_agents() {
        // Only include agents whose terminal's attachable set belongs to this repo.
        // Find the set that contains this attachable_id.
        let matching_set = pd.attachable_sets.iter().find(|(_, set)| set.members.contains(&attachable_id));
        let Some((set_id, _)) = matching_set else {
            continue;
        };
        let correlation_keys = vec![flotilla_protocol::CorrelationKey::AttachableSet(set_id.clone())];

        pd.agents.insert(attachable_id.to_string(), flotilla_protocol::Agent {
            harness: entry.harness,
            status: entry.status,
            model: entry.model,
            context: flotilla_protocol::AgentContext::Local { attachable_id },
            correlation_keys,
            provider_name: "cli-agent".to_string(),
            provider_display_name: "CLI Agent".to_string(),
            item_noun: "agent".to_string(),
        });
    }
}

fn compute_provider_health(registry: &ProviderRegistry, errors: &[RefreshError]) -> HashMap<(&'static str, String), bool> {
    use crate::providers::discovery::ProviderCategory;

    let mut health = HashMap::new();

    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::CloudAgent.slug(),
        registry.cloud_agents.display_names().map(|s| s.to_string()),
        &["sessions"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::ChangeRequest.slug(),
        registry.change_requests.display_names().map(|s| s.to_string()),
        &["PRs", "merged"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::CheckoutManager.slug(),
        registry.checkout_managers.display_names().map(|s| s.to_string()),
        &["checkouts"],
    );
    insert_category_health(&mut health, errors, ProviderCategory::Vcs.slug(), registry.vcs.display_names().map(|s| s.to_string()), &[
        "branches",
    ]);
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::WorkspaceManager.slug(),
        registry.presentation_managers.display_names().map(|s| s.to_string()),
        &["workspaces"],
    );
    insert_category_health(
        &mut health,
        errors,
        ProviderCategory::TerminalPool.slug(),
        registry.terminal_pools.display_names().map(|s| s.to_string()),
        &["terminals"],
    );

    health
}

#[cfg(test)]
mod tests;
