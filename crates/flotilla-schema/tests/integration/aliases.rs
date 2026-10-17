//! The store-level attributes of #3392 A7: `alias` on a renamed field, and `edges` on a kind.

use flotilla_schema::{Code, CodeEnum, DescriptorError, Kind, State, Stored};

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Person {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Note {
    #[store(alias = "text")]
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Phase {
    #[store(code = "op")]
    Open,
    #[store(code = "cl")]
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum PhaseEdge {
    #[store(code = "cl")]
    Close,
    #[store(code = "re")]
    Reopen,
}

/// Every field renamed, each keeping its earlier name.
#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Renamed", code = "rn", edges = PhaseEdge)]
struct Renamed {
    #[store(alias = "title")]
    heading: String,
    #[store(flatten, alias = "who")]
    by: Person,
    #[store(child, alias = "tags")]
    labels: Vec<String>,
    #[store(records, alias = "notes")]
    comments: flotilla_schema::Records<Note>,
    #[store(state, alias = "status")]
    phase: State<Phase>,
}

// Behaviour (#3392 A7): an alias gives a renamed field's earlier name to the field, to each of its
// columns (a flattened part's under the old prefix) and to its table, so a log entry written under
// the old names still names them; a field's own columns in a nested table keep their names, and
// an alias inside the nested row's struct is that table's own. The kind's `edges` are its edge
// enum's codes, checked against its one root state, whose label codes the descriptor gives.
#[test]
fn aliases_name_fields_columns_and_tables_by_their_earlier_names() {
    let set = Renamed::table_set();
    assert_eq!(set.validate(), Ok(()));
    let root = &set.tables[0];
    let aliases: Vec<(&str, &str)> = root.aliases.iter().map(|(old, current)| (old.as_str(), current.as_str())).collect();
    assert_eq!(aliases, [("title", "heading"), ("who_name", "by_name"), ("status", "phase"), ("status_since", "phase_since")]);
    let fields: Vec<(&str, &[String])> = root.fields.iter().map(|field| (field.name.as_str(), &field.aliases[..])).collect();
    assert_eq!(fields, [("heading", &["title".to_owned()][..]), ("by", &["who".to_owned()][..]), ("phase", &["status".to_owned()][..])]);
    let tables: Vec<(&str, &str, &[String])> =
        set.tables[1..].iter().map(|table| (table.name.as_str(), table.field.as_str(), &table.field_aliases[..])).collect();
    assert_eq!(
        tables,
        [("renamed_labels", "labels", &["tags".to_owned()][..]), ("renamed_comments", "comments", &["notes".to_owned()][..])]
    );
    assert!(set.tables[1].aliases.is_empty(), "a nested table's columns restart their names");
    assert_eq!(set.tables[2].aliases, [("text".to_owned(), "body".to_owned())]);
    assert_eq!(set.tables[2].fields[0].aliases, ["text"]);

    assert_eq!(set.edges, PhaseEdge::CODES);
    let (state, labels) = set.edge_state().expect("one root state");
    assert_eq!((state.name.as_str(), labels), ("phase", &[Code::new("cl"), Code::new("op")][..]));
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Clash", code = "cs")]
struct Clash {
    #[store(alias = "b")]
    a: String,
    b: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Stateless", code = "sl", edges = PhaseEdge)]
struct Stateless {
    title: String,
}

// Behaviour (#3392 A7): the descriptor refuses an alias that names something else of its row too
// (here another field and its column), which would make an old entry ambiguous, and a kind that
// declares edges without exactly one root state for their labels.
#[test]
fn ambiguous_aliases_and_stateless_edges_are_refused() {
    assert!(matches!(Clash::table_set().validate(), Err(DescriptorError::AliasClash { alias, .. }) if alias == "b"));
    assert_eq!(Stateless::table_set().validate(), Err(DescriptorError::EdgeState { kind: "Stateless".into(), states: 0 }));
}
