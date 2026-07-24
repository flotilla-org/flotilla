//! Named-query result sets — the Aggregator's data plane.
//!
//! The Aggregator maintains incrementally-updated result sets for a small set
//! of named queries (e.g. [`QueryId::Convoys`]: all Convoys, durable ∪
//! observed, fleet-merged, joined with Presentation attach state). Clients
//! subscribe per query and receive a full [`ResultSet`] followed by
//! [`ResultDelta`]s. Rows are typed per query; presentation concerns
//! (columns, labels, tab composition) are consumer config and never appear
//! on the wire.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    host::HostName,
    provider_data::{ChangeRequestStatus, Issue},
    resource_ref::ResourceRef,
    snapshot::RepoKey,
    IssueRef, IssueSource, IssueState, LifecycleAuthority, PrincipalRef, RepositoryKey,
};

pub type Timestamp = DateTime<Utc>;

/// Identifier of a named query maintained by the Aggregator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryId {
    /// All Convoys — durable ∪ observed, fleet-merged, joined with
    /// Presentation attach state. Rows are [`ConvoyRow`].
    Convoys,
    /// TerminalSessions with no Convoy association. Rows are
    /// [`IndependentRow`].
    Independents { scope: Option<QueryScope> },
    /// Open issues in one Project scope. Rows are populated
    /// only while at least one client subscribes to this query.
    Issues {
        scope: QueryScope,
        /// An ephemeral source search. `None` is the maintained open-issues
        /// window for the Project.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
        /// An optional provider-side label filter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Concrete observed checkouts fleet-wide (`None`) or in one Project.
    Checkouts { scope: Option<QueryScope> },
    /// Awareness-band enrichment tree, fleet-wide or in one Project.
    Awareness {
        scope: Option<QueryScope>,
        #[serde(default)]
        grouping: AwarenessGrouping,
        #[serde(default)]
        limit: AwarenessLimit,
    },
}

/// The Project scope owned by a curated query family. Repository membership
/// expansion is an Aggregator implementation detail and never crosses the
/// client protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryScope {
    pub namespace: String,
    pub name: String,
}

impl QueryScope {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self { namespace: namespace.into(), name: name.into() }
    }
}

impl QueryId {
    /// Finite query families that are always materialized. Parameterized
    /// demand-backed queries cannot appear in a static list.
    pub const ALWAYS_MATERIALIZED: &'static [QueryId] =
        &[QueryId::Convoys, QueryId::Independents { scope: None }, QueryId::Checkouts { scope: None }];

    pub fn family(&self) -> &'static str {
        match self {
            Self::Convoys => "convoys",
            Self::Independents { .. } => "independents",
            Self::Issues { .. } => "issues",
            Self::Checkouts { .. } => "checkouts",
            Self::Awareness { .. } => "awareness",
        }
    }
}

impl fmt::Display for QueryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.family())
    }
}

/// Full state of one named query's result set.
///
/// The query identity derives from the typed [`Rows`] variant (see
/// [`ResultSet::query`]) — a mismatched query/rows pair is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSet {
    pub seq: u64,
    pub rows: Rows,
    #[serde(default, skip_serializing_if = "ResultSetState::is_empty")]
    pub state: ResultSetState,
}

impl ResultSet {
    pub fn query(&self) -> QueryId {
        self.rows.query()
    }
}

/// Incremental update to a named query's result set.
///
/// Sequence numbers are contiguous per query: a delta is applicable iff
/// `seq == last_seen + 1`; anything else is a gap and the client must
/// resubscribe to get a fresh [`ResultSet`]. A removal-only delta carries an
/// empty `changed` variant, which still tags the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultDelta {
    pub seq: u64,
    pub changes: QueryChanges,
    /// Complete replacement of set-level state. `None` leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ResultSetState>,
}

impl ResultDelta {
    pub fn query(&self) -> QueryId {
        self.changes.query()
    }
}

