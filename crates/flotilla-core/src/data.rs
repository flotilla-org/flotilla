use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::Path,
    sync::Arc,
};

// Re-export protocol types that are used throughout the crate and by consumers.
pub use flotilla_protocol::{CheckoutRef, CheckoutStatus, WorkItemIdentity, WorkItemKind};

use crate::{
    provider_data::ProviderData,
    providers::{
        correlation::{self, CorrelatedGroup, CorrelatedItem, ItemKind as CorItemKind, ProviderItemKey},
        run,
        types::{AssociationKey, CorrelationKey},
    },
};

#[derive(Debug, Clone)]
pub struct RefreshError {
    pub category: &'static str,
    pub provider: String,
    pub message: String,
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}: {}", self.category, self.message)
        } else {
            write!(f, "{}/{}: {}", self.category, self.provider, self.message)
        }
    }
}

#[derive(Debug, Clone)]
pub enum CorrelatedAnchor {
    Checkout(CheckoutRef),
    AttachableSet(flotilla_protocol::AttachableSetId),
    ChangeRequest(String),
    Session(String),
    Agent(String),
}

#[derive(Debug, Clone)]
pub struct CorrelatedWorkItem {
    pub anchor: CorrelatedAnchor,
    pub checkout_ref: Option<CheckoutRef>,
    pub attachable_set_id: Option<flotilla_protocol::AttachableSetId>,
    pub branch: Option<String>,
    pub description: String,
    pub linked_change_request: Option<String>,
    pub linked_session: Option<String>,
    pub linked_issues: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub correlation_group_idx: usize,
    pub host: Option<flotilla_protocol::HostName>,
    pub source: Option<String>,
    pub terminal_ids: Vec<flotilla_protocol::AttachableId>,
    pub agent_keys: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum StandaloneResult {
    Issue { key: String, description: String, source: String },
    RemoteBranch { branch: String },
}

#[derive(Debug, Clone)]
// Correlated work items are the common path; boxing the standalone arm would add
// indirection where we pay the cost most often.
#[allow(clippy::large_enum_variant)]
pub enum CorrelationResult {
    Correlated(CorrelatedWorkItem),
    Standalone(StandaloneResult),
}

impl CorrelationResult {
    pub fn kind(&self) -> WorkItemKind {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(_) => WorkItemKind::Checkout,
                CorrelatedAnchor::AttachableSet(_) => WorkItemKind::AttachableSet,
                CorrelatedAnchor::ChangeRequest(_) => WorkItemKind::ChangeRequest,
                CorrelatedAnchor::Session(_) => WorkItemKind::Session,
                CorrelatedAnchor::Agent(_) => WorkItemKind::Agent,
            },
            CorrelationResult::Standalone(s) => match s {
                StandaloneResult::Issue { .. } => WorkItemKind::Issue,
                StandaloneResult::RemoteBranch { .. } => WorkItemKind::RemoteBranch,
            },
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => c.branch.as_deref(),
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch }) => Some(branch.as_str()),
            CorrelationResult::Standalone(StandaloneResult::Issue { .. }) => None,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            CorrelationResult::Correlated(c) => &c.description,
            CorrelationResult::Standalone(StandaloneResult::Issue { description, .. }) => description,
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch }) => branch,
        }
    }

    pub fn checkout(&self) -> Option<&CheckoutRef> {
        match self {
            CorrelationResult::Correlated(c) => c.checkout_ref.as_ref(),
            _ => None,
        }
    }

    pub fn checkout_key(&self) -> Option<&flotilla_protocol::QualifiedPath> {
        self.checkout().map(|co| &co.key)
    }

    pub fn is_main_checkout(&self) -> bool {
        self.checkout().is_some_and(|co| co.is_main_checkout)
    }

    pub fn change_request_key(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::ChangeRequest(key) => Some(key.as_str()),
                _ => c.linked_change_request.as_deref(),
            },
            _ => None,
        }
    }

    pub fn session_key(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Session(key) => Some(key.as_str()),
                _ => c.linked_session.as_deref(),
            },
            _ => None,
        }
    }

    pub fn attachable_set_id(&self) -> Option<&flotilla_protocol::AttachableSetId> {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::AttachableSet(id) => Some(id),
                _ => c.attachable_set_id.as_ref(),
            },
            _ => None,
        }
    }

    pub fn issue_keys(&self) -> &[String] {
        match self {
            CorrelationResult::Correlated(c) => &c.linked_issues,
            CorrelationResult::Standalone(StandaloneResult::Issue { key, .. }) => std::slice::from_ref(key),
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { .. }) => &[],
        }
    }

    pub fn workspace_refs(&self) -> &[String] {
        match self {
            CorrelationResult::Correlated(c) => &c.workspace_refs,
            _ => &[],
        }
    }

    pub fn terminal_ids(&self) -> &[flotilla_protocol::AttachableId] {
        match self {
            CorrelationResult::Correlated(c) => &c.terminal_ids,
            _ => &[],
        }
    }

    pub fn agent_keys(&self) -> &[String] {
        match self {
            CorrelationResult::Correlated(c) => &c.agent_keys,
            _ => &[],
        }
    }

    pub fn correlation_group_idx(&self) -> Option<usize> {
        match self {
            CorrelationResult::Correlated(c) => Some(c.correlation_group_idx),
            _ => None,
        }
    }

    pub fn source(&self) -> Option<&str> {
        match self {
            CorrelationResult::Correlated(c) => c.source.as_deref(),
            CorrelationResult::Standalone(StandaloneResult::Issue { source, .. }) => {
                if source.is_empty() {
                    None
                } else {
                    Some(source.as_str())
                }
            }
            CorrelationResult::Standalone(StandaloneResult::RemoteBranch { .. }) => Some("git"),
        }
    }

    /// Derive the host for this item.
    ///
    /// For checkout-anchored items, uses the checkout's HostPath host.
    /// For all other items, falls back to the provided local host name.
    pub fn host(&self, local_host: &flotilla_protocol::HostName) -> flotilla_protocol::HostName {
        match self {
            CorrelationResult::Correlated(c) => c.host.clone().unwrap_or_else(|| local_host.clone()),
            _ => local_host.clone(),
        }
    }

    pub fn identity(&self) -> WorkItemIdentity {
        match self {
            CorrelationResult::Correlated(c) => match &c.anchor {
                CorrelatedAnchor::Checkout(co) => WorkItemIdentity::Checkout(co.key.clone()),
                CorrelatedAnchor::AttachableSet(id) => WorkItemIdentity::AttachableSet(id.clone()),
                CorrelatedAnchor::ChangeRequest(key) => WorkItemIdentity::ChangeRequest(key.clone()),
                CorrelatedAnchor::Session(key) => WorkItemIdentity::Session(key.clone()),
                CorrelatedAnchor::Agent(key) => WorkItemIdentity::Agent(key.clone()),
            },
            CorrelationResult::Standalone(s) => match s {
                StandaloneResult::Issue { key, .. } => WorkItemIdentity::Issue(key.clone()),
                StandaloneResult::RemoteBranch { branch } => WorkItemIdentity::RemoteBranch(branch.clone()),
            },
        }
    }

    pub fn as_correlated_mut(&mut self) -> Option<&mut CorrelatedWorkItem> {
        match self {
            CorrelationResult::Correlated(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub providers: Arc<ProviderData>,
    pub loading: bool,
    /// Set from the latest background refresh snapshot, for debug display.
    pub correlation_groups: Vec<CorrelatedGroup>,
    pub provider_health: HashMap<(&'static str, String), bool>,
}

pub struct SectionLabels {
    pub checkouts: String,
    pub change_requests: String,
    pub issues: String,
    pub sessions: String,
}

impl Default for SectionLabels {
    fn default() -> Self {
        Self {
            checkouts: "Checkouts".into(),
            change_requests: "Change Requests".into(),
            issues: "Issues".into(),
            sessions: "Sessions".into(),
        }
    }
}

/// Identifies a table section by the kind of work items it contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    Checkouts,
    AttachableSets,
    CloudAgents,
    ChangeRequests,
    RemoteBranches,
    Issues,
}

/// A single section's worth of sorted work items, ready for display.
#[derive(Debug, Clone)]
pub struct SectionData {
    pub kind: SectionKind,
    pub label: String,
    pub items: Vec<flotilla_protocol::WorkItem>,
}

/// Convert a correlation group into a CorrelationResult.
/// Returns None for groups that contain only workspaces (no checkout, PR, or session).
fn group_to_work_item(providers: &ProviderData, group: &CorrelatedGroup, group_idx: usize) -> Option<CorrelationResult> {
    let mut checkout_ref: Option<CheckoutRef> = None;
    let mut attachable_set_id: Option<flotilla_protocol::AttachableSetId> = None;
    let mut change_request_key: Option<String> = None;
    let mut session_key: Option<String> = None;
    let mut agent_key: Option<String> = None;
    let mut agent_keys: Vec<String> = Vec::new();
    let mut workspace_refs: Vec<String> = Vec::new();
    let mut terminal_ids: Vec<flotilla_protocol::AttachableId> = Vec::new();
    let mut host: Option<flotilla_protocol::HostName> = None;

    for item in &group.items {
        match (&item.kind, &item.source_key) {
            (CorItemKind::Checkout, ProviderItemKey::Checkout(path)) => {
                if checkout_ref.is_none() {
                    let is_main_checkout = providers.checkouts.get(path).is_some_and(|co| co.is_main);
                    checkout_ref = Some(CheckoutRef { key: path.clone(), is_main_checkout });
                    host = path.host_id().map(|h| flotilla_protocol::HostName::new(h.as_str()));
                }
            }
            (CorItemKind::AttachableSet, ProviderItemKey::AttachableSet(id)) => {
                attachable_set_id.get_or_insert_with(|| id.clone());
                if host.is_none() {
                    host = providers.attachable_sets.get(id).and_then(|set| {
                        set.checkout
                            .as_ref()
                            .and_then(|co| co.host_id().map(|h| flotilla_protocol::HostName::new(h.as_str())))
                            .or_else(|| set.host_affinity.clone())
                    });
                }
            }
            (CorItemKind::ChangeRequest, ProviderItemKey::ChangeRequest(id)) => {
                change_request_key = Some(id.clone());
            }
            (CorItemKind::CloudSession, ProviderItemKey::Session(id)) => {
                if session_key.is_none() {
                    session_key = Some(id.clone());
                }
            }
            (CorItemKind::Agent, ProviderItemKey::Agent(id)) => {
                agent_keys.push(id.clone());
                if agent_key.is_none() {
                    agent_key = Some(id.clone());
                }
            }
            (CorItemKind::Workspace, ProviderItemKey::Workspace(ws_ref)) => {
                if providers.workspaces.contains_key(ws_ref.as_str()) {
                    workspace_refs.push(ws_ref.clone());
                    if attachable_set_id.is_none() {
                        attachable_set_id = providers.workspaces.get(ws_ref).and_then(|ws| ws.attachable_set_id.clone());
                    }
                }
            }
            (CorItemKind::ManagedTerminal, ProviderItemKey::ManagedTerminal(key)) => {
                if let Some(terminal) = providers.managed_terminals.get(key) {
                    terminal_ids.push(key.clone());
                    if attachable_set_id.is_none() {
                        attachable_set_id = Some(terminal.set_id.clone());
                    }
                } else {
                    tracing::debug!(key = %key, "managed_terminals lookup miss in group_to_work_item");
                }
            }
            _ => {}
        }
    }

    let (anchor, linked_change_request, linked_session) = if let Some(co) = checkout_ref.clone() {
        (CorrelatedAnchor::Checkout(co), change_request_key.clone(), session_key.clone())
    } else if let Some(set_id) = &attachable_set_id {
        (CorrelatedAnchor::AttachableSet(set_id.clone()), change_request_key.clone(), session_key.clone())
    } else if let Some(key) = change_request_key.clone() {
        (CorrelatedAnchor::ChangeRequest(key), None, session_key.clone())
    } else if let Some(key) = session_key.clone() {
        (CorrelatedAnchor::Session(key), None, None)
    } else if let Some(key) = agent_key.clone() {
        (CorrelatedAnchor::Agent(key), None, None)
    } else {
        return None;
    };

    let branch = group.branch().map(|s| s.to_string());

    let pr_ref = match &anchor {
        CorrelatedAnchor::ChangeRequest(k) => Some(k.as_str()),
        _ => linked_change_request.as_deref(),
    };
    let session_ref = match &anchor {
        CorrelatedAnchor::Session(k) => Some(k.as_str()),
        _ => linked_session.as_deref(),
    };

    let pr_title = pr_ref.and_then(|k| providers.change_requests.get(k)).map(|cr| cr.title.clone()).filter(|t| !t.is_empty());
    let session_title = session_ref.and_then(|k| providers.sessions.get(k)).map(|s| s.title.clone()).filter(|t| !t.is_empty());
    let agent_title = agent_key.as_deref().and_then(|k| providers.agents.get(k)).map(|a| format!("{:?}", a.harness));
    let set_description = attachable_set_id.as_ref().and_then(|id| {
        providers.attachable_sets.get(id).and_then(|set| {
            set.checkout
                .as_ref()
                .and_then(|checkout| checkout.path.file_name().map(|name| name.to_string_lossy().to_string()))
                .filter(|name| !name.is_empty())
                .or_else(|| Some(id.to_string()))
        })
    });
    let description = pr_title.or(session_title).or(agent_title).or_else(|| branch.clone()).or(set_description).unwrap_or_default();

    let source = match &anchor {
        CorrelatedAnchor::Checkout(co) => co.key.host_id().map(|h| h.to_string()),
        CorrelatedAnchor::AttachableSet(id) => providers.attachable_sets.get(id).and_then(|set| {
            set.checkout
                .as_ref()
                .and_then(|co| co.host_id().map(|h| h.to_string()))
                .or_else(|| set.host_affinity.as_ref().map(ToString::to_string))
        }),
        CorrelatedAnchor::ChangeRequest(key) => {
            providers.change_requests.get(key.as_str()).map(|cr| cr.provider_display_name.clone()).filter(|s| !s.is_empty())
        }
        CorrelatedAnchor::Session(key) => {
            providers.sessions.get(key.as_str()).map(|s| s.provider_display_name.clone()).filter(|s| !s.is_empty())
        }
        CorrelatedAnchor::Agent(key) => {
            providers.agents.get(key.as_str()).map(|a| a.provider_display_name.clone()).filter(|s| !s.is_empty())
        }
    };

    Some(CorrelationResult::Correlated(CorrelatedWorkItem {
        anchor,
        checkout_ref,
        attachable_set_id,
        branch,
        description,
        linked_change_request,
        linked_session,
        linked_issues: Vec::new(),
        workspace_refs,
        correlation_group_idx: group_idx,
        host,
        source,
        terminal_ids,
        agent_keys,
    }))
}

/// Phases 1-3: Build CorrelatedItems, run union-find, convert to CorrelationResults.
/// Returns (work_items, correlation_groups).
pub fn correlate(providers: &ProviderData) -> (Vec<CorrelationResult>, Vec<CorrelatedGroup>) {
    // Phase 1: Build CorrelatedItems from identity-keyed sources.
    let mut items: Vec<CorrelatedItem> = Vec::new();

    for (path, co) in &providers.checkouts {
        items.push(CorrelatedItem {
            provider_name: "checkout".to_string(),
            kind: CorItemKind::Checkout,
            title: co.branch.clone(),
            correlation_keys: co.correlation_keys.clone(),
            source_key: ProviderItemKey::Checkout(path.clone()),
        });
    }

    for (id, set) in &providers.attachable_sets {
        let mut keys = vec![CorrelationKey::AttachableSet(id.clone())];
        if let Some(checkout) = &set.checkout {
            keys.push(CorrelationKey::CheckoutPath(checkout.clone()));
        }
        items.push(CorrelatedItem {
            provider_name: "attachable_set".to_string(),
            kind: CorItemKind::AttachableSet,
            title: set
                .checkout
                .as_ref()
                .and_then(|checkout| checkout.path.file_name().map(|name| name.to_string_lossy().to_string()))
                .unwrap_or_else(|| id.to_string()),
            correlation_keys: keys,
            source_key: ProviderItemKey::AttachableSet(id.clone()),
        });
    }

    for (id, cr) in &providers.change_requests {
        items.push(CorrelatedItem {
            provider_name: "change_request".to_string(),
            kind: CorItemKind::ChangeRequest,
            title: cr.title.clone(),
            correlation_keys: cr.correlation_keys.clone(),
            source_key: ProviderItemKey::ChangeRequest(id.clone()),
        });
    }

    for (id, session) in &providers.sessions {
        items.push(CorrelatedItem {
            provider_name: "session".to_string(),
            kind: CorItemKind::CloudSession,
            title: session.title.clone(),
            correlation_keys: session.correlation_keys.clone(),
            source_key: ProviderItemKey::Session(id.clone()),
        });
    }

    for (ws_ref, ws) in &providers.workspaces {
        items.push(CorrelatedItem {
            provider_name: "workspace".to_string(),
            kind: CorItemKind::Workspace,
            title: ws.name.clone(),
            correlation_keys: ws.attachable_set_id.as_ref().map(|id| vec![CorrelationKey::AttachableSet(id.clone())]).unwrap_or_default(),
            source_key: ProviderItemKey::Workspace(ws_ref.clone()),
        });
    }

    for (id, terminal) in &providers.managed_terminals {
        items.push(CorrelatedItem {
            provider_name: "terminal".to_string(),
            kind: CorItemKind::ManagedTerminal,
            title: format!("{} ({})", id, terminal.role),
            correlation_keys: vec![CorrelationKey::AttachableSet(terminal.set_id.clone())],
            source_key: ProviderItemKey::ManagedTerminal(id.clone()),
        });
    }

    for (id, agent) in &providers.agents {
        items.push(CorrelatedItem {
            provider_name: "agent".to_string(),
            kind: CorItemKind::Agent,
            title: format!("{:?}", agent.harness),
            correlation_keys: agent.correlation_keys.clone(),
            source_key: ProviderItemKey::Agent(id.clone()),
        });
    }

    // Phase 2: Run correlation engine
    let groups = correlation::correlate(items);

    // Phase 3: Convert groups to CorrelationResults
    let mut work_items: Vec<CorrelationResult> = Vec::new();
    let mut linked_issue_keys: HashSet<String> = HashSet::new();

    for (group_idx, group) in groups.iter().enumerate() {
        let mut work_item = match group_to_work_item(providers, group, group_idx) {
            Some(wi) => wi,
            None => continue,
        };

        // Post-correlation: link issues via association keys on change requests
        if let Some(change_request_key) = work_item.change_request_key() {
            if let Some(cr) = providers.change_requests.get(change_request_key) {
                let issue_ids: Vec<String> = cr
                    .association_keys
                    .iter()
                    .filter_map(|key| {
                        let AssociationKey::IssueRef(_, issue_id) = key;
                        if providers.issues.contains_key(issue_id.as_str()) {
                            Some(issue_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(c) = work_item.as_correlated_mut() {
                    for id in issue_ids {
                        if !c.linked_issues.contains(&id) {
                            c.linked_issues.push(id.clone());
                            linked_issue_keys.insert(id);
                        }
                    }
                }
            }
        }

        // Also link issues via association keys on checkouts (from git config)
        if let Some(co_key) = work_item.checkout_key() {
            if let Some(co) = providers.checkouts.get(co_key) {
                let issue_ids: Vec<String> = co
                    .association_keys
                    .iter()
                    .filter_map(|key| {
                        let AssociationKey::IssueRef(_, issue_id) = key;
                        if providers.issues.contains_key(issue_id.as_str()) {
                            Some(issue_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(c) = work_item.as_correlated_mut() {
                    for id in issue_ids {
                        if !c.linked_issues.contains(&id) {
                            c.linked_issues.push(id.clone());
                            linked_issue_keys.insert(id);
                        }
                    }
                }
            }
        }

        work_items.push(work_item);
    }

    // Add standalone issues (not linked to any PR)
    for (id, issue) in &providers.issues {
        if !linked_issue_keys.contains(id.as_str()) {
            work_items.push(CorrelationResult::Standalone(StandaloneResult::Issue {
                key: id.clone(),
                description: issue.title.clone(),
                source: issue.provider_display_name.clone(),
            }));
        }
    }

    // Add remote-only branches
    let known_branches: HashSet<String> = work_items.iter().filter_map(|wi| wi.branch().map(|b| b.to_string())).collect();
    for (b, branch_info) in &providers.branches {
        if branch_info.status == flotilla_protocol::BranchStatus::Remote
            && b.as_str() != "HEAD"
            && b.as_str() != "main"
            && b.as_str() != "master"
            && !known_branches.contains(b.as_str())
        {
            work_items.push(CorrelationResult::Standalone(StandaloneResult::RemoteBranch { branch: b.clone() }));
        }
    }

    (work_items, groups)
}

/// Sort tier for a checkout path relative to the repo root.
/// Tier 0 = child of repo root or sibling (same parent, name starts with repo name).
/// Tier 1 = everything else (external worktrees).
fn checkout_sort_tier(path: &Path, repo_root: &Path) -> u8 {
    // Child of repo root (e.g. repo_root/.worktrees/feat)
    if path.starts_with(repo_root) {
        return 0;
    }
    // Sibling (e.g. repo_root.branch-name)
    if let Some(parent) = repo_root.parent() {
        if let Ok(rel) = path.strip_prefix(parent) {
            let root_name = repo_root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            if rel.to_string_lossy().starts_with(root_name.as_ref()) {
                return 0;
            }
        }
    }
    1
}

/// Sort work items into typed sections, each with its own sorted item list.
///
/// Returns a `Vec<SectionData>` where each section is self-contained with its
/// kind, display label, and sorted items. Empty sections are omitted.
/// Display order: Checkouts, AttachableSets, CloudAgents, ChangeRequests,
/// RemoteBranches, Issues.
pub fn group_work_items_split(
    work_items: &[flotilla_protocol::WorkItem],
    providers: &ProviderData,
    labels: &SectionLabels,
    repo_root: &Path,
) -> Vec<SectionData> {
    let mut checkout_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut attachable_set_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut session_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut pr_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut remote_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();
    let mut issue_items: Vec<&flotilla_protocol::WorkItem> = Vec::new();

    for item in work_items {
        match item.kind {
            WorkItemKind::Checkout => checkout_items.push(item),
            WorkItemKind::AttachableSet => attachable_set_items.push(item),
            WorkItemKind::Session => session_items.push(item),
            WorkItemKind::ChangeRequest => pr_items.push(item),
            WorkItemKind::RemoteBranch => remote_items.push(item),
            WorkItemKind::Issue => issue_items.push(item),
            WorkItemKind::Agent => session_items.push(item),
        }
    }

    // Checkouts -- group by host, then main first within host, then proximity, then path
    checkout_items.sort_by_cached_key(|item| {
        let host_name = item.host.to_string();
        let main_tier = u8::from(!item.is_main_checkout);
        let key = item.checkout_key();
        let proximity_tier = key.map(|p| checkout_sort_tier(&p.path, repo_root)).unwrap_or(1);
        let path_key = key.map(|p| p.path.to_path_buf());
        (host_name, main_tier, proximity_tier, path_key)
    });

    // AttachableSets -- sorted by description
    attachable_set_items.sort_by(|a, b| a.description.cmp(&b.description));

    // Sessions/Agents -- grouped by provider, then sorted by updated_at descending
    session_items.sort_by(|a, b| {
        let a_ses = a.session_key.as_deref().and_then(|k| providers.sessions.get(k));
        let b_ses = b.session_key.as_deref().and_then(|k| providers.sessions.get(k));
        let a_provider = a_ses.map(|s| s.provider_name.as_str()).unwrap_or("");
        let b_provider = b_ses.map(|s| s.provider_name.as_str()).unwrap_or("");
        a_provider.cmp(b_provider).then_with(|| {
            let a_time = a_ses.and_then(|s| s.updated_at.as_deref());
            let b_time = b_ses.and_then(|s| s.updated_at.as_deref());
            b_time.cmp(&a_time)
        })
    });

    // PRs -- sorted by id descending
    pr_items.sort_by(|a, b| {
        let a_num = a.change_request_key.as_deref().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.change_request_key.as_deref().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });

    // Remote branches -- sorted by branch name
    remote_items.sort_by(|a, b| a.branch.cmp(&b.branch));

    // Issues -- sorted by id descending
    issue_items.sort_by(|a, b| {
        let a_num = a.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        let b_num = b.issue_keys.first().and_then(|k| k.parse::<i64>().ok());
        b_num.cmp(&a_num)
    });

    let mut sections: Vec<SectionData> = Vec::new();

    if !checkout_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::Checkouts,
            label: labels.checkouts.clone(),
            items: checkout_items.into_iter().cloned().collect(),
        });
    }
    if !attachable_set_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::AttachableSets,
            label: "Attachable Sets".into(),
            items: attachable_set_items.into_iter().cloned().collect(),
        });
    }
    if !session_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::CloudAgents,
            label: labels.sessions.clone(),
            items: session_items.into_iter().cloned().collect(),
        });
    }
    if !pr_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::ChangeRequests,
            label: labels.change_requests.clone(),
            items: pr_items.into_iter().cloned().collect(),
        });
    }
    if !remote_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::RemoteBranches,
            label: "Remote Branches".into(),
            items: remote_items.into_iter().cloned().collect(),
        });
    }
    if !issue_items.is_empty() {
        sections.push(SectionData {
            kind: SectionKind::Issues,
            label: labels.issues.clone(),
            items: issue_items.into_iter().cloned().collect(),
        });
    }

    sections
}

