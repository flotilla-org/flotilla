//! `#[store(cbor)]`: serde documents inline in one column of their row, without a backend.

use std::collections::BTreeMap;

use flotilla_schema::{ColumnType, DecodeError, Decoder, EncodeError, Encoder, Kind, Stored, Value, INLINE_LIMITS};
use hegel::{generators as gs, TestCase};
use serde::{Deserialize, Serialize};

/// A document whose shape storage never sees: nested, optional and keyed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Args {
    reason: Option<String>,
    reads: Vec<(String, u64)>,
    tags: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Stored)]
enum Shape {
    #[store(code = 1)]
    Round,
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
enum Variant {
    #[store(code = 1)]
    Plain,
    #[store(code = 2)]
    Carrying {
        #[store(cbor)]
        args: Args,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Stored)]
#[store(kind = "Doc", code = 1)]
struct Doc {
    shape: Shape,
    #[store(cbor)]
    args: Args,
    #[store(cbor)]
    maybe: Option<Args>,
    variant: Variant,
}

fn args(tc: &TestCase) -> Args {
    let count = || tc.draw(gs::integers::<usize>().max_value(3));
    Args {
        reason: tc.draw(gs::booleans()).then(|| tc.draw(gs::text())),
        reads: (0..count()).map(|_| (tc.draw(gs::text()), tc.draw(gs::integers::<u64>()))).collect(),
        tags: (0..count()).map(|_| (tc.draw(gs::text()), tc.draw(gs::booleans()))).collect(),
    }
}

// Behaviour (#3320 ruling of 2026-10-15): a `cbor` field is one non-null BLOB column holding the
// value's canonical CBOR (ADR 0063), whatever its shape: an `Option` is the document `null`, not a
// NULL column, and the column is null only under an inactive variant. Values round-trip.
#[hegel::test]
fn cbor_fields_are_one_column_of_canonical_cbor(tc: TestCase) {
    let set = Doc::table_set();
    let columns: Vec<(&str, &ColumnType, bool)> =
        set.tables[0].columns.iter().map(|column| (column.name.as_str(), &column.ty, column.nullable)).collect();
    assert_eq!(
        columns,
        [
            ("shape", &ColumnType::Code(vec![1]), false),
            ("args", &ColumnType::Cbor, false),
            ("maybe", &ColumnType::Cbor, false),
            ("variant", &ColumnType::Code(vec![1, 2]), false),
            ("variant_carrying_args", &ColumnType::Cbor, true),
        ]
    );
    assert_eq!(set.tables.len(), 1, "a document adds no table");
    let doc = Doc {
        shape: Shape::Round,
        args: args(&tc),
        maybe: tc.draw(gs::booleans()).then(|| args(&tc)),
        variant: if tc.draw(gs::booleans()) { Variant::Carrying { args: args(&tc) } } else { Variant::Plain },
    };
    let rows = Encoder::encode(&doc).expect("encode");
    let values = &rows.tables[0][0].values;
    assert_eq!(values[1], Value::Blob(flotilla_cbor::to_vec(&doc.args, &INLINE_LIMITS).expect("cbor")));
    assert_eq!(values[2], Value::Blob(flotilla_cbor::to_vec(&doc.maybe, &INLINE_LIMITS).expect("cbor")));
    assert_eq!(values[4] == Value::Null, doc.variant == Variant::Plain);
    assert_eq!(Decoder::decode::<Doc>(rows).expect("decode"), doc);
}

// Behaviour: an inline document is bounded at 64 KiB (larger ones belong in `frozen`), and stored
// bytes that are not canonical CBOR of the field's type are refused, naming the column.
#[test]
fn cbor_fields_are_bounded_and_verified() {
    let big = Args { reason: Some("x".repeat(64 * 1024)), reads: Vec::new(), tags: BTreeMap::new() };
    let doc = Doc { shape: Shape::Round, args: big, maybe: None, variant: Variant::Plain };
    assert!(matches!(Encoder::encode(&doc), Err(EncodeError::Cbor { column, .. }) if column == "args"));

    let small = Args { reason: None, reads: Vec::new(), tags: BTreeMap::new() };
    let mut rows = Encoder::encode(&Doc { args: small, ..doc }).expect("encode");
    rows.tables[0][0].values[2] = Value::Blob(vec![0x01]);
    assert!(matches!(Decoder::decode::<Doc>(rows), Err(DecodeError::Cbor { column, .. }) if column == "maybe"));
}
