//! The descriptor the derive produces: one kind's tables, columns, keys, CHECKs and indexes.

use std::collections::BTreeSet;

use crate::{ddl::quote_ident, value::KindCode};

/// Schema object names of the shared tables and their indexes; no kind table or index may take one.
pub const SHARED_NAMES: &[&str] =
    &["strings", "resource_identity", "resource_identity_by_home", "objects", "home_sequence", "change_log", "change_log_by_resource"];

/// Column names the backend adds to a kind's root table.
pub const RESERVED_ROOT_COLUMNS: &[&str] = &["id", "version"];

/// Column names the backend adds to a kind's child tables.
pub const RESERVED_CHILD_COLUMNS: &[&str] = &["owner"];

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
    /// Sparse-column guards, in the order they closed; each renders as one table CHECK.
    pub guards: Vec<Guard>,
}

/// Columns that hold only while `column = value`: a data enum variant (`column` is the tag) or a
/// present `Option<flatten>` (`column` is `<field>_present`, `value` 1). Kept structured rather
/// than as SQL text so tests can derive the rows each CHECK must refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub column: String,
    pub value: i64,
    /// Columns that must be non-null while the guard holds.
    pub required: Vec<String>,
    /// Every column declared directly inside the guard; all null while it does not hold. A nested
    /// guard's columns are in its own scope only (its controlling column is in this one).
    pub scope: Vec<String>,
}

impl Guard {
    /// `CASE WHEN <cond> THEN <required> IS NOT NULL AND ... ELSE <scope> IS NULL AND ... END`.
    /// When an enclosing guard does not hold, this guard's controlling column is null, the
    /// condition is null and the ELSE branch applies.
    pub fn check(&self) -> String {
        self.check_with(" ")
    }

    /// The CHECK with `separator` before `THEN`, `ELSE` and each term, for readable DDL.
    pub fn check_with(&self, separator: &str) -> String {
        let and = format!("{separator}AND ");
        let then = conjunction(self.required.iter().map(|name| format!("{} IS NOT NULL", quote_ident(name))), &and);
        let otherwise = conjunction(self.scope.iter().map(|name| format!("{} IS NULL", quote_ident(name))), &and);
        format!("CASE WHEN {} = {}{separator}THEN {then}{separator}ELSE {otherwise}{separator}END", quote_ident(&self.column), self.value)
    }
}

fn conjunction(terms: impl Iterator<Item = String>, and: &str) -> String {
    let terms: Vec<String> = terms.collect();
    if terms.is_empty() {
        "1".to_owned()
    } else {
        terms.join(and)
    }
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
    /// data or key columns named like the backend's own columns: `id` and `version` on the root, `owner` on
    /// children (a child row may well have a `version` field, such as a gate input's observed version).
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
                let reserved = if table.parent.is_none() { RESERVED_ROOT_COLUMNS } else { RESERVED_CHILD_COLUMNS };
                if reserved.contains(&column.name.as_str()) {
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