/// Filter archived/expired sessions from structured section data.
/// Removes sessions with archived or expired status from the CloudAgents section.
/// Agent items are never filtered. Drops sections that become empty.
pub fn filter_archived_sections(sections: Vec<SectionData>, providers: &ProviderData) -> Vec<SectionData> {
    use flotilla_protocol::SessionStatus;

    sections
        .into_iter()
        .filter_map(|mut section| {
            if section.kind == SectionKind::CloudAgents {
                section.items.retain(|item| {
                    if item.kind == WorkItemKind::Session {
                        let is_archived = item
                            .session_key
                            .as_deref()
                            .and_then(|k| providers.sessions.get(k))
                            .is_some_and(|s| matches!(s.status, SessionStatus::Archived | SessionStatus::Expired));
                        !is_archived
                    } else {
                        true // keep agents
                    }
                });
            }
            if section.items.is_empty() {
                None
            } else {
                Some(section)
            }
        })
        .collect()
}

pub async fn fetch_checkout_status(
    branch: &str,
    checkout_path: Option<&Path>,
    change_request_id: Option<&str>,
    repo_root: &Path,
    runner: &dyn crate::providers::CommandRunner,
) -> CheckoutStatus {
    let branch_owned = branch.to_string();
    let repo = repo_root.to_path_buf();
    let checkout_path = checkout_path.map(|p| p.to_path_buf());
    let cr_id = change_request_id.map(|s| s.to_string());
    let repo2 = repo_root.to_path_buf();

    let repo_for_base = repo.clone();
    let branch_for_base = branch_owned.clone();

    let (unpushed_result, uncommitted, pr_info) = tokio::join!(
        async {
            let base = async {
                let upstream =
                    run!(runner, "git", &["rev-parse", "--abbrev-ref", &format!("{branch_for_base}@{{upstream}}"),], &repo_for_base,);
                if let Ok(ref u) = upstream {
                    let u = u.trim();
                    if !u.is_empty() {
                        return Ok(u.to_string());
                    }
                }
                let remote_head = run!(runner, "git", &["rev-parse", "--abbrev-ref", "origin/HEAD"], &repo_for_base,);
                if let Ok(ref rh) = remote_head {
                    let rh = rh.trim();
                    if !rh.is_empty() {
                        return Ok(rh.to_string());
                    }
                }
                Err("Could not determine base branch — unpushed commit status unknown".to_string())
            }
            .await;

            match base {
                Ok(base_ref) => {
                    let log = run!(runner, "git", &["log", &format!("{base_ref}..{branch_for_base}"), "--oneline",], &repo_for_base,)
                        .unwrap_or_default();
                    Ok(log)
                }
                Err(warning) => Err(warning),
            }
        },
        async {
            if let Some(path) = &checkout_path {
                run!(runner, "git", &["status", "--porcelain"], path).unwrap_or_default()
            } else {
                String::new()
            }
        },
        async {
            if let Some(ref num) = cr_id {
                run!(runner, "gh", &["pr", "view", num, "--json", "state,mergeCommit"], &repo2,).ok()
            } else {
                None
            }
        },
    );

    let mut info = CheckoutStatus { branch: branch.to_string(), ..Default::default() };

    match unpushed_result {
        Ok(log_output) => {
            info.unpushed_commits = log_output.lines().map(|l| l.to_string()).filter(|l| !l.is_empty()).collect();
        }
        Err(warning) => {
            info.base_detection_warning = Some(warning);
        }
    }

    info.uncommitted_files = uncommitted.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect();
    info.has_uncommitted = !info.uncommitted_files.is_empty();

    if let Some(pr_json) = pr_info {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&pr_json) {
            info.change_request_status = v.get("state").and_then(|s| s.as_str()).map(|s| s.to_string());
            info.merge_commit_sha =
                v.get("mergeCommit").and_then(|mc| mc.get("oid")).and_then(|s| s.as_str()).map(|s| s[..7.min(s.len())].to_string());
        }
    }

    info
}

#[cfg(test)]
mod tests;
