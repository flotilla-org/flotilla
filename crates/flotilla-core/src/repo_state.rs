//! Per-repo state managed by the in-process daemon.
//!
//! `RepoState` tracks a single logical repository (identified by
//! `RepoIdentity`), which may have multiple filesystem roots (e.g. a
//! local checkout and a remote-only synthetic root). It owns the issue
//! cache, delta log, and broadcast tracking for that repo.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use flotilla_protocol::{DeltaEntry, HostName, Issue, ProviderData, ProviderError, RepoSnapshot};
use tokio::sync::Mutex;

use crate::{
    delta,
    issue_cache::IssueCache,
    model::{provider_names_from_registry, RepoModel},
    providers::discovery::{EnvironmentBag, UnmetRequirement},
    refresh::RefreshSnapshot,
};

/// Maximum number of delta entries retained per repo.
pub(crate) const DELTA_LOG_CAPACITY: usize = 16;

/// Context needed to build a proto `RepoSnapshot` from local provider data.
///
/// Constructed via [`RepoState::snapshot_context`] for production use.
/// Tests may construct directly with ad-hoc values.
pub(crate) struct SnapshotBuildContext<'a> {
    pub(crate) repo_identity: flotilla_protocol::RepoIdentity,
    pub(crate) path: &'a Path,
    /// Local-only provider data — must NOT contain merged peer data.
    /// Errors and health from the last snapshot are passed separately.
    pub(crate) local_providers: &'a ProviderData,
    pub(crate) errors: &'a [crate::data::RefreshError],
    pub(crate) provider_health: &'a HashMap<(&'static str, String), bool>,
    pub(crate) cache: &'a IssueCache,
    pub(crate) search_results: &'a Option<Vec<(String, Issue)>>,
    pub(crate) host_name: &'a HostName,
}

pub(crate) struct RepoRootState {
    pub(crate) path: PathBuf,
    pub(crate) model: RepoModel,
    pub(crate) slug: Option<String>,
    pub(crate) repo_bag: EnvironmentBag,
    pub(crate) unmet: Vec<(String, UnmetRequirement)>,
    pub(crate) is_local: bool,
}

pub(crate) struct RepoState {
    identity: flotilla_protocol::RepoIdentity,
    pub(crate) roots: Vec<RepoRootState>,
    seq: u64,
    pub(crate) last_local_providers: ProviderData,
    pub(crate) last_snapshot: Arc<RefreshSnapshot>,
    pub(crate) issue_cache: IssueCache,
    pub(crate) search_results: Option<Vec<(String, Issue)>>,
    /// Serializes issue fetch operations for this repo to prevent concurrent page skips.
    issue_fetch_mutex: Arc<Mutex<()>>,
    /// Last broadcast provider data (with injected issues), used for delta computation.
    last_broadcast_providers: ProviderData,
    /// Last broadcast provider health, used for delta computation.
    last_broadcast_health: HashMap<String, HashMap<String, bool>>,
    /// Last broadcast errors, used for delta computation.
    last_broadcast_errors: Vec<ProviderError>,
    /// Bounded delta log for replay on client reconnect.
    delta_log: VecDeque<DeltaEntry>,
    /// Incremented only when local provider data changes (not peer data merges).
    /// Used by the outbound peer task to avoid re-sending unchanged local data.
    local_data_version: u64,
    /// The last broadcast snapshot (merged local + peer data, fully correlated).
    /// Populated by `poll_snapshots` and `broadcast_snapshot_inner` after each
    /// broadcast. Query methods read from this instead of recomputing.
    last_merged_snapshot: Option<Arc<RepoSnapshot>>,
}

impl RepoState {
    pub(crate) fn new(identity: flotilla_protocol::RepoIdentity, root: RepoRootState) -> Self {
        Self {
            identity,
            roots: vec![root],
            seq: 0,
            last_local_providers: ProviderData::default(),
            last_snapshot: Arc::new(RefreshSnapshot::default()),
            issue_cache: IssueCache::new(),
            search_results: None,
            issue_fetch_mutex: Arc::new(Mutex::new(())),
            last_broadcast_providers: ProviderData::default(),
            last_broadcast_health: HashMap::new(),
            last_broadcast_errors: Vec::new(),
            delta_log: VecDeque::new(),
            local_data_version: 0,
            last_merged_snapshot: None,
        }
    }

    pub(crate) fn preferred_root(&self) -> &RepoRootState {
        self.roots.first().expect("repo state should always have at least one root")
    }

