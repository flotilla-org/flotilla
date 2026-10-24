//! The descriptor the derive produces: one kind's tables, columns, keys, CHECKs and indexes.

use std::{collections::BTreeSet, ops::Range};

use crate::{code::Code, value::KindCode};

/// Reserved suffix of the backend's readable views.
pub const VIEW_SUFFIX: &str = "_view";

/// Schema object names of the shared tables and their indexes; no kind table or index may take one.
pub const SHARED_NAMES: &[&str] = &[
    "strings",
    "resource_identity",
    "resource_identity_by_home",
    "objects",
    "home_sequence",
    "log",
    "log_by_resource",
    "log_about",
    "log_expunges",
    "codes",
];

/// Column names the backend adds to a kind's root table.
pub const RESERVED_ROOT_COLUMNS: &[&str] = &["id", "version"];

/// The version stamp of a `records` or `history` row: the first data column of those tables, which
/// the backend sets to the sequence of the write that appended the row.
pub const RECORD_VERSION: &str = "version";

/// Column names reserved in `records` and `history` tables beside the parent key
/// ([`TableSet::parent_key_column`]): the version stamp.
pub const RESERVED_RECORD_COLUMNS: &[&str] = &[RECORD_VERSION];

/// One kind's storage. Tables are numbered in pre-order of declaration, root first; the
/// encoder and decoder address tables by that index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSet {
    /// Whether storage invariants apply; independent of generated-column metadata.
    pub mode: SchemaMode,
    pub kind: String,
    pub code: KindCode,
    pub tables: Vec<Table>,
    pub indexes: Vec<Index>,
    /// The codes of the kind's edges (`#[store(edges = E)]`), ascending: the only `edge` codes
    /// its log entries may carry. Empty for a kind that declares none, which takes no edge.
    pub edges: Vec<Code>,
    /// The Rust names of the variants carrying [`TableSet::edges`], in the same order: the
    /// `codes` table's names for the kind's log entries, which only the readable views read.
    pub edge_names: Vec<String>,
}

/// The purpose of a table description. This is never inferred from its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode {
    Stored,
    DescribeOnly,
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
    /// Columns supplied by the storage layout (empty for describe-only sources).
    pub generated: Vec<ColumnDef>,
    /// Tag and presence guards, in the order they were declared; each renders as one table CHECK.
    pub guards: Vec<Guard>,
    /// The data enums expanded into this table's columns, outermost and nested.
    pub enums: Vec<Expansion>,
    /// The column-shaped fields of the struct whose rows this table holds, in declaration order:
    /// the unit an edge sets (`Edge::set`), and the columns a spec edit may not change (`state`).
    /// Empty for tables of primitive and enum elements, which have no named fields.
    pub fields: Vec<FieldColumns>,
    /// Earlier names of this table's `field` (`#[store(alias = "old")]` on it or on a field
    /// enclosing it in its parent row): a log entry written under one still names this table.
    pub field_aliases: Vec<String>,
    /// Earlier names of columns, as `(old, current)`: a renamed field's columns keep their
    /// names under the field's alias, so a log entry written under one still decodes.
    pub aliases: Vec<(String, String)>,
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
    /// Earlier names of the field (`#[store(alias = "old")]`).
    pub aliases: Vec<String>,
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
    Is(String),
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

/// The columns a data enum expands into in one table: its tag and every column its variants'
/// data declares, nested enums and optional values included. Recorded for the schema view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub tag: String,
    pub columns: Vec<String>,
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
    /// Enum values and their Rust variant names (ascending code order for stored enums).
    pub categories: Vec<(String, String)>,
    /// Logical target table of a reference; identities without a static kind target
    /// the shared resource identity table.
    pub reference: Option<String>,
    /// Open semantic properties. Structural roles live in `roles` as an array of strings.
    pub properties: std::collections::BTreeMap<String, serde_json::Value>,
}

impl ColumnDef {
    /// An explicit ADR 0047 additive default, restricted to integer/bool columns.
    pub fn previous_generation_default(&self) -> Option<i64> {
        if !matches!(self.ty, ColumnType::Integer | ColumnType::Bool) {
            return None;
        }
        self.properties.get("previous_generation_default").and_then(serde_json::Value::as_i64)
    }

