//! Named-query result sets — the Aggregator's data plane.
//!
//! The Aggregator maintains incrementally-updated result sets for a small set
//! of named queries (e.g. [`QueryId::Convoys`]: all Convoys, durable ∪
//! observed, fleet-merged, joined with Presentation attach state). Clients
//! subscribe per query and receive a full [`ResultSet`] followed by
//! [`ResultDelta`]s. Rows are typed per query; presentation concerns
//! (columns, labels, tab composition) are consumer config and never appear
//! on the wire.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{host::HostName, provider_data::Issue, resource_ref::ResourceRef, snapshot::RepoKey, IssueRef, IssueSource, RepositoryKey};

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
    Independents,
    /// Open issues in one Project or Repository scope. Rows are populated
    /// only while at least one client subscribes to this query.
    Issues { scope: QueryScope },
}

/// Scope parameters owned by a curated query family.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    Project { namespace: String, name: String },
    Repository(RepositoryKey),
}

impl QueryId {
    /// Finite query families that are always materialized. Parameterized
    /// demand-backed queries cannot appear in a static list.
    pub const ALWAYS_MATERIALIZED: &'static [QueryId] = &[QueryId::Convoys, QueryId::Independents];

    pub fn family(&self) -> &'static str {
        match self {
            Self::Convoys => "convoys",
            Self::Independents => "independents",
            Self::Issues { .. } => "issues",
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
        changed: Vec<IndependentRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<ResourceRef>,
    },
    Issues {
        scope: QueryScope,
        changed: Vec<IssueRow>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<IssueRef>,
    },
}

impl QueryChanges {
    pub fn query(&self) -> QueryId {
        match self {
            Self::Convoys { .. } => QueryId::Convoys,
            Self::Independents { .. } => QueryId::Independents,
            Self::Issues { scope, .. } => QueryId::Issues { scope: scope.clone() },
        }
    }

    pub fn changed_len(&self) -> usize {
        match self {
            Self::Convoys { changed, .. } => changed.len(),
            Self::Independents { changed, .. } => changed.len(),
            Self::Issues { changed, .. } => changed.len(),
        }
    }

    pub fn removed_len(&self) -> usize {
        match self {
            Self::Convoys { removed, .. } | Self::Independents { removed, .. } => removed.len(),
            Self::Issues { removed, .. } => removed.len(),
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

    pub fn removed_resources(&self) -> Option<&[ResourceRef]> {
        match self {
            Self::Convoys { removed, .. } | Self::Independents { removed, .. } => Some(removed),
            Self::Issues { .. } => None,
        }
    }

    pub fn removed_issues(&self) -> Option<&[IssueRef]> {
        match self {
            Self::Issues { removed, .. } => Some(removed),
            _ => None,
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
}

impl ResultSetState {
    pub fn is_empty(&self) -> bool {
        self.demand.is_none() && self.conditions.is_empty()
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
}

/// Typed rows of a query result. The variant always matches the enclosing
/// `query` id; new named queries add a variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rows", rename_all = "snake_case")]
pub enum Rows {
    Convoys(Vec<ConvoyRow>),
    Independents(Vec<IndependentRow>),
    Issues { scope: QueryScope, rows: Vec<IssueRow> },
}

impl Rows {
    pub fn query(&self) -> QueryId {
        match self {
            Self::Convoys(_) => QueryId::Convoys,
            Self::Independents(_) => QueryId::Independents,
            Self::Issues { scope, .. } => QueryId::Issues { scope: scope.clone() },
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Convoys(rows) => rows.len(),
            Self::Independents(rows) => rows.len(),
            Self::Issues { rows, .. } => rows.len(),
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
            Self::Independents(rows) => Some(rows),
            _ => None,
        }
    }

    pub fn as_issues(&self) -> Option<&[IssueRow]> {
        match self {
            Self::Issues { rows, .. } => Some(rows),
            _ => None,
        }
    }
}

/// One row in an `issues{scope}` result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRow {
    pub reference: IssueRef,
    pub issue: Issue,
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
}

impl ConvoyPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
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
    /// Vessels from the convoy's workflow snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub vessels: Vec<VesselRow>,
}

/// One vessel within a [`ConvoyRow`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct VesselRow {
    /// `convoy_ref.subresource("vessels/{name}")`.
    pub resource: ResourceRef,
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
    /// Capability fact: the daemon will accept completing this vessel's work.
    #[builder(default)]
    pub complete_work: bool,
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

    use super::{DemandBackedMetadata, IssueRow, QueryChanges, QueryId, QueryScope, ResultDelta, ResultSetState};
    use crate::{provider_data::Issue, IssueRef, IssueSource, IssueState, RepositoryKey};

    #[test]
    fn issues_query_round_trips_with_owned_project_scope() {
        let query = QueryId::Issues { scope: QueryScope::Project { namespace: "flotilla".into(), name: "dashboard".into() } };

        let json = serde_json::to_string(&query).expect("serialize scoped query");
        assert_eq!(json, r#"{"issues":{"scope":{"project":{"namespace":"flotilla","name":"dashboard"}}}}"#);
        assert_eq!(serde_json::from_str::<QueryId>(&json).expect("deserialize scoped query"), query);
    }

    #[test]
    fn issues_query_round_trips_with_typed_repository_scope() {
        let query = QueryId::Issues { scope: QueryScope::Repository(RepositoryKey("repo_abc123".into())) };

        let json = serde_json::to_string(&query).expect("serialize scoped query");
        assert_eq!(json, r#"{"issues":{"scope":{"repository":"repo_abc123"}}}"#);
        assert_eq!(serde_json::from_str::<QueryId>(&json).expect("deserialize scoped query"), query);
    }

    #[test]
    fn issue_delta_round_trip_keeps_source_qualified_opaque_keys() {
        let scope = QueryScope::Project { namespace: "flotilla".into(), name: "dashboard".into() };
        let source = IssueSource { service: "https://linear.app".into(), scope: "WIDGET".into() };
        let reference = IssueRef { source: source.clone(), id: "WIDGET-123".into() };
        let delta = ResultDelta {
            seq: 4,
            changes: QueryChanges::Issues {
                scope: scope.clone(),
                changed: vec![IssueRow {
                    reference: reference.clone(),
                    issue: Issue {
                        reference: reference.clone(),
                        title: "Opaque identifiers survive".into(),
                        body: None,
                        state: IssueState::Open,
                        labels: vec!["protocol".into()],
                        as_of: Utc.with_ymd_and_hms(2026, 7, 15, 9, 30, 0).unwrap(),
                        association_keys: vec![],
                        provider_name: "linear".into(),
                        provider_display_name: "Linear".into(),
                    },
                }],
                removed: vec![IssueRef { source, id: "WIDGET-OLD".into() }],
            },
            state: None,
        };

        assert_eq!(delta.query(), QueryId::Issues { scope });
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
        let scope = QueryScope::Repository(RepositoryKey("repo_abc123".into()));
        let state = ResultSetState {
            demand: Some(DemandBackedMetadata {
                as_of: Utc.with_ymd_and_hms(2026, 7, 14, 19, 30, 0).single().expect("timestamp"),
                has_more: true,
            }),
            conditions: vec![],
        };
        let delta =
            ResultDelta { seq: 2, changes: QueryChanges::Issues { scope, changed: vec![], removed: vec![] }, state: Some(state.clone()) };

        let json = serde_json::to_string(&delta).expect("serialize metadata delta");
        let decoded = serde_json::from_str::<ResultDelta>(&json).expect("deserialize metadata delta");
        assert_eq!(decoded.state, Some(state));
        assert!(decoded.changes.is_empty());
    }
}
