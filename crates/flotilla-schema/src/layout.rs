//! Builds a kind's [`TableSet`] from the derive's `layout` calls.
//!
//! Generated code walks a type's fields in declaration order and declares columns, scopes and
//! nested tables here. Scopes are how nullable data gets its CHECKs (#3392 rulings G5 and G6):
//!
//! - an optional value of several columns ([`Layout::optional`]) is present when its first
//!   required column is non-null, and closing it emits one presence CHECK;
//! - a datum shared by name across a state's labels or a data enum's variants ([`Layout::scope`],
//!   [`Layout::datum`]) collects its columns, and one CHECK per label or variant
//!   ([`Layout::tag_guard`]) requires the data that code carries and nulls the data it lacks.
//!
//! Closing a scope adds its columns to the enclosing scope, so a value that holds another value's
//! columns nulls them all when it is absent.

use crate::{
    code::Code,
    descriptor::{ColumnDef, ColumnType, Expansion, FieldColumns, Guard, Index, SchemaMode, Table, TableRole, TableSet, When},
    value::KindCode,
};

pub struct Layout {
    set: TableSet,
    frames: Vec<Frame>,
}

/// One table being declared; its scope stack stays with it, so a nested table opened inside a
/// scope starts outside any.
struct Frame {
    table: usize,
    scopes: Vec<Scope>,
    key_mode: bool,
    /// Fields being declared: only a row's own fields (depth 0) are recorded, not the fields of a
    /// struct flattened into one of them.
    field_depth: usize,
    /// Open alias scopes, outermost first: `(current, old)` names of a renamed field, prefixed.
    aliases: Vec<(String, String)>,
}

impl Frame {
    fn new(table: usize) -> Frame {
        Frame { table, scopes: Vec::new(), key_mode: false, field_depth: 0, aliases: Vec::new() }
    }

    /// The earlier names of `name` under the open alias scopes: each scope whose current name is
    /// `name` or a `_`-joined prefix of it gives its old name in its place, innermost first, so
    /// nested renames compose.
    fn old_names(&self, name: &str) -> Vec<String> {
        let mut names = vec![name.to_owned()];
        for (current, old) in self.aliases.iter().rev() {
            let renamed: Vec<String> = names
                .iter()
                .filter_map(|name| match name.strip_prefix(current.as_str()) {
                    Some("") => Some(old.clone()),
                    Some(rest) if rest.starts_with('_') => Some(format!("{old}{rest}")),
                    _ => None,
                })
                .collect();
            names.extend(renamed);
        }
        names.remove(0);
        names
    }
}

/// The columns declared inside one scope, and those of them its value requires when present.
#[derive(Default)]
struct Scope {
    columns: Vec<String>,
    required: Vec<String>,
}

impl Layout {
    /// Starts a kind: the root table is named by the snake_case kind name.
    pub fn root(kind: &str, code: KindCode) -> Layout {
        let root = Table {
            name: snake_case(kind),
            role: TableRole::Root,
            parent: None,
            field: String::new(),
            key: Vec::new(),
            own_key: 0,
            columns: Vec::new(),
            generated: Vec::new(),
            guards: Vec::new(),
            enums: Vec::new(),
            fields: Vec::new(),
            field_aliases: Vec::new(),
            aliases: Vec::new(),
        };
        let mut id = ColumnDef::new("id", ColumnType::Ref, false);
        id.reference = Some("resource_identity".into());
        id.add_role("key");
        id.add_role("ref");
        let mut version = ColumnDef::new("version", ColumnType::Integer, false);
        version.add_role("sequence");
        let root = Table { generated: vec![id, version], ..root };
        Layout {
            set: TableSet {
                mode: SchemaMode::Stored,
                kind: kind.to_owned(),
                code,
                tables: vec![root],
                indexes: Vec::new(),
                edges: Vec::new(),
                edge_names: Vec::new(),
            },
            frames: vec![Frame::new(0)],
        }
    }

