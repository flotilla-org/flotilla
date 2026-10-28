use flotilla_schema::{ColumnType, Describe, Element, KindCode, Layout, Stored};

#[derive(Stored, Describe, serde::Serialize)]
enum Phase {
    #[shape(code = "a")]
    #[serde(rename = "a")]
    Active,
    #[shape(code = "z")]
    #[serde(rename = "z")]
    Finished,
}

#[derive(Stored, Describe)]
struct Part {
    name: String,
    count: Option<u32>,
}

#[derive(Stored, Describe, serde::Serialize)]
enum Detail {
    #[shape(code = "a")]
    #[serde(rename = "a")]
    First { value: u32, note: Option<String> },
    #[shape(code = "z")]
    #[serde(rename = "z")]
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

#[derive(Describe)]
#[allow(dead_code)] // Description-only fixtures deliberately have no value codec.
struct AllOptional {
    count: Option<u32>,
    label: Option<String>,
}

#[derive(Describe)]
#[allow(dead_code)]
struct OptionalQuery {
    part: Option<AllOptional>,
}

// Review #3592: describe-only optional parts may be present-but-empty; there is no codec
// needing a presence discriminator, and all their flattened columns remain nullable.
#[test]
fn description_all_optional_parts_need_no_presence_guard() {
    let set = OptionalQuery::table_set();
    assert_eq!(set.mode, flotilla_schema::SchemaMode::DescribeOnly);
    assert!(set.tables[0].guards.is_empty());
    assert_eq!(
        set.tables[0].columns.iter().map(|column| (column.name.as_str(), column.nullable)).collect::<Vec<_>>(),
        [("part_count", true), ("part_label", true)]
    );
}

// Review #3592: changing incidental generated-column metadata cannot select description mode
// or bypass storage-only reserved-column invariants.
#[test]
fn storage_mode_does_not_depend_on_generated_columns() {
    let mut layout = Layout::root("Stored", KindCode::new("st"));
    layout.column("id", ColumnType::Text, true);
    let mut set = layout.finish();
    set.tables[0].generated.clear();
    assert!(matches!(set.validate(), Err(flotilla_schema::DescriptorError::ReservedColumn { column, .. }) if column == "id"));
}

#[derive(Stored)]
#[store(kind = "ShapeKind")]
#[shape(code = "sk")]
struct ShapeKind {
    value: String,
}

// Review #3592: kind identity is a storage declaration, but its stable code may use the shape
// namespace. Both namespaces are parsed separately into the one stored descriptor.
#[test]
fn stored_types_accept_the_shape_code_namespace() {
    use flotilla_schema::Kind;
    assert_eq!(ShapeKind::table_set().code, KindCode::new("sk"));
}

#[derive(Describe)]
#[allow(dead_code)]
struct MapRows {
    #[shape(child)]
    ordered: std::collections::BTreeMap<usize, bool>,
    #[shape(child)]
    hashed: std::collections::HashMap<usize, bool>,
    #[shape(child)]
    nested: std::collections::BTreeMap<String, std::collections::BTreeMap<usize, bool>>,
}

// Review #3592: supported scalars compose in child maps; unnamed nested map values get the
// documented `items` table name, consistently carrying their parent's key.
#[test]
fn scalar_map_keys_and_values_compose_in_child_tables() {
    let set = MapRows::table_set();
    for table in &set.tables[1..3] {
        assert_eq!(table.key[0].ty, ColumnType::Integer);
        assert_eq!(table.columns[0].ty, ColumnType::Bool);
    }
    let nested = set.tables.iter().find(|table| table.name == "map_rows_nested_items").unwrap();
    assert_eq!(nested.key.iter().map(|column| column.name.as_str()).collect::<Vec<_>>(), ["nested_key", "key"]);
}

#[derive(Describe)]
#[allow(dead_code)]
struct Source {
    service: String,
    #[shape(flatten)]
    host: Option<String>,
}

#[derive(Describe)]
#[allow(dead_code)]
struct Keyed {
    #[shape(key)]
    source: Source,
    #[shape(key)]
    id: u32,
    #[shape(child)]
    labels: Vec<String>,
    #[shape(child)]
    groups: std::collections::BTreeMap<String, Vec<u32>>,
}

#[derive(Describe)]
#[allow(dead_code)]
struct Unkeyed {
    id: u32,
    #[shape(child)]
    labels: Vec<String>,
}

#[test]
fn described_child_tables_carry_their_root_key() {
    let set = Keyed::table_set();
    set.validate().unwrap();
    let names = |table: &str| {
        let table = set.tables.iter().find(|candidate| candidate.name == table).unwrap();
        table.key.iter().map(|column| (column.name.as_str(), column.nullable)).collect::<Vec<_>>()
    };
    assert_eq!(names("keyed"), [("source_service", false), ("source_host", true), ("id", false)]);
    assert!(set.tables[0].columns.is_empty());
    let inherited = [("keyed_source_service", false), ("keyed_source_host", true), ("keyed_id", false)];
    assert_eq!(names("keyed_labels"), [&inherited[..], &[("pos", false)]].concat());
    assert_eq!(names("keyed_groups"), [&inherited[..], &[("key", false)]].concat());
    assert_eq!(names("keyed_groups_items"), [&inherited[..], &[("groups_key", false), ("pos", false)]].concat());
    assert!(matches!(
        Unkeyed::table_set().validate(),
        Err(flotilla_schema::DescriptorError::UnkeyedParent { table, .. }) if table == "unkeyed_labels"
    ));
}