/// Query-typed changed and removed rows. Keeping both halves in one variant
/// makes mismatched key families unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryChanges {
    Convoys {
        changed: Vec<ConvoyRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<ResourceRef>,
    },
    Independents {
        scope: Option<QueryScope>,
        changed: Vec<IndependentRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<ResourceRef>,
    },
    Issues {
        scope: QueryScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        changed: Vec<IssueRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<IssueRef>,
    },
    Checkouts {
        scope: Option<QueryScope>,
        changed: Vec<CheckoutRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<ResourceRef>,
    },
    Awareness {
        scope: Option<QueryScope>,
        grouping: AwarenessGrouping,
        limit: AwarenessLimit,
        changed: Vec<AwarenessNode>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
    },
}

impl QueryChanges {
    pub fn query(&self) -> QueryId {
        match self {
            Self::Convoys { .. } => QueryId::Convoys,
            Self::Independents { scope, .. } => QueryId::Independents { scope: scope.clone() },
            Self::Issues { scope, search, label, .. } => {
                QueryId::Issues { scope: scope.clone(), search: search.clone(), label: label.clone() }
            }
            Self::Checkouts { scope, .. } => QueryId::Checkouts { scope: scope.clone() },
            Self::Awareness { scope, grouping, limit, .. } => {
                QueryId::Awareness { scope: scope.clone(), grouping: *grouping, limit: *limit }
            }
        }
    }

    pub fn changed_len(&self) -> usize {
        match self {
            Self::Convoys { changed, .. } => changed.len(),
            Self::Independents { changed, .. } => changed.len(),
            Self::Issues { changed, .. } => changed.len(),
            Self::Checkouts { changed, .. } => changed.len(),
            Self::Awareness { changed, .. } => changed.len(),
        }
    }

    pub fn removed_len(&self) -> usize {
        match self {
            Self::Convoys { removed, .. } | Self::Independents { removed, .. } | Self::Checkouts { removed, .. } => removed.len(),
            Self::Issues { removed, .. } => removed.len(),
            Self::Awareness { removed, .. } => removed.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changed_len() == 0 && self.removed_len() == 0
    }

    pub fn as_convoys(&self) -> Option<&[ConvoyRow]> {
        match self {
            Self::Convoys { changed, .. } => Some(changed),
            _ => None,
        }
    }

    pub fn as_independents(&self) -> Option<&[IndependentRow]> {
        match self {
            Self::Independents { changed, .. } => Some(changed),
            _ => None,
        }
    }

    pub fn as_issues(&self) -> Option<&[IssueRow]> {
        match self {
            Self::Issues { changed, .. } => Some(changed),
            _ => None,
        }
    }

    pub fn as_checkouts(&self) -> Option<&[CheckoutRow]> {
        match self {
            Self::Checkouts { changed, .. } => Some(changed),
            _ => None,
        }
    }

    pub fn as_awareness(&self) -> Option<&[AwarenessNode]> {
        match self {
            Self::Awareness { changed, .. } => Some(changed),
            _ => None,
        }
    }

    pub fn removed_resources(&self) -> Option<&[ResourceRef]> {
        match self {
            Self::Convoys { removed, .. } | Self::Independents { removed, .. } | Self::Checkouts { removed, .. } => Some(removed),
            Self::Issues { .. } | Self::Awareness { .. } => None,
        }
    }

    pub fn removed_issues(&self) -> Option<&[IssueRef]> {
        match self {
            Self::Issues { removed, .. } => Some(removed),
            Self::Convoys { .. } | Self::Independents { .. } | Self::Checkouts { .. } | Self::Awareness { .. } => None,
        }
    }
}

/// Optional state carried beside a query's rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSetState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand: Option<DemandBackedMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<ResultSetCondition>,
    /// The query projection omitted rows after applying its own limit.
    ///
    /// This is independent of demand-backed [`DemandBackedMetadata::has_more`],
    /// which reports whether the underlying source window can be expanded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

impl ResultSetState {
    pub fn is_empty(&self) -> bool {
        self.demand.is_none() && self.conditions.is_empty() && !self.truncated
    }
}

/// Freshness and window state present only on demand-backed result sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandBackedMetadata {
    pub as_of: Timestamp,
    pub has_more: bool,
}

