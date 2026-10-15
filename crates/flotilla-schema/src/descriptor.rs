//! The descriptor the derive produces: one kind's tables, columns, keys, CHECKs and indexes.

use std::collections::BTreeSet;

use crate::value::KindCode;

/// Schema object names of the shared tables and their indexes; no kind table or index may take one.
pub const SHARED_NAMES: &[&str] =
    &["strings", "resource_identity", "resource_identity_by_home", "objects", "home_sequence", "change_log", "change_log_by_resource"];

/// Column names the backend adds to every kind table: `id` and `version` on the root, `owner` on children.
pub const RESERVED_COLUMNS: &[&str] = &["id", "version", "owner"];

/// One kind's storage. Tables are numbered in pre-order of declaration, root first; the
/// encoder and decoder address tables by that index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSet {
    pub kind: String,
    pub code: KindCode,
    pub tables: Vec<Table>,
    pub indexes: Vec<Index>,
}

/// How a table's rows may change through the owner's write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableRole {
    /// The resource row: exactly one per resource.
    Root,
    /// A collection: rows are inserted, updated and deleted freely.
    Child,
    /// Append-only rows: new positions are inserted, existing rows may be replaced, none removed.
    Records,
    /// Append-only immutable rows: new positions only.
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub role: TableRole,
    pub parent: Option<usize>,
    /// The field this table stores, joined with its column prefix; empty for the root. Descendant
    /// tables name this table's own key columns `<field>_<column>`.
    pub field: String,
    /// The logical key, excluding the owner id: ancestors' own keys, then this table's own key.
    pub key: Vec<ColumnDef>,
    /// How many trailing `key` columns are this table's own key.
    pub own_key: usize,
    pub columns: Vec<ColumnDef>,
    /// Table CHECK constraints (guard checks), as SQL expressions.
    pub checks: Vec<String>,
}

impl Table {
    /// The key columns inherited from ancestors.
    pub fn parent_key(&self) -> &[ColumnDef] {
        &self.key[..self.key.len() - self.own_key]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Bool,
    Text,
    Blob,
    /// Microseconds since the Unix epoch.
    Timestamp,
    /// A stable enum code, restricted to the listed codes.
    Code(Vec<i64>),
    /// An id in `strings`.
    Interned,
    /// An id in `resource_identity`.
    Ref,
    /// A hash in `objects`.
    Frozen,
}

/// A declared index. Layer 5 fills in the predicate vocabulary; the descriptor only carries
/// what the renderer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    /// A rendered partial-index predicate.
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DescriptorError {
    /// Tables and indexes share one schema namespace.
    #[error("kind {kind}: schema object name {name} is declared twice")]
    DuplicateName { kind: String, name: String },
    #[error("kind {kind}: schema object name {name} is reserved for a shared table or index")]
    ReservedName { kind: String, name: String },
    #[error("kind {kind}: table {table} declares column {column} twice")]
    DuplicateColumn { kind: String, table: String, column: String },
    #[error("kind {kind}: table {table} uses the reserved column name {column}")]
    ReservedColumn { kind: String, table: String, column: String },
}

impl TableSet {
    /// Refuses descriptors that would render invalid or ambiguous DDL: table or index names declared
    /// twice or taken by a shared table, column names declared twice in one table (key columns included), and
    /// data or key columns named like the backend's own `id`, `version` and `owner` columns.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        let kind = || self.kind.clone();
        let mut names = BTreeSet::new();
        for name in self.object_names() {
            if SHARED_NAMES.contains(&name) {
                return Err(DescriptorError::ReservedName { kind: kind(), name: name.to_owned() });
            }
            if !names.insert(name) {
                return Err(DescriptorError::DuplicateName { kind: kind(), name: name.to_owned() });
            }
        }
        for table in &self.tables {
            let mut columns = BTreeSet::new();
            for column in table.key.iter().chain(&table.columns) {
                if RESERVED_COLUMNS.contains(&column.name.as_str()) {
                    return Err(DescriptorError::ReservedColumn { kind: kind(), table: table.name.clone(), column: column.name.clone() });
                }
                if !columns.insert(column.name.as_str()) {
                    return Err(DescriptorError::DuplicateColumn { kind: kind(), table: table.name.clone(), column: column.name.clone() });
                }
            }
        }
        Ok(())
    }

    /// The names this kind takes in the backend's schema namespace: its tables, then its indexes.
    pub fn object_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|table| table.name.as_str()).chain(self.indexes.iter().map(|index| index.name.as_str()))
    }
}