    /// Starts a table description without any columns or identity supplied by a store.
    pub fn description(name: &str) -> Layout {
        let mut layout = Self::root(name, KindCode::new("query"));
        layout.set.mode = SchemaMode::DescribeOnly;
        layout.set.tables[0].generated.clear();
        layout
    }

    /// Joins a column prefix and a field name with `_`; either may be empty.
    pub fn join(prefix: &str, name: &str) -> String {
        match (prefix.is_empty(), name.is_empty()) {
            (true, _) => name.to_owned(),
            (_, true) => prefix.to_owned(),
            _ => format!("{prefix}_{name}"),
        }
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("layout has a current table")
    }

    /// Declares a column of the current table. An empty name is a scalar element's `value`.
    /// Outside any scope a required column is `NOT NULL`; inside one it is nullable, and the
    /// scope's CHECKs require it where its value is present.
    pub fn column(&mut self, name: &str, ty: ColumnType, required: bool) {
        let name = if name.is_empty() { "value" } else { name };
        let described = self.set.mode == SchemaMode::DescribeOnly;
        let frame = self.frames.last_mut().expect("layout has a current table");
        let table = &mut self.set.tables[frame.table];
        table.aliases.extend(frame.old_names(name).into_iter().map(|old| (old, name.to_owned())));
        if frame.key_mode {
            // A described row key may hold an optional part, such as a reference's host; joins on
            // such a key must treat null as equal to null.
            let nullable = described && (!required || !frame.scopes.is_empty());
            let mut column = ColumnDef::new(name, ty, nullable);
            column.add_role("key");
            if column.ty == ColumnType::Ref {
                column.add_role("ref");
                column.reference = Some("resource_identity".into());
            }
            if column.ty == ColumnType::Timestamp {
                column.add_role("time");
            }
            table.key.push(column);
            table.own_key += 1;
            return;
        }
        let nullable = match frame.scopes.last_mut() {
            Some(scope) => {
                scope.columns.push(name.to_owned());
                if required {
                    scope.required.push(name.to_owned());
                }
                true
            }
            None => !required,
        };
        let mut column = ColumnDef::new(name, ty, nullable);
        match column.ty {
            ColumnType::Ref => {
                column.add_role("ref");
                column.reference = Some("resource_identity".into());
            }
            ColumnType::Timestamp => column.add_role("time"),
            _ => {}
        }
        table.columns.push(column);
    }

    /// Declares a code column: `codes` and the variants' Rust `names`, in the same order, become its
    /// CHECK and its rows of the `codes` table.
    pub fn code(&mut self, name: &str, codes: &[Code], names: &[&str], required: bool) {
        debug_assert_eq!(codes.len(), names.len(), "one name per code");
        self.categories(name, codes.iter().zip(names).map(|(code, name)| (code.to_string(), (*name).to_owned())).collect(), required);
    }

    /// Declares an enum, including non-storage sources whose values are not short storage codes.
    pub fn categories(&mut self, name: &str, categories: Vec<(String, String)>, required: bool) {
        self.column(name, ColumnType::Code(categories.iter().map(|(code, _)| code.clone()).collect()), required);
        self.last_column().categories = categories;
    }

    fn last_column(&mut self) -> &mut ColumnDef {
        let frame = self.frames.last().expect("current table");
        let table = &mut self.set.tables[frame.table];
        if frame.key_mode { table.key.last_mut() } else { table.columns.last_mut() }.expect("column declared")
    }

    /// Adds an open semantic field property to the column just declared.
    pub fn property(&mut self, name: &str, value: serde_json::Value) {
        self.last_column().properties.insert(name.into(), value);
    }

    /// Adds meaning at the point that generates a column, never by matching its name.
    pub fn role(&mut self, role: &str) {
        self.last_column().add_role(role);
    }

    pub fn reference(&mut self, table: &str) {
        self.last_column().reference = Some(table.into());
    }

    /// Marks the column just declared as holding canonical ADR 0066 addresses (wire type `address`).
    pub fn address(&mut self) {
        self.last_column().address = true;
    }