/// Explicit conditions affecting a result set without conflating them with
/// a valid empty row window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultSetCondition {
    IssueSourceUnavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<IssueSource>,
        message: String,
    },
    QueryScopeUnavailable {
        scope: QueryScope,
        message: String,
    },
}

/// Typed rows of a query result. The variant always matches the enclosing
/// `query` id; new named queries add a variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rows", rename_all = "snake_case")]
pub enum Rows {
    Convoys(Vec<ConvoyRow>),
    Independents {
        scope: Option<QueryScope>,
        rows: Vec<IndependentRow>,
    },
    Issues {
        scope: QueryScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        rows: Vec<IssueRow>,
    },
    Checkouts {
        scope: Option<QueryScope>,
        rows: Vec<CheckoutRow>,
    },
    Awareness {
        scope: Option<QueryScope>,
        grouping: AwarenessGrouping,
        limit: AwarenessLimit,
        rows: Vec<AwarenessNode>,
    },
}

impl Rows {
    pub fn query(&self) -> QueryId {
        match self {
            Self::Convoys(_) => QueryId::Convoys,
            Self::Independents { scope, .. } => QueryId::Independents { scope: scope.clone() },
            Self::Issues { scope, search, label, .. } => {
                QueryId::Issues { scope: scope.clone(), search: search.clone(), label: label.clone() }
            }
            Self::Checkouts { scope, .. } => QueryId::Checkouts { scope: scope.clone() },
            Self::Awareness { scope, grouping, limit, .. } => {
                QueryId::Awareness { scope: scope.clone(), grouping: *grouping, limit: *limit }
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Convoys(rows) => rows.len(),
            Self::Independents { rows, .. } => rows.len(),
            Self::Issues { rows, .. } => rows.len(),
            Self::Checkouts { rows, .. } => rows.len(),
            Self::Awareness { rows, .. } => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_convoys(&self) -> Option<&[ConvoyRow]> {
        match self {
            Self::Convoys(rows) => Some(rows),
            _ => None,
        }
    }

    pub fn as_independents(&self) -> Option<&[IndependentRow]> {
        match self {
            Self::Independents { rows, .. } => Some(rows),
            _ => None,
        }
    }

    pub fn as_issues(&self) -> Option<&[IssueRow]> {
        match self {
            Self::Issues { rows, .. } => Some(rows),
            _ => None,
        }
    }

    pub fn as_checkouts(&self) -> Option<&[CheckoutRow]> {
        match self {
            Self::Checkouts { rows, .. } => Some(rows),
            _ => None,
        }
    }

    pub fn as_awareness(&self) -> Option<&[AwarenessNode]> {
        match self {
            Self::Awareness { rows, .. } => Some(rows),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwarenessGrouping {
    #[default]
    Project,
    Convoy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AwarenessLimit {
    pub groups: usize,
    pub entries: usize,
}

impl Default for AwarenessLimit {
    fn default() -> Self {
        Self { groups: 32, entries: 32 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwarenessKind {
    Fleet,
    Project,
    Convoy,
    Issue,
    Vessel,
    Independent,
    Checkout,
}

/// Named-query family represented by one panel of a project awareness view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwarenessFamily {
    Convoys,
    Issues,
    Checkouts,
    Independents,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwarenessState {
    #[default]
    Unknown,
    Pending,
    Waiting,
    Active,
    Done,
    Failed,
    Cancelled,
}

/// Centrally-computed urgency of an awareness node or entry.
///
/// Surfaces render this value but must not derive it from the underlying
/// demand, regard, or attention facts themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Salience {
    #[default]
    None,
    Info,
    Attention,
    Urgent,
}

/// Centrally aggregated salience and freshness for one awareness family.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct AwarenessFamilySummary {
    pub family: AwarenessFamily,
    #[builder(default)]
    #[serde(default)]
    pub salience: Salience,
    pub as_of: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AwarenessPhase {
    Convoy(ConvoyPhase),
    Work(WorkPhase),
    Session(SessionPhase),
    Issue(IssueState),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct AwarenessCounts {
    #[builder(default)]
    pub total: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub active: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub waiting: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub done: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub failed: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub issues: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub convoys: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub vessels: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub checkouts: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    #[builder(default)]
    pub independents: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwarenessLink {
    pub rel: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct AwarenessEntry {
    pub id: String,
    pub kind: AwarenessKind,
    pub label: String,
    pub state: AwarenessState,
    #[builder(default)]
    #[serde(default)]
    pub salience: Salience,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<AwarenessPhase>,
    pub as_of: Timestamp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub refs: Vec<ResourceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub issue_refs: Vec<IssueRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub links: Vec<AwarenessLink>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[builder(default)]
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct AwarenessNode {
    pub id: String,
    pub kind: AwarenessKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<QueryScope>,
    pub state: AwarenessState,
    #[builder(default)]
    #[serde(default)]
    pub salience: Salience,
    pub as_of: Timestamp,
    #[serde(default, skip_serializing_if = "AwarenessCounts::is_empty")]
    #[builder(default)]
    pub counts: AwarenessCounts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub refs: Vec<ResourceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub issue_refs: Vec<IssueRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub links: Vec<AwarenessLink>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[builder(default)]
    pub annotations: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub entries: Vec<AwarenessEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub family_summaries: Vec<AwarenessFamilySummary>,
}

impl AwarenessNode {
    pub fn family_summary(&self, family: AwarenessFamily) -> Option<&AwarenessFamilySummary> {
        self.family_summaries.iter().find(|summary| summary.family == family)
    }
}

impl AwarenessCounts {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One row in an `issues{scope}` result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRow {
    pub reference: IssueRef,
    pub issue: Issue,
}

/// One concrete checkout observation in a scoped result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CheckoutRow {
    pub resource: ResourceRef,
    /// Canonical Repository identity used for joins.
    pub repo: RepositoryKey,
    /// Human-readable Repository presentation, never the opaque identity key.
    pub repo_label: String,
    /// Canonical cross-producer `vcs.repo` fact value. Kept separate from
    /// `repo_label`, which scoped projections may rewrite for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_fact: Option<RepoKey>,
    pub path: String,
    pub branch: String,
    pub host: HostName,
    pub authority: LifecycleAuthority,
}

/// Lifecycle phase of an independent terminal session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    #[default]
    Starting,
    Running,
    Stopped,
    Failed,
}

impl SessionPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for SessionPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the [`QueryId::Independents`] result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct IndependentRow {
    /// Row identity and merge key across hosts.
    pub resource: ResourceRef,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoKey>,
    /// Canonical cross-producer `vcs.repo` fact value. Kept separate from
    /// `repo`, which is a presentation label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_fact: Option<RepoKey>,
    /// Canonical Repository membership fact used by the Aggregator to derive
    /// Project-scoped result sets. This is data on the row, not a query scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_key: Option<RepositoryKey>,
    /// Host whose daemon can act on this session.
    pub host: HostName,
    /// Session reference accepted by `flotilla attach`. `Some` is a
    /// capability fact: the daemon can currently resolve the attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    pub phase: SessionPhase,
}

/// Convoy lifecycle phase as reported on query rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvoyPhase {
    #[default]
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

impl ConvoyPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for ConvoyPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Work lifecycle phase for one vessel within a convoy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPhase {
    #[default]
    Pending,
    Ready,
    Launching,
    Running,
    Complete,
    Failed,
    Cancelled,
    Abandoned,
}

impl WorkPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for WorkPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the [`QueryId::Convoys`] result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct ConvoyRow {
    /// Row identity and merge key across hosts.
    pub resource: ResourceRef,
    pub name: String,
    pub workflow_ref: String,
    /// Human principal that dispatched the convoy.
    #[builder(default)]
    #[serde(default)]
    pub dispatching_principal_ref: PrincipalRef,
    pub phase: ConvoyPhase,
    /// The convoy has no workflow snapshot yet and is not terminal.
    #[builder(default)]
    pub initializing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workflow_ref: Option<String>,
    /// The Project this convoy belongs to, from `ConvoySpec.project_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ref: Option<String>,
    /// Issues represented by this convoy, captured at admission time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub issues: Vec<ConvoyIssueRow>,
    /// Shallow forge lookup for the convoy branch. This is display/reference
    /// data, not a Flotilla-managed resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request: Option<ConvoyChangeRequest>,
    /// Vessels from the convoy's workflow snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub vessels: Vec<VesselRow>,
    #[builder(default)]
    pub needs_attention: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyIssueRow {
    pub reference: IssueRef,
    pub title: String,
    pub state: IssueState,
}

/// The external change request currently associated with a convoy branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoyChangeRequest {
    pub id: String,
    pub status: ChangeRequestStatus,
    pub repository_key: RepositoryKey,
}

/// One vessel within a [`ConvoyRow`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct VesselRow {
    /// `convoy_ref.subresource("vessels/{name}")`.
    pub resource: ResourceRef,
    /// Concrete Vessel resource realizing this workflow requirement, when it
    /// has been placed. Demand origins use this control-plane identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vessel_resource: Option<ResourceRef>,
    pub name: String,
    pub phase: WorkPhase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub crew: Vec<CrewMemberSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_stance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_stance: Option<String>,
    /// Names of sibling vessels this vessel depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub depends_on: Vec<String>,
    /// Host whose daemon can act on this vessel.
    pub host: HostName,
    /// Presentation join: the observed workspace reference for this vessel's
    /// running session. `Some` is a capability fact — the daemon will accept
    /// an attach on this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<String>,
    /// Terminal-session reference from which a presentation manager can
    /// materialize this vessel's workspace. This is deliberately distinct
    /// from `attach`, which names an already-observed PM workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialize: Option<String>,
    /// Capability fact: the daemon will accept completing this vessel's work.
    #[builder(default)]
    pub complete_work: bool,
    /// A live observation requests human attention; it never changes `phase`.
    #[builder(default)]
    pub needs_attention: bool,
}

/// Crew membership summary on a vessel row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewMemberSummary {
    pub role: String,
    pub command_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_stance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_stance: Option<String>,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{
        AwarenessGrouping, AwarenessLimit, CheckoutRow, DemandBackedMetadata, IndependentRow, IssueRow, QueryChanges, QueryId, QueryScope,
        ResultDelta, ResultSet, ResultSetState, Rows, SessionPhase,
    };
    use crate::{provider_data::Issue, HostName, IssueRef, IssueSource, IssueState, LifecycleAuthority, RepositoryKey, ResourceRef};

    #[test]
    fn fleet_checkout_result_set_round_trip_preserves_host_and_authority() {
        let set = ResultSet {
            seq: 7,
            rows: Rows::Checkouts {
                scope: None,
                rows: vec![CheckoutRow::builder()
                    .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "checkout-a").on_host(HostName::new("kiwi")))
                    .repo(RepositoryKey("repo_abc123".into()))
                    .repo_label("flotilla")
                    .path("/work/flotilla")
                    .branch("feature/query")
                    .host(HostName::new("kiwi"))
                    .authority(LifecycleAuthority::Adopted)
                    .build()],
            },
            state: ResultSetState::default(),
        };

        assert_eq!(set.query(), QueryId::Checkouts { scope: None });
        let json = serde_json::to_string(&set).expect("serialize checkout set");
        let decoded = serde_json::from_str::<ResultSet>(&json).expect("deserialize checkout set");
        assert_eq!(decoded, set);
    }

    #[test]
    fn project_independents_result_set_round_trip_preserves_scope_and_repository_membership() {
        let scope = QueryScope::new("flotilla", "roadmap");
        let row = IndependentRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", "governor"))
            .name("governor")
            .maybe_repo(Some(crate::RepoKey("flotilla".into())))
            .maybe_repository_key(Some(RepositoryKey("repository-flotilla".into())))
            .host(HostName::new("feta"))
            .maybe_attach(Some("governor".to_string()))
            .phase(SessionPhase::Running)
            .build();
        let set = ResultSet {
            seq: 7,
            rows: Rows::Independents { scope: Some(scope.clone()), rows: vec![row.clone()] },
            state: ResultSetState::default(),
        };

        let encoded = serde_json::to_string(&set).expect("serialize scoped independents");
        let decoded: ResultSet = serde_json::from_str(&encoded).expect("deserialize scoped independents");

        assert_eq!(decoded, set);
        assert_eq!(decoded.query(), QueryId::Independents { scope: Some(scope) });
        assert_eq!(decoded.rows.as_independents(), Some([row].as_slice()));
    }

    #[test]
    fn checkout_delta_round_trip_preserves_scope_rows_and_removals() {
        let scope = QueryScope::new("flotilla", "dashboard");
        let removed = ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "checkout-old").on_host(HostName::new("kiwi"));
        let delta = ResultDelta {
            seq: 8,
            changes: QueryChanges::Checkouts {
                scope: Some(scope.clone()),
                changed: vec![CheckoutRow::builder()
                    .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "checkout-new").on_host(HostName::new("mango")))
                    .repo(RepositoryKey("repo_abc123".into()))
                    .repo_label("flotilla")
                    .path("/work/flotilla")
                    .branch("feature/query")
                    .host(HostName::new("mango"))
                    .authority(LifecycleAuthority::Observed)
                    .build()],
                removed: vec![removed.clone()],
            },
            state: Some(ResultSetState {
                demand: None,
                conditions: vec![super::ResultSetCondition::QueryScopeUnavailable {
                    scope: scope.clone(),
                    message: "repository definition is temporarily unavailable".into(),
                }],
                truncated: false,
            }),
        };

        assert_eq!(delta.query(), QueryId::Checkouts { scope: Some(scope) });
        let json = serde_json::to_string(&delta).expect("serialize checkout delta");
        let decoded = serde_json::from_str::<ResultDelta>(&json).expect("deserialize checkout delta");
        assert_eq!(decoded, delta);
        assert_eq!(decoded.changes.removed_resources().expect("checkout removals"), &[removed]);
    }

    #[test]
    fn issues_query_round_trips_with_owned_project_scope() {
        let query = QueryId::Issues { scope: QueryScope::new("flotilla", "dashboard"), search: None, label: None };

        let json = serde_json::to_string(&query).expect("serialize scoped query");
        assert_eq!(json, r#"{"issues":{"scope":{"namespace":"flotilla","name":"dashboard"}}}"#);
        assert_eq!(serde_json::from_str::<QueryId>(&json).expect("deserialize scoped query"), query);
    }

    #[test]
    fn ephemeral_issue_search_is_part_of_query_identity() {
        let query = QueryId::Issues { scope: QueryScope::new("flotilla", "dashboard"), search: Some("is:open crash".into()), label: None };

        let json = serde_json::to_string(&query).expect("serialize scoped query");
        assert_eq!(json, r#"{"issues":{"scope":{"namespace":"flotilla","name":"dashboard"},"search":"is:open crash"}}"#);
        assert_eq!(serde_json::from_str::<QueryId>(&json).expect("deserialize scoped query"), query);
    }

    #[test]
    fn issue_label_filter_is_part_of_query_identity() {
        let query = QueryId::Issues { scope: QueryScope::new("flotilla", "dashboard"), search: None, label: Some("ready".into()) };

        let json = serde_json::to_string(&query).expect("serialize scoped query");
        assert_eq!(json, r#"{"issues":{"scope":{"namespace":"flotilla","name":"dashboard"},"label":"ready"}}"#);
        assert_eq!(serde_json::from_str::<QueryId>(&json).expect("deserialize scoped query"), query);
    }

    #[test]
    fn issue_delta_round_trip_keeps_source_qualified_opaque_keys() {
        let scope = QueryScope::new("flotilla", "dashboard");
        let source = IssueSource { service: "https://linear.app".into(), scope: "WIDGET".into() };
        let reference = IssueRef { source: source.clone(), id: "WIDGET-123".into() };
        let delta = ResultDelta {
            seq: 4,
            changes: QueryChanges::Issues {
                scope: scope.clone(),
                search: None,
                label: None,
                changed: vec![IssueRow {
                    reference: reference.clone(),
                    issue: Issue {
                        reference: reference.clone(),
                        title: "Opaque identifiers survive".into(),
                        body: None,
                        state: IssueState::Open,
                        labels: vec!["protocol".into()],
                        as_of: Utc.with_ymd_and_hms(2026, 7, 15, 9, 30, 0).unwrap(),
                        observed_at: None,
                        association_keys: vec![],
                        provider_name: "linear".into(),
                        provider_display_name: "Linear".into(),
                    },
                }],
                removed: vec![IssueRef { source, id: "WIDGET-OLD".into() }],
            },
            state: None,
        };

        assert_eq!(delta.query(), QueryId::Issues { scope, search: None, label: None });
        let json = serde_json::to_string(&delta).expect("serialize issue delta");
        let decoded = serde_json::from_str::<ResultDelta>(&json).expect("deserialize issue delta");
        assert_eq!(decoded, delta);
        let QueryChanges::Issues { changed, removed, .. } = decoded.changes else {
            panic!("expected issue changes");
        };
        assert_eq!(changed[0].reference, reference);
        assert_eq!(removed[0].id, "WIDGET-OLD");
    }

    #[test]
    fn delta_can_replace_demand_backed_set_metadata_without_row_changes() {
        let scope = QueryScope::new("flotilla", "dashboard");
        let state = ResultSetState {
            demand: Some(DemandBackedMetadata {
                as_of: Utc.with_ymd_and_hms(2026, 7, 14, 19, 30, 0).single().expect("timestamp"),
                has_more: true,
            }),
            conditions: vec![],
            truncated: false,
        };
        let delta = ResultDelta {
            seq: 2,
            changes: QueryChanges::Issues { scope, search: None, label: None, changed: vec![], removed: vec![] },
            state: Some(state.clone()),
        };

        let json = serde_json::to_string(&delta).expect("serialize metadata delta");
        let decoded = serde_json::from_str::<ResultDelta>(&json).expect("deserialize metadata delta");
        assert_eq!(decoded.state, Some(state));
        assert!(decoded.changes.is_empty());
    }

    #[test]
    fn projection_truncation_round_trips_without_demand_metadata() {
        let state = ResultSetState { truncated: true, ..ResultSetState::default() };
        let set = ResultSet {
            seq: 1,
            rows: Rows::Awareness {
                scope: None,
                grouping: AwarenessGrouping::Project,
                limit: AwarenessLimit { groups: 1, entries: 1 },
                rows: vec![],
            },
            state: state.clone(),
        };

        let json = serde_json::to_string(&set).expect("serialize truncated projection");
        assert!(json.contains(r#""state":{"truncated":true}"#));
        assert_eq!(serde_json::from_str::<ResultSet>(&json).expect("deserialize truncated projection").state, state);
    }
}
