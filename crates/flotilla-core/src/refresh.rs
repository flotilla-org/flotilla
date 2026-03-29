use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
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

impl RepoRefreshHandle {
    pub fn spawn(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        attachable_store: SharedAttachableStore,
        agent_state_store: crate::agents::SharedAgentStateStore,
        interval: Duration,
    ) -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());
        let trigger = refresh_trigger.clone();

        let task_handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = timer.tick() => {}
                    _ = trigger.notified() => {}
                }

                // Fetch all provider data
                let mut provider_data = ProviderData::default();
                let errors =
                    refresh_providers(&mut provider_data, &repo_root, &registry, &criteria, &attachable_store, &agent_state_store).await;
                let provider_health = compute_provider_health(&registry, &errors);

                // Correlate
                let providers = Arc::new(provider_data);
                let (work_items, correlation_groups) = data::correlate(&providers);

                let snapshot = Arc::new(RefreshSnapshot { providers, work_items, correlation_groups, errors, provider_health });

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
async fn refresh_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
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

    let cr_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_change_requests(repo_root, 20))).collect(),
    );

    let sessions_fut = collect_named_results(
        registry.cloud_agents.iter().map(|(desc, ca)| (desc.display_name.clone(), ca.list_sessions(criteria))).collect(),
    );

    let branches_fut = collect_named_results(
        registry.vcs.iter().map(|(desc, vcs)| (desc.display_name.clone(), vcs.list_remote_branches(&ee_root))).collect(),
    );

    let merged_fut = collect_named_results(
        registry.change_requests.iter().map(|(desc, cr)| (desc.display_name.clone(), cr.list_merged_branch_names(repo_root, 50))).collect(),
    );

    let ws_fut = async {
        if let Some((desc, ws_mgr)) = registry.workspace_managers.preferred_with_desc() {
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

    let (
        (checkouts, checkout_errors),
        (crs, cr_errors),
        (sessions, session_errors),
        (branches, branch_errors),
        (merged, merged_errors),
        (workspaces, ws_errors),
        tp_errors,
    ) = tokio::join!(checkouts_fut, cr_fut, sessions_fut, branches_fut, merged_fut, ws_fut, tp_fut);

    fn collect_errors(errors: &mut Vec<RefreshError>, category: &'static str, provider_errors: Vec<(String, String)>) {
        for (provider, message) in provider_errors {
            errors.push(RefreshError { category, provider, message });
        }
    }

    let local_host = flotilla_protocol::HostName::local();
    pd.checkouts = checkouts
        .into_iter()
        .map(|(path, co)| (flotilla_protocol::QualifiedPath::from_host_path(&local_host, path.as_path()), co))
        .collect();
    collect_errors(&mut errors, "checkouts", checkout_errors);

    pd.change_requests = crs.into_iter().collect();
    collect_errors(&mut errors, "PRs", cr_errors);

    pd.sessions = sessions.into_iter().collect();
    collect_errors(&mut errors, "sessions", session_errors);

    pd.workspaces = workspaces.into_iter().collect();
    collect_errors(&mut errors, "workspaces", ws_errors);

    collect_errors(&mut errors, "terminals", tp_errors);

    project_attachable_data(pd, registry, attachable_store);
    project_agent_data(pd, agent_state_store);
    {
        use flotilla_protocol::delta::{Branch, BranchStatus};
        let remote = branches;
        collect_errors(&mut errors, "branches", branch_errors);
        let merged_names = merged;
        collect_errors(&mut errors, "merged", merged_errors);
        for name in remote {
            pd.branches.insert(name, Branch { status: BranchStatus::Remote });
        }
        for name in merged_names {
            pd.branches.insert(name, Branch { status: BranchStatus::Merged });
        }
    }

    errors
}

fn project_attachable_data(pd: &mut ProviderData, registry: &ProviderRegistry, attachable_store: &SharedAttachableStore) {
    let workspace_provider = registry.workspace_managers.preferred_with_desc().map(|(desc, _)| desc.implementation.clone());
    let Ok(mut store) = attachable_store.lock() else {
        tracing::warn!("attachable store lock poisoned while projecting provider data");
        return;
    };

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
        if let Some((desc, ws_mgr)) = registry.workspace_managers.preferred_with_desc() {
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
    let checkout_paths: std::collections::HashSet<&flotilla_protocol::QualifiedPath> = pd.checkouts.keys().collect();
    pd.attachable_sets = store
        .registry()
        .sets
        .iter()
        .filter(|(_, set)| set.checkout.as_ref().is_some_and(|co| checkout_paths.contains(co)))
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
        registry.workspace_managers.display_names().map(|s| s.to_string()),
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
