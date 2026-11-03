use std::collections::{BTreeMap, BTreeSet};

use flotilla_schema::{Describe, DescribedRow, RowWriter, TableSet};
use hegel::generators as gs;
use serde_json::{json, Value};

#[derive(Describe)]
enum Kind {
    Plain,
    Tagged { weight: u32, note: Option<String> },
    Wrapped(String),
}

#[derive(Describe)]
struct Item {
    label: String,
    #[shape(child)]
    tags: Vec<String>,
    kind: Kind,
}

#[derive(Describe)]
struct Doc {
    #[shape(child)]
    items: Vec<Item>,
    #[shape(child)]
    counts: BTreeMap<String, u32>,
    #[shape(key)]
    id: String,
    note: Option<String>,
}

fn key(row: &DescribedRow, columns: &[flotilla_schema::ColumnDef]) -> Vec<Value> {
    columns.iter().map(|column| row.get(&column.name).cloned().unwrap_or(Value::Null)).collect()
}

/// Every row fits its table: only described columns, every required one non-null, a unique
/// primary key, and a parent key naming a row of the parent table.
fn assert_fits(set: &TableSet, tables: &BTreeMap<String, Vec<DescribedRow>>) {
    for table in &set.tables {
        let rows = tables.get(&table.name).map(Vec::as_slice).unwrap_or_default();
        let columns: BTreeSet<_> = table.key.iter().chain(&table.columns).map(|column| column.name.as_str()).collect();
        let mut keys = BTreeSet::new();
        for row in rows {
            assert!(row.keys().all(|name| columns.contains(name.as_str())), "{}: {row:?}", table.name);
            for column in table.key.iter().chain(&table.columns).filter(|column| !column.nullable) {
                assert!(row.get(&column.name).is_some_and(|value| !value.is_null()), "{} needs {}: {row:?}", table.name, column.name);
            }
            assert!(keys.insert(Value::Array(key(row, &table.key)).to_string()), "{}: duplicate key in {rows:?}", table.name);
            if let Some(parent) = table.parent {
                let parent = &set.tables[parent];
                let inherited = key(row, table.parent_key());
                assert!(
                    tables[&parent.name].iter().any(|candidate| key(candidate, &parent.key) == inherited),
                    "{}: no parent row for {row:?}",
                    table.name
                );
            }
        }
    }
}

// Behaviour (#3249): a described row crosses as a parent table plus child tables, each child row
// carrying its parent's key, so rows of different parents never collide and every child row joins
// its parent. The key is declared after the child fields, nested children inherit two levels of
// key, and the generator spans empty and duplicate collections, every enum variant and both
// presences of each option.
#[hegel::test]
fn described_rows_fit_their_tables_and_join_their_parents(tc: hegel::TestCase) {
    let text = || gs::sampled_from(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    let note = |tc: &hegel::TestCase| tc.draw(gs::booleans()).then(|| tc.draw(text()));
    let ids: BTreeSet<String> = tc.draw(gs::vecs(text()).max_size(3)).into_iter().collect();
    let docs: Vec<Doc> = ids
        .into_iter()
        .map(|id| Doc {
            items: (0..tc.draw(gs::integers::<usize>().max_value(3)))
                .map(|_| Item {
                    label: tc.draw(text()),
                    tags: tc.draw(gs::vecs(text()).max_size(2)),
                    kind: match tc.draw(gs::integers::<u8>().max_value(2)) {
                        0 => Kind::Plain,
                        1 => Kind::Tagged { weight: tc.draw(gs::integers::<u32>()), note: note(&tc) },
                        _ => Kind::Wrapped(tc.draw(text())),
                    },
                })
                .collect(),
            counts: tc.draw(gs::vecs(text()).max_size(3)).into_iter().map(|key| (key, 1)).collect(),
            id,
            note: note(&tc),
        })
        .collect();
    let set = Doc::table_set();
    let tables = RowWriter::rows(&set.tables[0].name, &docs);
    assert_fits(&set, &tables);
    let count = |name: &str| tables.get(name).map_or(0, Vec::len);
    assert_eq!(count("doc"), docs.len());
    assert_eq!(count("doc_items"), docs.iter().map(|doc| doc.items.len()).sum::<usize>());
    assert_eq!(count("doc_items_tags"), docs.iter().flat_map(|doc| &doc.items).map(|item| item.tags.len()).sum::<usize>());
    assert_eq!(count("doc_counts"), docs.iter().map(|doc| doc.counts.len()).sum::<usize>());
    for (doc, row) in docs.iter().zip(tables.get("doc").into_iter().flatten()) {
        assert_eq!(row["id"], json!(doc.id));
        assert_eq!(row.get("note").cloned().unwrap_or(Value::Null), json!(doc.note));
    }
    let items = docs.iter().flat_map(|doc| &doc.items);
    for (item, row) in items.zip(tables.get("doc_items").into_iter().flatten()) {
        let (tag, weight, wrapped) = match &item.kind {
            Kind::Plain => ("Plain", None, None),
            Kind::Tagged { weight, .. } => ("Tagged", Some(*weight), None),
            Kind::Wrapped(text) => ("Wrapped", None, Some(text.clone())),
        };
        assert_eq!(row["kind"], json!(tag));
        assert_eq!(row.get("kind_weight").cloned().unwrap_or(Value::Null), json!(weight));
        assert_eq!(row.get("kind_wrapped").cloned().unwrap_or(Value::Null), json!(wrapped));
    }
}
