use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::data::{self, ProviderError, WorkItem};
use crate::provider_data::ProviderData;
use crate::providers::correlation::CorrelatedGroup;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::RepoCriteria;

/// Result of a single background refresh cycle.
#[derive(Debug, Clone)]
pub struct RefreshSnapshot {
    pub providers: Arc<ProviderData>,
    pub work_items: Vec<WorkItem>,
    pub correlation_groups: Vec<CorrelatedGroup>,
    pub errors: Vec<ProviderError>,
    pub provider_health: HashMap<&'static str, bool>,
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
    pub skip_issues: Arc<AtomicBool>,
    _task_handle: JoinHandle<()>,
}

impl RepoRefreshHandle {
    pub fn spawn(
        repo_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        criteria: RepoCriteria,
        interval: Duration,
    ) -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(RefreshSnapshot::default()));
        let refresh_trigger = Arc::new(Notify::new());
        let trigger = refresh_trigger.clone();
        let skip_issues = Arc::new(AtomicBool::new(false));
        let skip_issues_clone = skip_issues.clone();

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
                let errors = refresh_providers(
                    &mut provider_data,
                    &repo_root,
                    &registry,
                    &criteria,
                    skip_issues_clone.load(Ordering::Relaxed),
                )
                .await;
                let provider_health = compute_provider_health(&registry, &errors);

                // Correlate
                let providers = Arc::new(provider_data);
                let (work_items, correlation_groups) = data::correlate(&providers);

                let snapshot = Arc::new(RefreshSnapshot {
                    providers,
                    work_items,
                    correlation_groups,
                    errors,
                    provider_health,
                });

                // Publish — receivers will see has_changed().
                // Break if receiver is dropped (handle dropped without Drop running).
                if snapshot_tx.send(snapshot).is_err() {
                    break;
                }
            }
        });

        Self {
            refresh_trigger,
            snapshot_rx,
            skip_issues,
            _task_handle: task_handle,
        }
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

/// Fetch all provider data into the given ProviderData struct.
async fn refresh_providers(
    pd: &mut ProviderData,
    repo_root: &Path,
    registry: &ProviderRegistry,
    criteria: &RepoCriteria,
    skip_issues: bool,
) -> Vec<ProviderError> {
    let mut errors = Vec::new();

    let checkouts_fut = async {
        if let Some(cm) = registry.checkout_managers.values().next() {
            cm.list_checkouts(repo_root).await
        } else {
            Ok(vec![])
        }
    };

    let cr_fut = async {
        if let Some(cr) = registry.code_review.values().next() {
            cr.list_change_requests(repo_root, 20).await
        } else {
            Ok(vec![])
        }
    };

    let issues_fut = async {
        if skip_issues {
            return Ok(vec![]);
        }
        if let Some(it) = registry.issue_trackers.values().next() {
            it.list_issues(repo_root, 20).await
        } else {
            Ok(vec![])
        }
    };

    let sessions_fut = async {
        if let Some(ca) = registry.coding_agents.values().next() {
            ca.list_sessions(criteria).await
        } else {
            Ok(vec![])
        }
    };

    let branches_fut = async {
        if let Some(vcs) = registry.vcs.values().next() {
            vcs.list_remote_branches(repo_root).await
        } else {
            Ok(vec![])
        }
    };

    let merged_fut = async {
        if let Some(cr) = registry.code_review.values().next() {
            cr.list_merged_branch_names(repo_root, 50).await
        } else {
            Ok(vec![])
        }
    };

    let ws_fut = async {
        if let Some((_, ws_mgr)) = &registry.workspace_manager {
            ws_mgr.list_workspaces().await
        } else {
            Ok(vec![])
        }
    };

    let (checkouts, crs, issues, sessions, branches, merged, workspaces) = tokio::join!(
        checkouts_fut,
        cr_fut,
        issues_fut,
        sessions_fut,
        branches_fut,
        merged_fut,
        ws_fut
    );

    pd.checkouts = checkouts
        .unwrap_or_else(|e| {
            errors.push(ProviderError {
                category: "checkouts",
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|co| (co.path.clone(), co))
        .collect();
    pd.change_requests = crs
        .unwrap_or_else(|e| {
            errors.push(ProviderError {
                category: "PRs",
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|cr| (cr.id.clone(), cr))
        .collect();
    pd.issues = issues
        .unwrap_or_else(|e| {
            errors.push(ProviderError {
                category: "issues",
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    pd.workspaces = workspaces
        .unwrap_or_else(|e| {
            errors.push(ProviderError {
                category: "workspaces",
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|ws| (ws.ws_ref.clone(), ws))
        .collect();
    pd.sessions = sessions
        .unwrap_or_else(|e| {
            errors.push(ProviderError {
                category: "sessions",
                message: e,
            });
            Vec::new()
        })
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();
    pd.remote_branches = branches.unwrap_or_else(|e| {
        errors.push(ProviderError {
            category: "branches",
            message: e,
        });
        Vec::new()
    });
    pd.merged_branches = merged.unwrap_or_else(|e| {
        errors.push(ProviderError {
            category: "merged",
            message: e,
        });
        Vec::new()
    });

    errors
}

fn compute_provider_health(
    registry: &ProviderRegistry,
    errors: &[ProviderError],
) -> HashMap<&'static str, bool> {
    let mut health = HashMap::new();
    if registry.coding_agents.values().next().is_some() {
        health.insert(
            "coding_agent",
            !errors.iter().any(|e| e.category == "sessions"),
        );
    }
    if registry.code_review.values().next().is_some() {
        health.insert(
            "code_review",
            !errors
                .iter()
                .any(|e| e.category == "PRs" || e.category == "merged"),
        );
    }
    if registry.issue_trackers.values().next().is_some() {
        health.insert(
            "issue_tracker",
            !errors.iter().any(|e| e.category == "issues"),
        );
    }
    health
}
