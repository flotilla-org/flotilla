use flotilla_schema::{ColumnType, Describe, Element, KindCode, Layout, Stored};

#[derive(Stored, Describe)]
enum Phase {
    #[shape(code = "a")]
    Active,
    #[shape(code = "z")]
    Finished,
}

#[derive(Stored, Describe)]
struct Part {
    name: String,
    count: Option<u32>,
}

#[derive(Stored, Describe)]
enum Detail {
    #[shape(code = "a")]
    First { value: u32, note: Option<String> },
    #[shape(code = "z")]
    Last { value: Option<u32> },
}

#[derive(Stored, Describe)]
struct Row {
    detail: Detail,
    #[shape(flatten)]
    part: Option<Part>,
    phase: Phase,
    #[shape(intern = "nm")]
    label: Option<String>,
    #[shape(child)]
    children: Vec<Part>,
}

// ADR 0067: shape attributes have one flattening for stored and describe-only structs.
// One constant shape covers required, optional, interned, enum and child fields; values
// cannot affect a descriptor, so generating row values would not add coverage.
#[test]
fn description_and_storage_share_column_and_child_shapes() {
    let described = Row::table_set();
    let mut layout = Layout::root("Row", KindCode::new("row"));
    Row::layout_row(&mut layout);
    let stored = layout.finish();
    assert_eq!(described.tables.len(), stored.tables.len());
    for (described, stored) in described.tables.iter().zip(&stored.tables) {
        assert_eq!(described.name, stored.name);
        assert_eq!(described.parent, stored.parent);
        assert_eq!(described.key, stored.key);
        assert_eq!(described.columns, stored.columns);
        assert_eq!(described.guards, stored.guards);
    }
    let root = &described.tables[0];
    assert_eq!(
        root.columns.iter().find(|column| column.name == "label").unwrap().ty,
        ColumnType::Interned(flotilla_schema::Code::new("nm"))
    );
}

// ADR 0067's open property slot is source-independent and can hold structured properties.
#[test]
fn a_non_derived_source_can_add_semantic_properties() {
    let mut layout = Layout::description("Measurements");
    layout.column("bytes", ColumnType::Integer, true);
    layout.property("unit", serde_json::json!("By"));
    layout.property("operators", serde_json::json!(["eq", "lt"]));
    let set = layout.finish();
    assert_eq!(set.tables[0].columns[0].properties["unit"], "By");
    assert_eq!(set.tables[0].columns[0].properties["operators"], serde_json::json!(["eq", "lt"]));
    assert!(set.tables[0].generated.is_empty());
}
