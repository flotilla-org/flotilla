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
    descriptor::{ColumnDef, ColumnType, Expansion, FieldColumns, Guard, Index, Table, TableRole, TableSet, When},
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
            guards: Vec::new(),
            enums: Vec::new(),
            fields: Vec::new(),
        };
        Layout {
            set: TableSet { kind: kind.to_owned(), code, tables: vec![root], indexes: Vec::new() },
            frames: vec![Frame { table: 0, scopes: Vec::new(), key_mode: false, field_depth: 0 }],
        }
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
        let frame = self.frames.last_mut().expect("layout has a current table");
        let table = &mut self.set.tables[frame.table];
        if frame.key_mode {
            table.key.push(ColumnDef { name: name.to_owned(), ty, nullable: false });
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
        table.columns.push(ColumnDef { name: name.to_owned(), ty, nullable });
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
    /// `<parent field>_<column>`.
    pub fn table(&mut self, field: &str, role: TableRole, f: impl FnOnce(&mut Layout)) {
        assert!(role != TableRole::Root, "only the kind declares the root table");
        let parent_index = self.frames.last().expect("layout has a current table").table;
        let parent = &self.set.tables[parent_index];
        let inherited = parent.key.len() - parent.own_key;
        let key = parent
            .key
            .iter()
            .enumerate()
            .map(|(position, column)| {
                let name = if position < inherited { column.name.clone() } else { Layout::join(&parent.field, &column.name) };
                ColumnDef { name, ty: column.ty.clone(), nullable: false }
            })
            .collect();
        let table = Table {
            name: Layout::join(&parent.name, field),
            role,
            parent: Some(parent_index),
            field: field.to_owned(),
            key,
            own_key: 0,
            columns: Vec::new(),
            guards: Vec::new(),
            enums: Vec::new(),
            fields: Vec::new(),
        };
        self.set.tables.push(table);
        self.frames.push(Frame { table: self.set.tables.len() - 1, scopes: Vec::new(), key_mode: false, field_depth: 0 });
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
            self.set.tables[table].fields.push(FieldColumns { name: name.to_owned(), columns: start..end, state, datum_of });
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
    pub fn tag_guard(&mut self, tag: &str, code: Code, datums: &[DatumColumns], carried: &[(usize, bool)]) {
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
        let guard = Guard { column: tag.to_owned(), when: When::Is(code), required, absent, otherwise: Vec::new() };
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

#[cfg(test)]
mod tests {
    use super::{snake_case, DatumColumns, Layout};
    use crate::{
        code::Code,
        descriptor::{ColumnDef, ColumnType, Guard, TableRole},
        value::KindCode,
    };

    #[test]
    fn kind_names_become_snake_case_tables() {
        assert_eq!(snake_case("Message"), "message");
        assert_eq!(snake_case("TerminalSession"), "terminal_session");
        assert_eq!(snake_case("HTTPRoute"), "http_route");
        assert_eq!(snake_case("ResolvedHTTP"), "resolved_http");
        assert_eq!(snake_case("Ipv4Address"), "ipv4_address");
        assert_eq!(snake_case("snake_already"), "snake_already");
        assert_eq!(Layout::root("TerminalSession", KindCode::new("ts")).finish().tables[0].name, "terminal_session");
    }

    // Behaviour: an element's unnamed column is `value`, so a field named `value` beside it in
    // the same table is a duplicate the descriptor refuses rather than a silent collision.
    #[test]
    fn a_field_named_like_the_element_column_is_refused() {
        let mut layout = Layout::root("Note", KindCode::new("no"));
        layout.table("items", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("", ColumnType::Text, true);
            layout.column("value", ColumnType::Text, true);
        });
        assert!(matches!(layout.finish().validate(), Err(crate::DescriptorError::DuplicateColumn { column, .. }) if column == "value"));
    }

    // Behaviour (#3392 G5): an optional value is present when its first required column is
    // non-null; a value of several columns gets one presence CHECK, a single column none. Inside a
    // scope a required column is nullable, and closing a scope adds its columns (not as required)
    // to the enclosing one, so an absent outer value nulls an inner value's columns too.
    #[test]
    fn optional_values_are_present_by_their_first_required_column() {
        let mut layout = Layout::root("Edge", KindCode::new("ed"));
        layout.optional(|layout| {
            layout.column("hold_note", ColumnType::Text, false);
            layout.column("hold_by", ColumnType::Text, true);
            layout.optional(|layout| layout.column("hold_until", ColumnType::Timestamp, true));
            layout.column("hold_at", ColumnType::Timestamp, true);
        });
        layout.optional(|layout| layout.column("watermark", ColumnType::Timestamp, true));
        let set = layout.finish();
        let root = &set.tables[0];
        assert!(root.columns.iter().all(|column| column.nullable));
        let checks: Vec<String> = root.guards.iter().map(Guard::check).collect();
        assert_eq!(
            checks,
            ["CASE WHEN \"hold_by\" IS NOT NULL THEN \"hold_at\" IS NOT NULL \
              ELSE \"hold_note\" IS NULL AND \"hold_until\" IS NULL AND \"hold_at\" IS NULL END"]
        );
    }

    #[test]
    #[should_panic(expected = "needs a required column")]
    fn an_optional_value_without_a_required_column_is_refused() {
        let mut layout = Layout::root("Edge", KindCode::new("ed"));
        layout.optional(|layout| layout.column("note", ColumnType::Text, false));
    }

    // Behaviour (#3392 G5, G6): one CHECK per code of a tag column requires the data the code
    // carries as `T` and nulls the data it lacks; data it carries as `Option<T>` is unconstrained,
    // and a code constraining nothing emits no CHECK.
    #[test]
    fn tag_guards_require_carried_data_and_null_the_rest() {
        let mut layout = Layout::root("Edge", KindCode::new("ed"));
        let (a, b) = (Code::new("a"), Code::new("b"));
        layout.column("tag", ColumnType::Code(vec![a, b]), true);
        let datums = [
            layout.scope(|layout| layout.column("tag_by", ColumnType::Text, true)),
            layout.scope(|layout| layout.optional(|layout| layout.column("tag_at", ColumnType::Timestamp, true))),
        ];
        assert_eq!(datums[1], DatumColumns { columns: vec!["tag_at".into()], required: vec![] });
        let at_required = Layout::required_of(|layout| layout.column("tag_at", ColumnType::Timestamp, true));
        let datums = [datums[0].clone(), DatumColumns { required: at_required, ..datums[1].clone() }];
        layout.tag_guard("tag", a, &datums, &[(0, true), (1, true)]);
        layout.tag_guard("tag", b, &datums, &[(1, false)]);
        layout.tag_guard("tag", Code::new("c"), &datums, &[(0, false), (1, false)]);
        let checks: Vec<String> = layout.finish().tables[0].guards.iter().map(Guard::check).collect();
        assert_eq!(
            checks,
            [
                "CASE WHEN \"tag\" = 'a' THEN \"tag_by\" IS NOT NULL AND \"tag_at\" IS NOT NULL END",
                "CASE WHEN \"tag\" = 'b' THEN \"tag_by\" IS NULL END",
            ]
        );
    }

    // Behaviour (DESIGN naming): a child table is `<parent>_<field>`; its key is the ancestors' own
    // keys renamed `<ancestor field>_<column>` followed by its own key. Scopes stay with their table.
    #[test]
    fn nested_tables_inherit_renamed_keys() {
        let mut layout = Layout::root("Convoy", KindCode::new("cv"));
        layout.column("state", ColumnType::Integer, true);
        layout.scope(|layout| {
            layout.table("deliverables", TableRole::Records, |layout| {
                layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                layout.column("title", ColumnType::Text, true);
                layout.table("evidence", TableRole::History, |layout| {
                    layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
                    layout.column("", ColumnType::Text, true);
                });
            });
        });
        let set = layout.finish();
        let names: Vec<&str> = set.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["convoy", "convoy_deliverables", "convoy_deliverables_evidence"]);
        let deliverables = &set.tables[1];
        assert_eq!(deliverables.parent, Some(0));
        assert_eq!(deliverables.columns, [ColumnDef { name: "title".into(), ty: ColumnType::Text, nullable: false }]);
        let evidence = &set.tables[2];
        let key: Vec<&str> = evidence.key.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(key, ["deliverables_pos", "pos"]);
        assert_eq!(evidence.own_key, 1);
        assert_eq!(evidence.parent_key().len(), 1);
        assert_eq!(evidence.columns[0].name, "value");
        assert!(set.tables[0].guards.is_empty());
    }
}
