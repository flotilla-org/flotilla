use flotilla_schema::{snake_case, DatumColumns, Layout};
use flotilla_schema::{Code, ColumnDef, ColumnType, Guard, KindCode, TableRole};
use flotilla_schema_sqlite::GuardSql;

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
    assert!(
        matches!(layout.finish().validate(), Err(flotilla_schema::DescriptorError::DuplicateColumn { column, .. }) if column == "value")
    );
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
    layout.column("tag", ColumnType::Code(vec![a.to_string(), b.to_string()]), true);
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
    assert_eq!(deliverables.columns, [ColumnDef::new("title", ColumnType::Text, false)]);
    let evidence = &set.tables[2];
    let key: Vec<&str> = evidence.key.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(key, ["deliverables_pos", "pos"]);
    assert_eq!(evidence.own_key, 1);
    assert_eq!(evidence.parent_key().len(), 1);
    assert_eq!(evidence.columns[0].name, "value");
    assert!(set.tables[0].guards.is_empty());
}