    /// Declares the current table's own key: columns declared inside `f` become key columns.
    pub fn key(&mut self, f: impl FnOnce(&mut Layout)) {
        let frame = self.frame();
        assert!(!frame.key_mode, "key declarations do not nest");
        assert!(frame.scopes.is_empty(), "a key cannot be declared inside a scope");
        frame.key_mode = true;
        f(self);
        self.frame().key_mode = false;
    }

    /// Runs `f` in a new scope and closes it, adding its columns to the enclosing scope (not as
    /// required: the enclosing value decides whether they hold).
    fn scoped(&mut self, f: impl FnOnce(&mut Layout)) -> Scope {
        self.frame().scopes.push(Scope::default());
        f(self);
        let frame = self.frame();
        let scope = frame.scopes.pop().expect("scope pushed above");
        if let Some(outer) = frame.scopes.last_mut() {
            outer.columns.extend(scope.columns.iter().cloned());
        }
        scope
    }

    /// Declares an optional value: `f` declares the value's columns as if required, and they are
    /// nullable. The value is present when its first required column is non-null (#3392 ruling
    /// G5: no presence flag); a value of several columns gets one CHECK requiring its other
    /// required columns while present and nulling all of them while absent. Nested tables declared
    /// inside are not checked: the codec writes them no rows while the value is absent.
    ///
    /// # Panics
    ///
    /// When `f` declares no required column directly, presence would be undecidable: an optional
    /// value needs a required column of its own.
    pub fn optional(&mut self, f: impl FnOnce(&mut Layout)) {
        let scope = self.scoped(f);
        if scope.columns.is_empty() {
            return;
        }
        // A query can contain an optional struct with only optional fields. Description
        // has no codec needing to distinguish absent from present-but-empty in that case.
        if scope.required.is_empty() && self.set.mode == SchemaMode::DescribeOnly {
            return;
        }
        let first = scope.required.first().cloned().unwrap_or_else(|| {
            panic!("an optional value needs a required column to mark its presence; its columns are {:?}", scope.columns)
        });
        if scope.columns.len() > 1 {
            let others = |names: &[String]| names.iter().filter(|name| **name != first).cloned().collect::<Vec<_>>();
            let guard = Guard {
                column: first.clone(),
                when: When::Present,
                required: others(&scope.required),
                absent: Vec::new(),
                otherwise: others(&scope.columns),
            };
            let table = self.frame().table;
            self.set.tables[table].guards.push(guard);
        }
    }

    /// Declares a nested table storing `field` of the current table; `f` declares its own key and
    /// its columns. Its inherited key is the parent's key, with the parent's own key columns renamed
    /// `<parent field>_<column>`, or `<root table>_<column>` under a described root.
    pub fn table(&mut self, field: &str, role: TableRole, f: impl FnOnce(&mut Layout)) {
        assert!(role != TableRole::Root, "only the kind declares the root table");
        let frame = self.frames.last().expect("layout has a current table");
        let (parent_index, field_aliases) = (frame.table, frame.old_names(field));
        let parent = &self.set.tables[parent_index];
        let inherited = parent.key.len() - parent.own_key;
        let prefix = if parent.parent.is_none() { &parent.name } else { &parent.field };
        let key = parent
            .key
            .iter()
            .enumerate()
            .map(|(position, column)| {
                let name = if position < inherited { column.name.clone() } else { Layout::join(prefix, &column.name) };
                ColumnDef { name, ..column.clone() }
            })
            .collect();
        let root = self.set.parent_key_column().to_owned();
        let mut owner = ColumnDef::new(&root, ColumnType::Ref, false);
        owner.reference = Some(root);
        owner.add_role("key");
        owner.add_role("ref");
        let table = Table {
            name: Layout::join(&parent.name, field),
            role,
            parent: Some(parent_index),
            field: field.to_owned(),
            key,
            own_key: 0,
            columns: Vec::new(),
            generated: if self.set.mode == SchemaMode::DescribeOnly { Vec::new() } else { vec![owner] },
            guards: Vec::new(),
            enums: Vec::new(),
            fields: Vec::new(),
            field_aliases,
            aliases: Vec::new(),
        };
        self.set.tables.push(table);
        self.frames.push(Frame::new(self.set.tables.len() - 1));
        f(self);
        let frame = self.frames.pop().expect("table frame pushed above");
        assert!(frame.scopes.is_empty() && !frame.key_mode, "scopes and keys close inside their table");
    }

