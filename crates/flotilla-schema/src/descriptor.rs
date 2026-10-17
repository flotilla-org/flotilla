//! The descriptor the derive produces: one kind's tables, columns, keys, CHECKs and indexes.

use std::{collections::BTreeSet, ops::Range};

use crate::{code::Code, ddl::quote_ident, value::KindCode};

/// Schema object names of the shared tables and their indexes; no kind table or index may take one.
pub const SHARED_NAMES: &[&str] =
    &["strings", "resource_identity", "resource_identity_by_home", "objects", "home_sequence", "change_log", "change_log_by_resource"];

/// Column names the backend adds to a kind's root table.
pub const RESERVED_ROOT_COLUMNS: &[&str] = &["id", "version"];

/// Column names the backend adds to a kind's child tables.
pub const RESERVED_CHILD_COLUMNS: &[&str] = &["owner"];

/// The version stamp of a `records` or `history` row: the first data column of those tables, which
/// the backend sets to the sequence of the write that appended the row.
pub const RECORD_VERSION: &str = "version";

/// Column names reserved in `records` and `history` tables: the owner id and the version stamp.
pub const RESERVED_RECORD_COLUMNS: &[&str] = &["owner", RECORD_VERSION];

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
    /// Append-only rows, each stamped with the version that appended it: none is removed, and an
    /// existing row changes only through a declared edge (never through a whole-value write).
    Records,
    /// Append-only immutable rows, each stamped with the version that appended it.
    History,
    /// A collection datum of a label-plus-data state (`#[store(labels = L)]`): one row per map key
    /// or set value, holding rows only while the state's label carries the datum. Rows change one
    /// at a time, and only through an edge (`Edge::transition`), never through a spec edit.
    State,
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
    /// Tag and presence guards, in the order they were declared; each renders as one table CHECK.
    pub guards: Vec<Guard>,
    /// The data enums expanded into this table's columns, outermost and nested.
    pub enums: Vec<Expansion>,
    /// The column-shaped fields of the struct whose rows this table holds, in declaration order:
    /// the unit an edge sets (`Edge::set`), and the columns a spec edit may not change (`state`).
    /// Empty for tables of primitive and enum elements, which have no named fields.
    pub fields: Vec<FieldColumns>,
}

/// One column-shaped field of a row: a range of [`Table::columns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldColumns {
    /// The Rust field name, as handles name it.
    pub name: String,
    pub columns: Range<usize>,
    /// A `#[store(state)]` field: moved only by an edge, never by a spec edit.
    pub state: bool,
    /// A column datum of a label-plus-data state: the name of the state's label field, whose
    /// first column is the label. `None` for the label field itself and for every other field.
    /// The label field and its datums are set by one statement, so the per-label CHECK holds
    /// after it.
    pub datum_of: Option<String>,
}

impl FieldColumns {
    /// A state's label field: a `state` field that is no datum. Its first column is the label.
    pub fn is_label(&self) -> bool {
        self.state && self.datum_of.is_none()
    }
}

/// When a [`Guard`] holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// The controlling column holds this code: one label of a state, or one variant of a data enum.
    Is(Code),
    /// The controlling column is not null: an optional value is present. Its controlling column is
    /// the value's first required column, so presence needs no flag of its own (#3392 ruling G5).
    Present,
}

/// A rule on the columns of one row, one table CHECK. Two kinds:
///
/// - **A tag guard** (`When::Is`): while a state's label or a data enum's tag (mapping B, #3392
///   ruling G6) holds one code, the data that code carries as required is non-null and the data
///   it lacks is null. It has no ELSE: the guards of the other codes cover their own rows, and a
///   null tag (an absent enum) is covered by the guard of the value holding it.
/// - **A presence guard** (`When::Present`): an optional value of several columns is present when
///   its first required column is non-null; then its other required columns are non-null, and
///   otherwise every one of its columns is null.
///
/// Kept structured rather than as SQL text so tests can derive the rows each CHECK must refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    pub column: String,
    pub when: When,
    /// Columns that must be non-null while the guard holds.
    pub required: Vec<String>,
    /// Columns that must be null while the guard holds: the data a tag lacks.
    pub absent: Vec<String>,
    /// Columns that must be null while the guard does not hold: a presence guard's other columns.
    pub otherwise: Vec<String>,
}

impl Guard {
    /// `CASE WHEN <cond> THEN <required> IS NOT NULL AND <absent> IS NULL [ELSE <otherwise> IS
    /// NULL] END`. A CASE without a match is null, which a CHECK accepts.
    pub fn check(&self) -> String {
        self.check_with(" ")
    }