    pub(crate) fn preferred_root_mut(&mut self) -> &mut RepoRootState {
        self.roots.first_mut().expect("repo state should always have at least one root")
    }

    pub(crate) fn preferred_path(&self) -> &Path {
        &self.preferred_root().path
    }

    pub(crate) fn registry(&self) -> Arc<crate::providers::registry::ProviderRegistry> {
        Arc::clone(&self.preferred_root().model.registry)
    }

    pub(crate) fn providers(&self) -> Arc<ProviderData> {
        Arc::clone(&self.preferred_root().model.data.providers)
    }

    pub(crate) fn refresh_trigger(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.preferred_root().model.refresh_handle.refresh_trigger)
    }

    pub(crate) fn slug(&self) -> Option<&str> {
        self.preferred_root().slug.as_deref()
    }

    pub(crate) fn repo_bag(&self) -> &EnvironmentBag {
        &self.preferred_root().repo_bag
    }

    pub(crate) fn unmet(&self) -> &[(String, UnmetRequirement)] {
        &self.preferred_root().unmet
    }

    pub(crate) fn labels(&self) -> &crate::model::RepoLabels {
        &self.preferred_root().model.labels
    }

    pub(crate) fn provider_names(&self) -> HashMap<String, Vec<String>> {
        provider_names_from_registry(&self.preferred_root().model.registry)
    }

    pub(crate) fn provider_health(&self) -> &HashMap<(&'static str, String), bool> {
        &self.preferred_root().model.data.provider_health
    }

    pub(crate) fn loading(&self) -> bool {
        self.preferred_root().model.data.loading
    }

    pub(crate) fn contains_path(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| root.path == path)
    }

    pub(crate) fn add_root(&mut self, root: RepoRootState) -> bool {
        if self.contains_path(&root.path) {
            return false;
        }
        // Keep local roots ahead of synthetic remote-only roots so
        // preferred_root() remains the executable local instance whenever
        // this identity is tracked on disk.
        let preferred_changed = !self.preferred_root().is_local && root.is_local;
        if preferred_changed {
            self.roots.insert(0, root);
        } else {
            self.roots.push(root);
        }
        preferred_changed
    }

    pub(crate) fn remove_root(&mut self, path: &Path) -> bool {
        let Some(idx) = self.roots.iter().position(|root| root.path == path) else {
            return false;
        };
        self.roots.remove(idx);
        true
    }

    pub(crate) fn local_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().filter(|root| root.is_local).map(|root| root.path.clone()).collect()
    }

    pub(crate) fn identity(&self) -> &flotilla_protocol::RepoIdentity {
        &self.identity
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    pub(crate) fn local_data_version(&self) -> u64 {
        self.local_data_version
    }

    pub(crate) fn mark_local_change(&mut self) {
        self.local_data_version += 1;
    }

    pub(crate) fn issue_fetch_mutex(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.issue_fetch_mutex)
    }

    pub(crate) fn cached_snapshot(&self) -> Option<&Arc<RepoSnapshot>> {
        self.last_merged_snapshot.as_ref()
    }

    pub(crate) fn set_cached_snapshot(&mut self, snapshot: RepoSnapshot) {
        self.last_merged_snapshot = Some(Arc::new(snapshot));
    }

    /// Return delta entries that replay state from `client_seq` to current.
    ///
    /// Returns `None` if `client_seq` is not in the delta log (caller should
    /// fall back to a full snapshot). Returns `Some(empty vec)` if the
    /// client is already up to date.
    pub(crate) fn deltas_since(&self, client_seq: u64) -> Option<Vec<&DeltaEntry>> {
        if client_seq == self.seq {
            return Some(vec![]);
        }
        let start = self.delta_log.iter().position(|entry| entry.prev_seq == client_seq)?;
        Some(self.delta_log.iter().skip(start).collect())
    }

    /// Build a [`SnapshotBuildContext`] from the current state.
    pub(crate) fn snapshot_context<'a>(&'a self, host_name: &'a HostName) -> SnapshotBuildContext<'a> {
        SnapshotBuildContext {
            repo_identity: self.identity.clone(),
            path: self.preferred_path(),
            local_providers: &self.last_local_providers,
            errors: &self.last_snapshot.errors,
            provider_health: &self.last_snapshot.provider_health,
            cache: &self.issue_cache,
            search_results: &self.search_results,
            host_name,
        }
    }

    /// Compute a delta from the last broadcast state to the new state,
    /// append to the delta log, update tracking fields, and advance `seq`.
    pub(crate) fn record_delta(
        &mut self,
        new_providers: &ProviderData,
        new_health: &HashMap<String, HashMap<String, bool>>,
        new_errors: &[ProviderError],
        work_items: Vec<flotilla_protocol::snapshot::WorkItem>,
    ) -> DeltaEntry {
        let mut changes = delta::diff_provider_data(&self.last_broadcast_providers, new_providers);

        // Diff provider health (nested: category → provider → bool)
        for (category, providers) in new_health {
            let old_providers = self.last_broadcast_health.get(category);
            for (provider, &val) in providers {
                let old_val = old_providers.and_then(|p| p.get(provider));
                match old_val {
                    Some(&prev) if prev == val => {}
                    Some(_) => changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Updated(val),
                    }),
                    None => changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Added(val),
                    }),
                }
            }
        }
        // Check for removed entries
        for (category, old_providers) in &self.last_broadcast_health {
            let new_providers = new_health.get(category);
            for provider in old_providers.keys() {
                if new_providers.and_then(|p| p.get(provider)).is_none() {
                    changes.push(flotilla_protocol::Change::ProviderHealth {
                        category: category.clone(),
                        provider: provider.clone(),
                        op: flotilla_protocol::EntryOp::Removed,
                    });
                }
            }
        }

        // Diff errors
        if let Some(error_change) = delta::diff_errors(&self.last_broadcast_errors, new_errors) {
            changes.push(error_change);
        }

        let prev_seq = self.seq;
        let entry = DeltaEntry { seq: self.seq + 1, prev_seq, changes, work_items };

        // Append to bounded log
        self.delta_log.push_back(entry.clone());
        if self.delta_log.len() > DELTA_LOG_CAPACITY {
            self.delta_log.pop_front();
        }

        // Update tracking state
        self.seq += 1;
        self.last_broadcast_providers = new_providers.clone();
        self.last_broadcast_health = new_health.clone();
        self.last_broadcast_errors = new_errors.to_vec();

        entry
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use flotilla_protocol::{Checkout, HostName, ProviderData, ProviderError};

    use super::*;

    /// Helper to create a minimal RepoState for delta testing.
    fn make_repo_state() -> RepoState {
        RepoState::new(flotilla_protocol::RepoIdentity { authority: "local".into(), path: "/virtual".into() }, RepoRootState {
            path: PathBuf::from("/virtual"),
            model: crate::model::RepoModel::new_virtual(),
            slug: None,
            repo_bag: crate::providers::discovery::EnvironmentBag::new(),
            unmet: Vec::new(),
            is_local: false,
        })
    }

    #[tokio::test]
    async fn record_delta_detects_added_checkout() {
        let mut state = make_repo_state();

        let mut new_providers = ProviderData::default();
        new_providers.checkouts.insert(flotilla_protocol::HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")), Checkout {
            branch: "feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            environment_id: None,
        });

        let entry = state.record_delta(&new_providers, &HashMap::new(), &[], vec![]);

        assert_eq!(entry.prev_seq, 0);
        assert_eq!(entry.seq, 1);
        assert!(
            entry.changes.iter().any(|c| matches!(c, flotilla_protocol::Change::Checkout { op: flotilla_protocol::EntryOp::Added(_), .. })),
            "should have an Added checkout change"
        );
    }

    #[tokio::test]
    async fn record_delta_detects_provider_health_update() {
        let mut state = make_repo_state();
        state.last_broadcast_health = HashMap::from([("vcs".into(), HashMap::from([("git".into(), true)]))]);

        // Change the health value from true to false
        let new_health = HashMap::from([("vcs".into(), HashMap::from([("git".into(), false)]))]);
        let entry = state.record_delta(&ProviderData::default(), &new_health, &[], vec![]);

        assert!(
            entry.changes.iter().any(|c| matches!(
                c,
                flotilla_protocol::Change::ProviderHealth {
                    category,
                    provider,
                    op: flotilla_protocol::EntryOp::Updated(false),
                } if category == "vcs" && provider == "git"
            )),
            "should have an Updated health change: {:?}",
            entry.changes
        );
    }

    #[tokio::test]
    async fn record_delta_detects_provider_health_added() {
        let mut state = make_repo_state();

        // New health entry with no prior state
        let new_health = HashMap::from([("change_request".into(), HashMap::from([("github".into(), true)]))]);
        let entry = state.record_delta(&ProviderData::default(), &new_health, &[], vec![]);

        assert!(
            entry.changes.iter().any(|c| matches!(
                c,
                flotilla_protocol::Change::ProviderHealth {
                    category,
                    provider,
                    op: flotilla_protocol::EntryOp::Added(true),
                } if category == "change_request" && provider == "github"
            )),
            "should have an Added health change: {:?}",
            entry.changes
        );
    }

    #[tokio::test]
    async fn record_delta_detects_provider_health_removed() {
        let mut state = make_repo_state();
        state.last_broadcast_health = HashMap::from([("vcs".into(), HashMap::from([("git".into(), true)]))]);

        // Empty health means the old entry was removed
        let entry = state.record_delta(&ProviderData::default(), &HashMap::new(), &[], vec![]);

        assert!(
            entry.changes.iter().any(|c| matches!(
                c,
                flotilla_protocol::Change::ProviderHealth {
                    category,
                    provider,
                    op: flotilla_protocol::EntryOp::Removed,
                } if category == "vcs" && provider == "git"
            )),
            "should have a Removed health change: {:?}",
            entry.changes
        );
    }

    #[tokio::test]
    async fn record_delta_detects_error_change() {
        let mut state = make_repo_state();

        let new_errors = vec![ProviderError { category: "vcs".into(), provider: "git".into(), message: "failed".into() }];
        let entry = state.record_delta(&ProviderData::default(), &HashMap::new(), &new_errors, vec![]);

        assert!(
            entry.changes.iter().any(|c| matches!(c, flotilla_protocol::Change::ErrorsChanged(_))),
            "should have an ErrorsChanged entry: {:?}",
            entry.changes
        );
    }

    #[tokio::test]
    async fn record_delta_log_bounded_at_capacity() {
        let mut state = make_repo_state();

        // Record more deltas than DELTA_LOG_CAPACITY
        for i in 0..(DELTA_LOG_CAPACITY + 5) {
            let mut providers = ProviderData::default();
            // Change something each iteration so a delta is produced
            providers.checkouts.insert(
                flotilla_protocol::HostPath::new(HostName::new("host"), PathBuf::from(format!("/tmp/co-{i}"))),
                Checkout {
                    branch: format!("feat-{i}"),
                    is_main: false,
                    trunk_ahead_behind: None,
                    remote_ahead_behind: None,
                    working_tree: None,
                    last_commit: None,
                    correlation_keys: vec![],
                    association_keys: vec![],
                    environment_id: None,
                },
            );
            state.record_delta(&providers, &HashMap::new(), &[], vec![]);
            // Update seq to match what record_delta expects
            state.seq = state.delta_log.back().expect("delta log non-empty").seq;
        }

        assert_eq!(state.delta_log.len(), DELTA_LOG_CAPACITY, "delta log should be bounded at {DELTA_LOG_CAPACITY}");
    }

    #[tokio::test]
    async fn record_delta_seq_increments_correctly() {
        let mut state = make_repo_state();

        let entry1 = state.record_delta(&ProviderData::default(), &HashMap::new(), &[], vec![]);
        assert_eq!(entry1.prev_seq, 0, "first delta prev_seq should be 0");
        assert_eq!(entry1.seq, 1, "first delta seq should be 1");

        // Advance state.seq to match
        state.seq = entry1.seq;

        let entry2 = state.record_delta(&ProviderData::default(), &HashMap::new(), &[], vec![]);
        assert_eq!(entry2.prev_seq, 1, "second delta prev_seq should be 1");
        assert_eq!(entry2.seq, 2, "second delta seq should be 2");
    }

    #[tokio::test]
    async fn record_delta_updates_tracking_state() {
        let mut state = make_repo_state();

        let new_health = HashMap::from([("vcs".into(), HashMap::from([("git".into(), true)]))]);
        let new_errors = vec![ProviderError { category: "vcs".into(), provider: "git".into(), message: "oops".into() }];
        let mut new_providers = ProviderData::default();
        new_providers.checkouts.insert(flotilla_protocol::HostPath::new(HostName::new("host"), PathBuf::from("/tmp/co")), Checkout {
            branch: "feat".into(),
            is_main: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
            environment_id: None,
        });

        state.record_delta(&new_providers, &new_health, &new_errors, vec![]);

        // After record_delta, the tracking state should be updated to the new values
        assert_eq!(state.last_broadcast_providers, new_providers);
        assert_eq!(state.last_broadcast_health, new_health);
        assert_eq!(state.last_broadcast_errors, new_errors);
        assert_eq!(state.delta_log.len(), 1);
    }
}