    /// Declares one column-shaped field of the current row: the columns `f` declares are recorded
    /// as the field `name` when the row is the current table's own (not a struct flattened into a
    /// field). Generated `layout_row` code wraps each column-shaped field of a struct in this,
    /// except a `state` field, whose `State` records its own label and data fields.
    pub fn field(&mut self, name: &str, state: bool, f: impl FnOnce(&mut Layout)) {
        self.record_field(name, state, None, f);
    }

    fn record_field(&mut self, name: &str, state: bool, datum_of: Option<&str>, f: impl FnOnce(&mut Layout)) {
        let frame = self.frame();
        let (table, record) = (frame.table, frame.field_depth == 0 && !frame.key_mode);
        frame.field_depth += 1;
        let start = self.set.tables[table].columns.len();
        f(self);
        self.frame().field_depth -= 1;
        let end = self.set.tables[table].columns.len();
        // A datum holding a table declares no columns here, and is not a field of the row.
        if record && (datum_of.is_none() || end > start) {
            let datum_of = datum_of.map(str::to_owned);
            let aliases = self.frames.last().expect("layout has a current table").old_names(name);
            self.set.tables[table].fields.push(FieldColumns { name: name.to_owned(), columns: start..end, state, datum_of, aliases });
        }
    }

    /// Declares one datum of a data enum, shared by name across the variants carrying it (mapping
    /// B, #3392 ruling G6): the columns and tables `f` declares are nullable (a datum is null, or
    /// has no rows, under a variant that lacks it), and the returned [`DatumColumns`] say which
    /// columns the datum declares and which of them a variant carrying it requires, for
    /// [`Layout::tag_guard`].
    pub fn scope(&mut self, f: impl FnOnce(&mut Layout)) -> DatumColumns {
        let scope = self.scoped(f);
        DatumColumns { columns: scope.columns, required: scope.required }
    }

    /// Declares a renamed field (`#[store(alias = "old")]`): every column, field and nested table
    /// `f` declares under the current name `current` (prefixed) also answers to `old`, so log
    /// entries written under the old name still decode. An alias belongs to its table: the columns
    /// of a nested table restart their names and take no alias from an enclosing scope.
    pub fn alias(&mut self, current: &str, old: &str, f: impl FnOnce(&mut Layout)) {
        self.frame().aliases.push((current.to_owned(), old.to_owned()));
        f(self);
        self.frame().aliases.pop();
    }

    /// Declares the kind's edge codes (`#[store(edges = E)]`) and the variant names carrying them,
    /// in the same order.
    pub fn edges(&mut self, codes: &[Code], names: &[&str]) {
        debug_assert_eq!(codes.len(), names.len(), "one name per code");
        self.set.edges = codes.to_vec();
        self.set.edge_names = names.iter().map(|name| (*name).to_owned()).collect();
    }

    /// [`Layout::scope`] for one datum of a label-plus-data state whose label column is `label`. A
    /// datum with columns is also recorded as a state field of the row, `datum_of` the label; one
    /// holding a table is not a field (its rows change one at a time).
    pub fn datum(&mut self, name: &str, label: &str, f: impl FnOnce(&mut Layout)) -> DatumColumns {
        let mut columns = DatumColumns::default();
        self.record_field(name, true, Some(label), |layout| columns = layout.scope(f));
        columns
    }

    /// The columns `f` requires when laid out as a required value, without declaring anything: a
    /// datum declared `T` under one code and `Option<T>` under another (#3392 ruling G5) is laid
    /// out once, as the option, and its codes carrying `T` require these.
    pub fn required_of(f: impl FnOnce(&mut Layout)) -> Vec<String> {
        let mut scratch = Layout::root("Scratch", KindCode::new("scratch"));
        scratch.scope(f).required
    }