    /// The CHECK with `separator` before `THEN`, `ELSE` and each term, for readable DDL.
    pub fn check_with(&self, separator: &str) -> String {
        let and = format!("{separator}AND ");
        let required = self.required.iter().map(|name| format!("{} IS NOT NULL", quote_ident(name)));
        let absent = self.absent.iter().map(|name| format!("{} IS NULL", quote_ident(name)));
        let then = conjunction(required.chain(absent), &and);
        let column = quote_ident(&self.column);
        let condition = match &self.when {
            When::Is(code) => format!("{column} = {}", code_literal(*code)),
            When::Present => format!("{column} IS NOT NULL"),
        };
        let mut out = format!("CASE WHEN {condition}{separator}THEN {then}");
        if !self.otherwise.is_empty() {
            let otherwise = conjunction(self.otherwise.iter().map(|name| format!("{} IS NULL", quote_ident(name))), &and);
            out.push_str(&format!("{separator}ELSE {otherwise}"));
        }
        out.push_str(&format!("{separator}END"));
        out
    }
}

/// A code as an SQL string literal. Codes hold only letters, digits and `_`, so need no escaping.
pub fn code_literal(code: Code) -> String {
    format!("'{code}'")
}

/// The columns a data enum expands into in one table: its tag and every column its variants'
/// data declares, nested enums and optional values included. Recorded for the schema view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub tag: String,
    pub columns: Vec<String>,
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
    /// A stable enum code (short text), restricted to the listed codes.
    Code(Vec<Code>),
    /// An id in `strings`.
    Interned,
    /// An id in `resource_identity`.
    Ref,
    /// A hash in `objects`.
    Frozen,
    /// Canonical CBOR of a serde value, stored inline in the row (`#[store(cbor)]`).
    Cbor,
}

/// A declared index on one of a kind's tables: `eq` columns compared for equality, then `order`
/// columns, under a partial-index predicate. On the root table every query the backend generates
/// for it orders by `order..., id`, so the row id is the index's last key. On a nested table
/// (declared on the part struct whose rows it holds, such as an edge log's `EdgeEvent`) the
/// table's primary key `owner, ..., pos` ends the index instead, as SQLite appends it to every
/// index of a `WITHOUT ROWID` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The declared name; the schema object is [`Index::object_name`], `<table>_<name>`.
    pub name: String,
    pub table: String,
    pub eq: Vec<String>,
    pub order: Vec<String>,
    /// Conjoined; empty for a full index.
    pub predicate: Vec<Condition>,
}

/// One term of a partial-index predicate. Kept structured so the DDL and every query render the
/// same text: SQLite uses a partial index only when the query's WHERE repeats its terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// A code column holds one of `codes` (taken from the enum, never restated).
    In { column: String, codes: Vec<Code> },
    /// A nullable column is present.
    NotNull { column: String },
}

impl Condition {
    pub fn column(&self) -> &str {
        match self {
            Condition::In { column, .. } | Condition::NotNull { column } => column,
        }
    }

    pub fn sql(&self) -> String {
        match self {
            Condition::In { column, codes } => {
                let codes: Vec<String> = codes.iter().copied().map(code_literal).collect();
                format!("{} IN ({})", quote_ident(column), codes.join(", "))
            }
            Condition::NotNull { column } => format!("{} IS NOT NULL", quote_ident(column)),
        }
    }
}

impl Index {
    /// The schema object's name: indexes share the schema namespace with tables.
    pub fn object_name(&self) -> String {
        format!("{}_{}", self.table, self.name)
    }

    /// The index key: `eq` columns, then `order` columns.
    pub fn columns(&self) -> impl Iterator<Item = &str> {
        self.eq.iter().chain(&self.order).map(String::as_str)
    }

    /// The predicate's terms joined by `AND`; `None` for a full index.
    pub fn predicate_sql(&self) -> Option<String> {
        if self.predicate.is_empty() {
            None
        } else {
            Some(self.predicate.iter().map(Condition::sql).collect::<Vec<_>>().join(" AND "))
        }
    }
}

