use flotilla_schema::{Code, DecodeError, EncodeError, EventError, Hash, Identity, KindCode, RegistryError, Value};

use crate::store::ResourceId;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("change stream resource {}/{}/{}: {reason}", .identity.home, .identity.kind, .identity.name)]
    ResourceLimit { identity: Identity, reason: StreamResourceLimit },
    #[error("change stream: {0}")]
    Stream(String),
    /// The new store's pre-domain baseline has no migration path; reject it at open.
    #[error("strings table has no domain column: pre-domain dictionaries are unsupported; open a new domain-tagged store")]
    LegacyDictionarySchema,
    /// A kind table exists with other columns than its kind declares, such as one written before
    /// the parent key was named after its root table (#3627). Opening only creates missing tables,
    /// and SQLite reads an unknown double-quoted column name as a string literal, so a stale table
    /// would read as empty rather than fail; reject it at open.
    #[error("kind table {table} has columns {found:?}, not {expected:?}: the database predates this schema and has no migration; open a new store")]
    KindTableDrift { table: String, expected: Vec<String>, found: Vec<String> },
    /// A kind table's stored CHECK on a coded column lacks codes its kind now declares, and the
    /// table differs from its kind's in more than its code lists, so opening cannot rebuild it
    /// with the wider CHECK (#3972); the old CHECK would refuse the first write of a new code.
    #[error("kind table {table} column {column} refuses codes {missing:?}: the table differs in more than its codes and has no migration; open a new store")]
    KindCodeDrift { table: String, column: String, missing: Vec<String> },
    /// A kind table's stored CHECK on a coded column accepts codes its kind no longer declares:
    /// rows may hold them, and this build cannot read them back.
    #[error("kind table {table} column {column} holds codes {removed:?} its kind no longer declares: the database is from a later or other schema; open a new store")]
    KindCodeRemoved { table: String, column: String, removed: Vec<String> },
    /// A kind table's coded column has no stored `CHECK (<column> IN (...))` this store can read,
    /// so whether it accepts the kind's codes is unknown; refused rather than assumed current.
    #[error(
        "kind table {table} column {column} has no readable code CHECK: the database predates this schema or was altered; open a new store"
    )]
    KindCodeUnreadable { table: String, column: String },
    /// Rebuilding a kind table with wider codes left rows whose foreign keys do not resolve; the
    /// open transaction rolls the rebuild back.
    #[error("kind table {table} has {violations} foreign key violations after widening its codes")]
    KindCodeWidening { table: String, violations: usize },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Encode(#[from] EncodeError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("kind code {code} is not registered with this store")]
    UnknownKind { code: KindCode },
    #[error("resource {id} is of kind {found}, not {expected}")]
    WrongKind { id: ResourceId, expected: KindCode, found: KindCode },
    #[error("resource {id} does not exist")]
    NotFound { id: ResourceId },
    /// Only the replication applier writes rows homed elsewhere.
    #[error("resource {id} is homed at {home}, not here")]
    NotHome { id: ResourceId, home: String },
    /// Optimistic concurrency: the resource changed since the caller loaded `expected`.
    #[error("resource {id} is no longer at version {expected}")]
    Conflict { id: ResourceId, expected: u64 },
    /// An edge whose event leaves label `from` while the stored state `field` holds label
    /// `stored`, at the version the caller read (#3373 ruling 5). Checked inside the write, after
    /// the version: a stale stamp is `Conflict` whatever the label.
    #[error("resource {id}: the edge leaves label {from} of {field}, which holds label {stored}")]
    WrongFrom { id: ResourceId, field: String, from: Code, stored: String },
    #[error("a {kind} resource named {name} already exists here")]
    Exists { kind: KindCode, name: String },
    /// A spec edit (`update`) changed what only an edge writes: a `state` column (`column` is
    /// set), or a `records` or `history` table, by an append, a change or a removal (`column` is
    /// `None`). Rows nested in a spec child row the edit inserts or removes go with that row.
    #[error("table {table}{}: changes only through an edge", column.as_ref().map(|column| format!(", column {column}")).unwrap_or_default())]
    NotThroughEdge { table: String, column: Option<String> },
    /// An edge addressed a `records` row that does not exist: to set a field of it, or to append
    /// under it.
    #[error("table {table} holds no row at {key:?}")]
    NoRecord { table: String, key: Vec<Value> },
    /// An edge named a table or field that the kind does not declare in that role. Derived handles
    /// never do; a hand-built one may.
    #[error("table {table} declares no edge target {field}")]
    EdgeTarget { table: String, field: String },
    /// Every `u32` position of a records or history table is taken.
    #[error("table {table} has no position left")]
    PositionsExhausted { table: String },
    /// A stored value the descriptor does not allow, such as a malformed hash or a dangling id.
    #[error("table {table}, column {column}: stored value does not match the schema")]
    Corrupt { table: String, column: String },
    /// A log entry that could not be encoded within its limits, or a stored one that does not
    /// decode.
    #[error(transparent)]
    Event(#[from] EventError),
    /// An edge code the kind does not declare (`#[store(edges = E)]`).
    #[error("kind code {kind} declares no edge {edge}")]
    UnknownEdge { kind: KindCode, edge: Code },
    /// An edge's `from` or `to` that is no label of the kind's root state.
    #[error("kind code {kind}'s state has no label {label}")]
    UnknownLabel { kind: KindCode, label: Code },
    /// An expunge of a resource that is not deleted: delete it first.
    #[error("resource {id} is not deleted")]
    Live { id: ResourceId },
    /// An expunge of a resource another resource's row still references.
    #[error("resource {id} is still referenced")]
    Referenced { id: ResourceId },
    /// `apply` writes only resources homed elsewhere; this home's changes come from its own writes.
    #[error("change homed at {home} is this store's own home")]
    OwnHome { home: String },
    /// A replicated object whose bytes do not hash to its name.
    #[error("object {hash:?} does not match its bytes")]
    TamperedObject { hash: Hash },
    /// A replicated change names an object this store does not hold and the change does not carry.
    #[error("object {hash:?} is not stored here")]
    MissingObject { hash: Hash },
    /// A change that does not follow the identity's last applied change: `found` is its
    /// `prev_seq`, `expected` the identity's sequence here.
    #[error("resource is at sequence {expected}; the change follows {found}")]
    Gap { expected: u64, found: u64 },
    /// A listing asked for no rows; every listing takes a positive limit.
    #[error("a page limit must be at least 1")]
    ZeroLimit,
    #[error("kind code {kind} declares no index {index}")]
    UnknownIndex { kind: KindCode, index: String },
    #[error("index {index} of {kind} does not have a single integer order column")]
    IntegerIndex { kind: KindCode, index: String },
    /// A cursor from another query: a different kind, index or `eq` arguments.
    #[error("cursor does not belong to this query of index {index}")]
    CursorMismatch { index: String },
    #[error("malformed event: {0}")]
    MalformedEvent(#[from] Malformed),
}

/// Why `apply` refused an event's shape.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Malformed {
    #[error("table {table} is not a table of this kind")]
    UnknownTable { table: String },
    /// A key or value count that does not match the table's columns, or a root key that is not empty.
    #[error("row of table {table} does not match its columns")]
    Shape { table: String },
    /// A change naming a field the table does not declare, or a `State` naming no state.
    #[error("table {table} declares no field {field} a change sets")]
    UnknownField { table: String, field: String },
    /// A change naming a column the table, field or key does not declare.
    #[error("table {table} declares no column {column} there")]
    UnknownColumn { table: String, column: String },
    /// Root rows are removed by `DeleteResource` only.
    #[error("root row of table {table} deleted by row")]
    RootDelete { table: String },
    /// A home sequence this store already logged for another resource of that home: a home
    /// numbers each change once, so the sender reused it.
    #[error("home sequence {home_seq} of {home} is already logged")]
    ReusedSequence { home: String, home_seq: u64 },
    /// Above `i64::MAX`, or 0, which no write takes (sequences start at 1).
    #[error("home sequence {home_seq} cannot be stored")]
    Sequence { home_seq: u64 },
}

/// Why a snapshot resource cannot be delivered within the stream's bounds.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamResourceLimit {
    #[error("single row exceeds frame limit ({limit} bytes)")]
    Row { limit: usize },
    #[error("resource exceeds assembly limit ({bytes} bytes, limit {limit})")]
    Assembly { bytes: usize, limit: usize },
}
