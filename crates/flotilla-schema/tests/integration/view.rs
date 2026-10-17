//! The schema review view (`render_view`) over derived descriptors.

use flotilla_schema::{column_count, enum_columns, render_view, Kind, Records, Stored, WIDE};

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Inner {
    #[store(code = "c1")]
    A { x: i64, y: i64 },
    #[store(code = "c2")]
    B,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Outer {
    #[store(code = "c1")]
    Plain { text: String },
    #[store(code = "c2")]
    Nested { inner: Inner },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
struct Part {
    kind: Inner,
    note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Wide", code = "k1")]
struct Wide {
    outer: Outer,
    #[store(flatten)]
    part: Option<Part>,
    #[store(records)]
    rows: Records<Part>,
    c00: i64,
    c01: i64,
    c02: i64,
    c03: i64,
    c04: i64,
    c05: i64,
    c06: i64,
    c07: i64,
    c08: i64,
    c09: i64,
    c10: i64,
    c11: i64,
    c12: i64,
    c13: i64,
    c14: i64,
    c15: i64,
    c16: i64,
    c17: i64,
    c18: i64,
    c19: i64,
}

// Behaviour (CLAUDE.md schema review): the view counts each table's backend columns (the root's
// id and version, a nested table's owner and key), attributes enum columns to the outermost data
// enum whose expansion adds them (a nested enum's columns, and an enum inside an optional flatten,
// included), lists child tables and indexes, and calls out tables wider than 30 columns.
#[test]
fn the_view_counts_columns_enum_expansion_and_wide_tables() {
    let set = Wide::table_set();
    let root = &set.tables[0];
    // outer (1) + plain text (1) + nested inner tag (1) + inner x, y (2); part's kind tag (1), x,
    // y (2) and note (1), present by its kind tag (no flag); twenty plain columns; id and version.
    assert_eq!(column_count(root), 2 + 5 + 4 + 20);
    assert_eq!(enum_columns(root), [("outer", 5), ("part_kind", 3)]);
    let rows = &set.tables[1];
    assert_eq!(column_count(rows), 1 + 1 + 1 + 4, "owner, pos, the stamp and the part's columns");
    assert_eq!(enum_columns(rows), [("kind", 3)]);

    let view = render_view([&set]);
    assert!(
        view.contains(&format!("| `wide` | root | {} **wide** | 8: `outer` 5, `part_kind` 3 | `wide_rows` (records) | – |", 31)),
        "{view}"
    );
    assert!(view.contains("| `wide_rows` | records | 7 | 3: `kind` 3 | – | – |"), "{view}");
    assert!(view.contains(&format!("Tables wider than {WIDE} columns:\n- `wide`: 31 columns, 8 from enum expansion\n")), "{view}");
}