    /// Declares the CHECK of one code of a tag column `tag` (a state's label, or a data enum's tag):
    /// while `tag = code`, the required columns of every datum in `carried` whose flag is `true`
    /// (the code carries it as `T`, not `Option<T>`) are non-null and every column of the other
    /// datums is null. `datums` are the datums in declaration order and `carried` holds the indexes
    /// of those this code carries. A code constraining nothing emits no CHECK.
    pub fn tag_guard(&mut self, tag: &str, code: impl ToString, datums: &[DatumColumns], carried: &[(usize, bool)]) {
        let mut required = Vec::new();
        let mut absent = Vec::new();
        for (index, datum) in datums.iter().enumerate() {
            match carried.iter().find(|(carried, _)| *carried == index) {
                Some((_, true)) => required.extend(datum.required.iter().cloned()),
                Some((_, false)) => {}
                None => absent.extend(datum.columns.iter().cloned()),
            }
        }
        if required.is_empty() && absent.is_empty() {
            return;
        }
        let table = self.frame().table;
        let guard = Guard { column: tag.to_owned(), when: When::Is(code.to_string()), required, absent, otherwise: Vec::new() };
        self.set.tables[table].guards.push(guard);
    }

    /// Records that the data enum with tag column `tag` expanded into `datums`' columns, for the
    /// schema view.
    pub fn expansion(&mut self, tag: &str, datums: &[DatumColumns]) {
        let table = self.frame().table;
        let columns = datums.iter().flat_map(|datum| datum.columns.iter().cloned()).collect();
        self.set.tables[table].enums.push(Expansion { tag: tag.to_owned(), columns });
    }

    /// The name of the table being declared: where generated code places a declared index.
    pub fn table_name(&self) -> &str {
        &self.set.tables[self.frames.last().expect("layout has a current table").table].name
    }

    pub fn index(&mut self, index: Index) {
        self.set.indexes.push(index);
    }

    /// Declares index `name` on the table of the current table's `child` list `field`, keyed by
    /// every column of its element.
    pub fn element_index(&mut self, name: &str, field: &str) {
        let parent = self.frames.last().expect("layout has a current table").table;
        let table = self
            .set
            .tables
            .iter()
            .find(|table| table.parent == Some(parent) && table.field == field && table.role == TableRole::Child)
            .unwrap_or_else(|| panic!("`{field}` is no child list of table {}", self.set.tables[parent].name));
        let index = Index {
            name: name.to_owned(),
            table: table.name.clone(),
            eq: table.columns.iter().map(|column| column.name.clone()).collect(),
            order: Vec::new(),
            predicate: Vec::new(),
            unique: false,
        };
        self.set.indexes.push(index);
    }

    pub fn finish(self) -> TableSet {
        assert_eq!(self.frames.len(), 1, "every nested table closes before the layout finishes");
        self.set
    }
}

/// The columns one datum declares, and those of them a code carrying it requires; see
/// [`Layout::scope`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatumColumns {
    pub columns: Vec<String>,
    pub required: Vec<String>,
}

/// `TerminalSession` becomes `terminal_session`.
pub fn snake_case(name: &str) -> String {
    // A table name is fixed once stored, so acronyms keep one word: `HTTPRoute` is
    // `http_route`, not `h_t_t_p_route`. An uppercase letter starts a word after a lowercase
    // letter or digit, or when it ends an acronym that a lowercase letter follows.
    let characters: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (index, &character) in characters.iter().enumerate() {
        if character.is_ascii_uppercase() {
            let previous = index.checked_sub(1).map(|index| characters[index]);
            let next = characters.get(index + 1);
            let after_word = previous.is_some_and(|previous| previous.is_ascii_lowercase() || previous.is_ascii_digit());
            let ends_acronym = previous.is_some_and(|previous| previous.is_ascii_uppercase()) && next.is_some_and(char::is_ascii_lowercase);
            if after_word || ends_acronym {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}
