use flotilla_schema::{DecodeError, EncodeError, KindCode, RegistryError};

use crate::store::ResourceId;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
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
    #[error("a {kind} resource named {name} already exists here")]
    Exists { kind: KindCode, name: String },
    /// A `records` row may be replaced but never removed.
    #[error("table {table}: records rows cannot be removed")]
    AppendOnly { table: String },
    /// A `history` row is never changed or removed.
    #[error("table {table}: history rows are immutable")]
    Immutable { table: String },
    /// A stored value the descriptor does not allow, such as a malformed hash or a dangling id.
    #[error("table {table}, column {column}: stored value does not match the schema")]
    Corrupt { table: String, column: String },
}