    pub fn new(name: impl Into<String>, ty: ColumnType, nullable: bool) -> Self {
        Self { name: name.into(), ty, nullable, categories: Vec::new(), reference: None, properties: Default::default() }
    }

    pub fn add_role(&mut self, role: &str) {
        let roles = self.properties.entry("roles".into()).or_insert_with(|| serde_json::json!([]));
        let roles = roles.as_array_mut().expect("roles is an array");
        if !roles.iter().any(|value| value.as_str() == Some(role)) {
            roles.push(role.into());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    /// Floating-point values in describe-only sources; Stored does not implement a codec for them.
    Number,
    Bool,
    Text,
    Blob,
    /// Microseconds since the Unix epoch.
    Timestamp,
    /// An enum value, restricted to the listed strings. Stored derives validate short explicit
    /// codes; describe-only sources retain their existing (possibly longer) enum values.
    Code(Vec<String>),
    /// An id in `strings`, whose row must carry this domain.
    Interned(Code),
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
/// (declared on the part struct whose rows it holds) the
/// table's primary key `<root>, ..., pos` ends the index instead, as SQLite appends it to every
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
    /// No two rows of one owner share the `eq` columns. Only on a nested table, whose index then
    /// leads with the owner's key ([`TableSet::index_key`]): a root row's uniqueness would span
    /// every resource of a store, replicas included, which no home could decide alone.
    pub unique: bool,
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
    #[error("a unique index belongs on a nested table, unique within its owner, with eq columns only and no predicate")]
    Unique,
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
    /// An alias (`#[store(alias = "old")]`) that names something else of its table or row too: a
    /// current column, field or table field, or another alias, so an old name would be ambiguous.
    #[error("kind {kind}: table {table}: alias {alias} names something else too")]
    AliasClash { kind: String, table: String, alias: String },
    /// A kind declaring edges (`#[store(edges = E)]`) has exactly one state at its root, whose
    /// labels an edge's `from` and `to` name.
    #[error("kind {kind} declares edges but has {states} root states, not one")]
    EdgeState { kind: String, states: usize },
}

impl TableSet {
    /// The column the backend adds to every table below the root, holding the resource's row id:
    /// named after the root table, so `convoy_promises.convoy` and `message_references.message`
    /// read as what they reference (ADR 0064 Records).
    pub fn parent_key_column(&self) -> &str {
        &self.tables[0].name
    }

    /// Refuses descriptors that would render invalid or ambiguous DDL: table or index names declared
    /// twice or taken by a shared table, column names declared twice in one table (key columns included), and
    /// data or key columns named like the backend's own columns: `id` and `version` on the root, the parent
    /// key (named after the root table) on children (a child row may well have a `version` field, such as a
    /// gate input's observed version), the parent key and `version` beside the stamp on `records` and
    /// `history` tables, which must start with it,
    /// and indexes that are not on one of the kind's tables, name unknown or repeated columns, page through a
    /// nullable column the predicate does not require, or whose predicate cannot hold as written.
    pub fn validate(&self) -> Result<(), DescriptorError> {
        let kind = || self.kind.clone();
        let stored = self.mode == SchemaMode::Stored;
        let mut names = BTreeSet::new();
        for name in self.object_names() {
            if stored && SHARED_NAMES.contains(&name.as_str()) {
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
                TableRole::Root => (if stored { RESERVED_ROOT_COLUMNS } else { &[] }, &table.columns[..]),
                TableRole::Child | TableRole::State => (&[][..], &table.columns[..]),
                TableRole::Records | TableRole::History => {
                    if !table
                        .columns
                        .first()
                        .is_some_and(|column| column.name == RECORD_VERSION && column.ty == ColumnType::Integer && !column.nullable)
                    {
                        return Err(DescriptorError::MissingStamp { kind: kind(), table: table.name.clone() });
                    }
                    (RESERVED_RECORD_COLUMNS, &table.columns[1..])
                }
            };
            let parent_key = table.parent.is_some() && (stored || matches!(table.role, TableRole::Records | TableRole::History));
            for column in table.key.iter().chain(data) {
                if reserved.contains(&column.name.as_str()) || (parent_key && column.name == self.parent_key_column()) {
                    return Err(DescriptorError::ReservedColumn { kind: kind(), table: table.name.clone(), column: column.name.clone() });
                }
                if !columns.insert(column.name.as_str()) {
                    return Err(DescriptorError::DuplicateColumn { kind: kind(), table: table.name.clone(), column: column.name.clone() });
                }
            }
            let clash = |alias: &str| DescriptorError::AliasClash { kind: kind(), table: table.name.clone(), alias: alias.to_owned() };
            let mut old = BTreeSet::new();
            for (alias, _) in &table.aliases {
                if columns.contains(alias.as_str()) || !old.insert(alias.as_str()) {
                    return Err(clash(alias));
                }
            }
            let fields: BTreeSet<&str> = table.fields.iter().map(|field| field.name.as_str()).collect();
            let mut old = BTreeSet::new();
            for alias in table.fields.iter().flat_map(|field| &field.aliases) {
                if fields.contains(alias.as_str()) || !old.insert(alias.as_str()) {
                    return Err(clash(alias));
                }
            }
            let index = self.tables.iter().position(|candidate| std::ptr::eq(candidate, table));
            let children: Vec<&Table> = self.tables.iter().filter(|child| child.parent.is_some() && child.parent == index).collect();
            let mut old = BTreeSet::new();
            for alias in children.iter().flat_map(|child| &child.field_aliases) {
                if children.iter().any(|child| &child.field == alias) || !old.insert(alias.as_str()) {
                    return Err(clash(alias));
                }
            }
        }
        if !self.edges.is_empty() {
            let states = self.tables[0].fields.iter().filter(|field| field.is_label()).count();
            if states != 1 {
                return Err(DescriptorError::EdgeState { kind: kind(), states });
            }
        }
        Ok(())
    }

    /// The root's state field, whose labels an edge's `from` and `to` name, and its label codes:
    /// `None` unless the kind has exactly one root state.
    pub fn edge_state(&self) -> Option<(&FieldColumns, Vec<Code>)> {
        let root = &self.tables[0];
        let mut labels = root.fields.iter().filter(|field| field.is_label());
        let (field, None) = (labels.next()?, labels.next()) else { return None };
        match &root.columns[field.columns.start].ty {
            ColumnType::Code(codes) => Some((field, codes.iter().map(|code| Code::parse(code)).collect::<Result<_, _>>().ok()?)),
            _ => None,
        }
    }

    fn validate_index(&self, index: &Index) -> Result<(), IndexProblem> {
        let root = self.tables.iter().find(|table| table.name == index.table).ok_or_else(|| IndexProblem::Table(index.table.clone()))?;
        if index.eq.is_empty() && index.order.is_empty() {
            return Err(IndexProblem::NoColumns);
        }
        if index.unique && (root.parent.is_none() || index.eq.is_empty() || !index.order.is_empty() || !index.predicate.is_empty()) {
            return Err(IndexProblem::Unique);
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
            if matches!(def.ty, ColumnType::Interned(_) | ColumnType::Ref) {
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
                    ColumnType::Code(allowed) if !codes.is_empty() && codes.iter().all(|code| allowed.contains(&code.to_string())) => {}
                    _ => return Err(IndexProblem::Codes(column.clone())),
                },
                Condition::NotNull { column } if !def.nullable => return Err(IndexProblem::NotNullable(column.clone())),
                Condition::NotNull { .. } => {}
            }
        }
        Ok(())
    }

    /// The names this kind takes in the backend's schema namespace: its tables, then its declared
    /// and its reference indexes, then its tables' readable views.
    pub fn object_names(&self) -> impl Iterator<Item = String> + '_ {
        let references = self.reference_indexes().into_iter().map(|index| index.object_name());
        let views = self.tables.iter().map(|table| format!("{}{VIEW_SUFFIX}", table.name));
        self.tables.iter().map(|table| table.name.clone()).chain(self.indexes.iter().map(Index::object_name)).chain(references).chain(views)
    }

