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

use crate::{host::HostName, resource_ref::ResourceRef, snapshot::RepoKey};

pub type Timestamp = DateTime<Utc>;

/// Identifier of a named query maintained by the Aggregator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryId {
    /// All Convoys — durable ∪ observed, fleet-merged, joined with
    /// Presentation attach state. Rows are [`ConvoyRow`].
    Convoys,
}

impl QueryId {
    /// Every query the Aggregator maintains.
    pub const ALL: &'static [QueryId] = &[QueryId::Convoys];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Convoys => "convoys",
        }
    }
}

impl fmt::Display for QueryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    pub changed: Rows,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<ResourceRef>,
}

impl ResultDelta {
    pub fn query(&self) -> QueryId {
        self.changed.query()
    }
}

/// Typed rows of a query result. The variant always matches the enclosing
/// `query` id; new named queries add a variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "rows", rename_all = "snake_case")]
pub enum Rows {
    Convoys(Vec<ConvoyRow>),
}

impl Rows {
    pub fn query(&self) -> QueryId {
        match self {
            Self::Convoys(_) => QueryId::Convoys,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Convoys(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_convoys(&self) -> Option<&[ConvoyRow]> {
        match self {
            Self::Convoys(rows) => Some(rows),
        }
    }
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
    #[builder(default = true)]
    pub complete_work: bool,
}

/// Crew membership summary on a vessel row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewMemberSummary {
    pub role: String,
    pub command_preview: String,
}
