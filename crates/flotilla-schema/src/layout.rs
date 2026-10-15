//! Builds a kind's [`TableSet`] from the derive's `layout` calls.
//!
//! Generated code walks a type's fields in declaration order and declares columns, guards and
//! nested tables here. Guards are how sparse columns get their CHECKs: a data enum variant or an
//! `Option<flatten>` opens one, and closing it emits exactly one CHECK covering the columns
//! declared directly inside it.

use crate::{
    descriptor::{ColumnDef, ColumnType, FieldColumns, Guard, Index, Table, TableRole, TableSet},
    value::KindCode,
};

pub struct Layout {
    set: TableSet,
    frames: Vec<Frame>,
}

/// One table being declared; its guard stack stays with it, so a nested table opened inside a
/// guard starts unguarded.
struct Frame {
    table: usize,
    guards: Vec<Guard>,
    key_mode: bool,
    /// Fields being declared: only a row's own fields (depth 0) are recorded, not the fields of a
    /// struct flattened into one of them.
    field_depth: usize,
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
            fields: Vec::new(),
        };
        Layout {
            set: TableSet { kind: kind.to_owned(), code, tables: vec![root], indexes: Vec::new() },
            frames: vec![Frame { table: 0, guards: Vec::new(), key_mode: false, field_depth: 0 }],
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

    /// Declares a column of the current table. An empty name is a scalar element's `value`.
    /// Outside any guard a required column is `NOT NULL`; inside one it is nullable and the
    /// innermost guard's CHECK requires it while the guard holds.
    pub fn column(&mut self, name: &str, ty: ColumnType, required: bool) {
        let name = if name.is_empty() { "value" } else { name };
        let frame = self.frames.last_mut().expect("layout has a current table");
        let table = &mut self.set.tables[frame.table];
        if frame.key_mode {
            table.key.push(ColumnDef { name: name.to_owned(), ty, nullable: false });
            table.own_key += 1;
            return;
        }
        let nullable = match frame.guards.last_mut() {
            Some(guard) => {
                guard.scope.push(name.to_owned());
                if required {
                    guard.required.push(name.to_owned());
                }
                true
            }
            None => !required,
        };
        table.columns.push(ColumnDef { name: name.to_owned(), ty, nullable });
    }

    /// Declares the current table's own key: columns declared inside `f` become key columns.
    pub fn key(&mut self, f: impl FnOnce(&mut Layout)) {
        let frame = self.frames.last_mut().expect("layout has a current table");
        assert!(!frame.key_mode, "key declarations do not nest");
        assert!(frame.guards.is_empty(), "a key cannot be declared inside a guard");
        frame.key_mode = true;
        f(self);
        self.frames.last_mut().expect("layout has a current table").key_mode = false;
    }

    /// Declares columns that hold only while `column = value`. The controlling column must already
    /// be declared in the enclosing scope. Nested tables declared inside are not guarded by a
    /// CHECK: the codec writes them no rows while the guard does not hold, and refuses rows there.
    pub fn guard(&mut self, column: &str, value: i64, f: impl FnOnce(&mut Layout)) {
        let frame = self.frames.last_mut().expect("layout has a current table");
        frame.guards.push(Guard { column: column.to_owned(), value, required: Vec::new(), scope: Vec::new() });
        // The table lists guards in the order they open, so an outer CHECK precedes its nested ones.
        // The slot holds a placeholder until `f` returns and the finished guard replaces it; a
        // layout is only read after `finish`, so the placeholder is never observed.
        let slot = self.set.tables[frame.table].guards.len();
        self.set.tables[frame.table].guards.push(Guard { column: String::new(), value, required: Vec::new(), scope: Vec::new() });
        f(self);
        let frame = self.frames.last_mut().expect("layout has a current table");
        let guard = frame.guards.pop().expect("guard opened above");
        // A guard with no columns of its own (a variant holding only tables) would render an
        // always-true CHECK; nothing nested can have opened inside it, as a nested guard's
        // controlling column belongs to this scope.
        if guard.scope.is_empty() {
            self.set.tables[frame.table].guards.remove(slot);
        } else {
            self.set.tables[frame.table].guards[slot] = guard;
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
            fields: Vec::new(),
        };
        self.set.tables.push(table);
        self.frames.push(Frame { table: self.set.tables.len() - 1, guards: Vec::new(), key_mode: false, field_depth: 0 });
        f(self);
        let frame = self.frames.pop().expect("table frame pushed above");
        assert!(frame.guards.is_empty() && !frame.key_mode, "guards and keys close inside their table");
    }

    /// Declares one column-shaped field of the current row: the columns `f` declares are recorded
    /// as the field `name` when the row is the current table's own (not a struct flattened into a
    /// field). Generated `layout_row` code wraps each column-shaped field of a struct in this.
    pub fn field(&mut self, name: &str, state: bool, f: impl FnOnce(&mut Layout)) {
        let frame = self.frames.last_mut().expect("layout has a current table");
        let (table, record) = (frame.table, frame.field_depth == 0 && !frame.key_mode);
        frame.field_depth += 1;
        let start = self.set.tables[table].columns.len();
        f(self);
        self.frames.last_mut().expect("layout has a current table").field_depth -= 1;
        let end = self.set.tables[table].columns.len();
        if record {
            self.set.tables[table].fields.push(FieldColumns { name: name.to_owned(), columns: start..end, state });
        }
    }

    pub fn index(&mut self, index: Index) {
        self.set.indexes.push(index);
    }

    pub fn finish(self) -> TableSet {
        assert_eq!(self.frames.len(), 1, "every nested table closes before the layout finishes");
        self.set
    }
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
    use super::{snake_case, Layout};
    use crate::{
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
        assert_eq!(Layout::root("TerminalSession", KindCode(3)).finish().tables[0].name, "terminal_session");
    }

    // Behaviour: an element's unnamed column is `value`, so a field named `value` beside it in
    // the same table is a duplicate the descriptor refuses rather than a silent collision.
    #[test]
    fn a_field_named_like_the_element_column_is_refused() {
        let mut layout = Layout::root("Note", KindCode(1));
        layout.table("items", TableRole::Child, |layout| {
            layout.key(|layout| layout.column("pos", ColumnType::Integer, true));
            layout.column("", ColumnType::Text, true);
            layout.column("value", ColumnType::Text, true);
        });
        assert!(matches!(layout.finish().validate(), Err(crate::DescriptorError::DuplicateColumn { column, .. }) if column == "value"));
    }

    // Behaviour (DESIGN guards): a required column inside a guard is nullable and required by that
    // guard's CHECK; a nested guard's controlling column is in the outer scope, while the nested
    // guard's own columns are covered only by its own CHECK, which follows the outer one. Outside guards
    // required means NOT NULL. A guard without columns emits no CHECK.
    #[test]
    fn nested_guards_emit_one_check_each() {
        let mut layout = Layout::root("Edge", KindCode(1));
        layout.column("event", ColumnType::Code(vec![1, 2]), true);
        layout.guard("event", 1, |layout| {
            layout.column("event_drop_reason", ColumnType::Code(vec![1, 2]), true);
            layout.guard("event_drop_reason", 2, |layout| {
                layout.column("event_drop_reason_undeliverable_cause", ColumnType::Text, true);
                layout.column("event_drop_reason_undeliverable_note", ColumnType::Text, false);
            });
            layout.column("event_drop_at", ColumnType::Timestamp, false);
        });
        layout.guard("event", 2, |_| {});
        let set = layout.finish();
        let root = &set.tables[0];
        let nullable: Vec<(&str, bool)> = root.columns.iter().map(|c| (c.name.as_str(), c.nullable)).collect();
        assert_eq!(
            nullable,
            [
                ("event", false),
                ("event_drop_reason", true),
                ("event_drop_reason_undeliverable_cause", true),
                ("event_drop_reason_undeliverable_note", true),
                ("event_drop_at", true),
            ]
        );
        let checks: Vec<String> = root.guards.iter().map(Guard::check).collect();
        assert_eq!(
            checks,
            [
                "CASE WHEN \"event\" = 1 THEN \"event_drop_reason\" IS NOT NULL \
                 ELSE \"event_drop_reason\" IS NULL AND \"event_drop_at\" IS NULL END",
                "CASE WHEN \"event_drop_reason\" = 2 THEN \"event_drop_reason_undeliverable_cause\" IS NOT NULL \
                 ELSE \"event_drop_reason_undeliverable_cause\" IS NULL AND \"event_drop_reason_undeliverable_note\" IS NULL END",
            ]
        );
    }

    // Behaviour (DESIGN naming): a child table is `<parent>_<field>`; its key is the ancestors' own
    // keys renamed `<ancestor field>_<column>` followed by its own key. Guards stay with their table.
    #[test]
    fn nested_tables_inherit_renamed_keys() {
        let mut layout = Layout::root("Convoy", KindCode(2));
        layout.column("state", ColumnType::Integer, true);
        layout.guard("state", 1, |layout| {
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
        assert!(set.tables[0].guards.is_empty(), "a guard holding only a table emits no CHECK");
    }
}