/// Why a declared index cannot be rendered or queried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexProblem {
    #[error("it is not on a table of this kind: {0}")]
    Table(String),
    #[error("it names no eq or order column")]
    NoColumns,
    #[error("column {0} is not a data column of the index's table")]
    UnknownColumn(String),
    #[error("column {0} is named twice")]
    RepeatedColumn(String),
    /// Keyset paging compares order columns; a null compares as unknown and would drop rows.
    #[error("order column {0} is nullable and the predicate does not require it")]
    NullableOrder(String),
    /// Interned strings and references are stored as local ids: ordering by them is insertion
    /// order in one store, different in every other.
    #[error("order column {0} holds local ids (interned or reference) and cannot order an index")]
    SurrogateOrder(String),
    #[error("column {0} is not a code column holding every listed code")]
    Codes(String),
    #[error("column {0} is not nullable, so `is_some` would be always true")]
    NotNullable(String),
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
    /// A `records` or `history` table whose first data column is not the version stamp.
    #[error("kind {kind}: table {table} does not start with the version stamp column")]
    MissingStamp { kind: String, table: String },
    #[error("kind {kind}: index {index}: {problem}")]
    Index { kind: String, index: String, problem: IndexProblem },
}

impl TableSet {
    /// Refuses descriptors that would render invalid or ambiguous DDL: table or index names declared
    /// twice or taken by a shared table, column names declared twice in one table (key columns included), and
    /// data or key columns named like the backend's own columns: `id` and `version` on the root, `owner` on
    /// children (a child row may well have a `version` field, such as a gate input's observed version),
    /// `owner` and `version` beside the stamp on `records` and `history` tables, which must start with it,
    /// and indexes that are not on one of the kind's tables, name unknown or repeated columns, page through a
    /// nullable column the predicate does not require, or whose predicate cannot hold as written.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        let kind = || self.kind.clone();
        let mut names = BTreeSet::new();
        for name in self.object_names() {
            if SHARED_NAMES.contains(&name.as_str()) {
                return Err(DescriptorError::ReservedName { kind: kind(), name });
            }
            if !names.insert(name.clone()) {
                return Err(DescriptorError::DuplicateName { kind: kind(), name });
            }
        }
        for index in &self.indexes {
            self.validate_index(index).map_err(|problem| DescriptorError::Index { kind: kind(), index: index.name.clone(), problem })?;
        }
        for table in &self.tables {
            let mut columns = BTreeSet::new();
            let (reserved, data) = match table.role {
                TableRole::Root => (RESERVED_ROOT_COLUMNS, &table.columns[..]),
                TableRole::Child | TableRole::State => (RESERVED_CHILD_COLUMNS, &table.columns[..]),
                TableRole::Records | TableRole::History => {
                    let stamp = ColumnDef { name: RECORD_VERSION.to_owned(), ty: ColumnType::Integer, nullable: false };
                    if table.columns.first() != Some(&stamp) {
                        return Err(DescriptorError::MissingStamp { kind: kind(), table: table.name.clone() });
                    }
                    (RESERVED_RECORD_COLUMNS, &table.columns[1..])
                }
            };
            for column in table.key.iter().chain(data) {
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

    fn validate_index(&self, index: &Index) -> Result<(), IndexProblem> {
        let root = self.tables.iter().find(|table| table.name == index.table).ok_or_else(|| IndexProblem::Table(index.table.clone()))?;
        if index.eq.is_empty() && index.order.is_empty() {
            return Err(IndexProblem::NoColumns);
        }
        let column =
            |name: &str| root.columns.iter().find(|column| column.name == name).ok_or_else(|| IndexProblem::UnknownColumn(name.to_owned()));
        let mut seen = BTreeSet::new();
        for name in index.columns() {
            column(name)?;
            if !seen.insert(name) {
                return Err(IndexProblem::RepeatedColumn(name.to_owned()));
            }
        }
        let required =
            |name: &str| index.predicate.iter().any(|condition| matches!(condition, Condition::NotNull { column } if column == name));
        for name in &index.order {
            let def = column(name)?;
            if matches!(def.ty, ColumnType::Interned | ColumnType::Ref) {
                return Err(IndexProblem::SurrogateOrder(name.clone()));
            }
            if def.nullable && !required(name) {
                return Err(IndexProblem::NullableOrder(name.clone()));
            }
        }
        for condition in &index.predicate {
            let def = column(condition.column())?;
            match condition {
                Condition::In { column, codes } => match &def.ty {
                    ColumnType::Code(allowed) if !codes.is_empty() && codes.iter().all(|code| allowed.contains(code)) => {}
                    _ => return Err(IndexProblem::Codes(column.clone())),
                },
                Condition::NotNull { column } if !def.nullable => return Err(IndexProblem::NotNullable(column.clone())),
                Condition::NotNull { .. } => {}
            }
        }
        Ok(())
    }

    /// The names this kind takes in the backend's schema namespace: its tables, then its indexes.
    pub fn object_names(&self) -> impl Iterator<Item = String> + '_ {
        self.tables.iter().map(|table| table.name.clone()).chain(self.indexes.iter().map(Index::object_name))
    }
}