    /// An index's key as rendered: a unique index on a nested table leads with its owner's key
    /// (the resource's row id, then the parent rows' keys), so it is unique within one owner,
    /// as a seat name is within one convoy; any other index is its `eq` then `order` columns.
    pub fn index_key(&self, index: &Index) -> Vec<String> {
        let mut key = Vec::new();
        if index.unique {
            let table = self.tables.iter().find(|table| table.name == index.table);
            debug_assert!(table.is_some(), "`validate` refuses an index on table `{}`, which this set does not hold", index.table);
            if let Some(table) = table {
                key.push(self.parent_key_column().to_owned());
                key.extend(table.parent_key().iter().map(|column| column.name.clone()));
            }
        }
        key.extend(index.columns().map(str::to_owned));
        key
    }

    /// One index per reference column of a child table, named `<column>_ref` (partial on the
    /// column being present when it is nullable), so the foreign-key check of removing an identity
    /// (an expunge) searches each child table rather than scanning it (#3392 lane 3 ruling 1).
    /// A child table's rows are keyed by their owner first, and nothing else indexes the column.
    /// A column a declared index already leads with is skipped. Root reference columns are left to
    /// declared indexes, as the root is keyed by the resource itself.
    pub fn reference_indexes(&self) -> Vec<Index> {
        let mut out = Vec::new();
        for table in self.tables.iter().filter(|table| table.parent.is_some()) {
            for column in table.key.iter().chain(&table.columns).filter(|column| column.ty == ColumnType::Ref) {
                let declared = self.indexes.iter().any(|index| {
                    index.table == table.name
                        && index.columns().next() == Some(column.name.as_str())
                        && index
                            .predicate
                            .iter()
                            .all(|condition| matches!(condition, Condition::NotNull { column: name } if *name == column.name))
                });
                if declared {
                    continue;
                }
                let predicate = if column.nullable { vec![Condition::NotNull { column: column.name.clone() }] } else { Vec::new() };
                out.push(Index {
                    name: format!("{}_ref", column.name),
                    table: table.name.clone(),
                    eq: vec![column.name.clone()],
                    order: Vec::new(),
                    predicate,
                    unique: false,
                });
            }
        }
        out
    }
}

