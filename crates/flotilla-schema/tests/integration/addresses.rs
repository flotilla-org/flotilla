//! Address columns (#3392 G1, ADR 0066 §5): a reference to a definitions-class kind or to anything
//! outside the new store is an `Address`, stored as its canonical text interned.

use std::collections::BTreeSet;

use flotilla_schema::{Address, ColumnType, DecodeError, Decoder, Encoder, Kind, Stored, Value};
use hegel::{generators as gs, TestCase};

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Placard", code = "pl")]
struct Placard {
    project: Address,
    subject: Option<Address>,
    #[store(child)]
    watchers: BTreeSet<Address>,
}

/// Segment values spanning bytes the canonical encoding escapes (`/ : % @`, space, quotes,
/// multi-byte UTF-8) as well as bare ones.
fn value(tc: &TestCase) -> String {
    tc.draw(gs::text().alphabet("aZ09-._~/:@% \"é中").min_size(1).max_size(8))
}

fn role(tc: &TestCase) -> Address {
    let project = Address::project(&value(tc)).expect("any non-empty project name is admitted");
    if tc.draw(gs::booleans()) {
        return project;
    }
    project.child(flotilla_address::Kind::Role, &value(tc)).expect("a role follows a project")
}

// Behaviour: an address field is one interned column, required unless optional, and a set of
// addresses is a child table keyed by the address.
#[test]
fn address_fields_are_interned_columns() {
    let set = Placard::table_set();
    let root = &set.tables[0];
    let column = |name: &str| root.columns.iter().find(|column| column.name == name).expect(name);
    assert_eq!((column("project").ty.clone(), column("project").nullable), (ColumnType::Interned, false));
    assert_eq!((column("subject").ty.clone(), column("subject").nullable), (ColumnType::Interned, true));
    let watchers = set.tables.iter().find(|table| table.name == "placard_watchers").expect("child table");
    assert_eq!(watchers.key.iter().map(|column| column.ty.clone()).collect::<Vec<_>>(), [ColumnType::Interned]);
}

// Property: every address round-trips through its logical row as its canonical text, so two
// equal addresses are one interned string and the stored text parses with no lookups.
#[hegel::test]
fn addresses_round_trip_as_canonical_text(tc: TestCase) {
    let placard = Placard {
        project: role(&tc),
        subject: if tc.draw(gs::booleans()) { Some(role(&tc)) } else { None },
        watchers: (0..tc.draw(gs::integers::<usize>().max_value(3))).map(|_| role(&tc)).collect(),
    };
    let rows = Encoder::encode(&placard).expect("encodes");
    assert_eq!(rows.tables[0][0].values[0], Value::Str(placard.project.as_str().to_owned()));
    assert_eq!(Decoder::decode::<Placard>(rows).expect("decodes"), placard);
}

// Behaviour: interned text that is not a canonical address is refused on decode, naming the
// column, rather than accepted as a second spelling of the same thing.
#[test]
fn non_canonical_address_text_is_refused() {
    let placard = Placard { project: Address::project("flotilla").expect("project"), subject: None, watchers: BTreeSet::new() };
    for text in ["project:a%2fb", "Project:flotilla", "project:", "convoy:demo/project:flotilla", "flotilla"] {
        let mut rows = Encoder::encode(&placard).expect("encodes");
        rows.tables[0][0].values[0] = Value::Str(text.to_owned());
        match Decoder::decode::<Placard>(rows) {
            Err(DecodeError::Address { column, text: found, .. }) => assert_eq!((column.as_str(), found.as_str()), ("project", text)),
            other => panic!("{text}: expected an address refusal, got {other:?}"),
        }
    }
}