impl TableSet {
    /// The shared event table as seen for this kind. Structural roles are declared here,
    /// alongside the columns, rather than inferred by a viewer from their names.
    /// This describes the existing log; it does not add a per-kind stored table.
    pub fn log_table(&self) -> Table {
        let mut layout = crate::Layout::description("Log");
        layout.key(|layout| {
            layout.column("home", ColumnType::Interned(crate::HOME_DOMAIN), true);
            layout.column("home_seq", ColumnType::Integer, true);
            layout.role("sequence");
        });
        layout.column("resource", ColumnType::Ref, false);
        layout.reference(&self.tables[0].name);
        layout.column("prev_seq", ColumnType::Integer, true);
        layout.role("sequence");
        layout.categories(
            "cause",
            [("create", "Create"), ("edit", "Edit"), ("edge", "Edge"), ("delete", "Delete"), ("expunge", "Expunge")]
                .map(|(value, name)| (value.into(), name.into()))
                .into(),
            true,
        );
        layout.categories(
            "edge",
            self.edges.iter().zip(&self.edge_names).map(|(code, name)| (code.to_string(), name.clone())).collect(),
            false,
        );
        let labels = self.edge_state().map(|(field, _)| self.tables[0].columns[field.columns.start].categories.clone()).unwrap_or_default();
        layout.categories("from", labels.clone(), false);
        layout.role("transition-from");
        layout.categories("to", labels, false);
        layout.role("transition-to");
        layout.column("about", ColumnType::Ref, false);
        layout.column("writer", ColumnType::Interned(crate::WRITER_DOMAIN), true);
        layout.column("at", ColumnType::Timestamp, true);
        layout.column("payload", ColumnType::Cbor, false);
        layout.column("effect", ColumnType::Cbor, true);
        layout.column("topic", ColumnType::Text, true);
        layout.finish().tables.remove(0)
    }
}
