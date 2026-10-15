//! Change-log entries through their CBOR encoding.

use flotilla_cbor::ErrorKind;
use flotilla_schema::{Hash, Identity, KindCode, RowChange, RowOp, Value};
use hegel::{generators as gs, TestCase};

/// Every variant, with integers across the whole `i64` range (bounds included), empty and
/// multi-byte text, empty and non-empty blobs, and reference kinds across `u32`.
fn value(tc: &TestCase) -> Value {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(6)) {
        0 => Value::Null,
        1 if tc.draw(gs::booleans()) => Value::Int(tc.draw(gs::sampled_from(vec![i64::MIN, -1, 0, 1, i64::MAX]))),
        1 => Value::Int(tc.draw(gs::integers::<i64>())),
        2 => Value::Text(tc.draw(gs::text().max_size(8))),
        3 => Value::Blob(tc.draw(gs::binary().max_size(8))),
        4 => Value::Str(tc.draw(gs::text().max_size(8))),
        5 => Value::Ref(Identity {
            kind: KindCode(tc.draw(gs::integers::<u32>())),
            name: tc.draw(gs::text().max_size(4)),
            home: tc.draw(gs::text().max_size(4)),
        }),
        _ => Value::Object(hash(tc)),
    }
}

fn hash(tc: &TestCase) -> Hash {
    let bytes = tc.draw(gs::binary().min_size(32).max_size(32));
    Hash(bytes.try_into().expect("32 bytes"))
}

fn values(tc: &TestCase) -> Vec<Value> {
    (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(4))).map(|_| value(tc)).collect()
}

/// Every op variant, with empty keys (root rows) and non-empty ones.
fn op(tc: &TestCase) -> RowOp {
    let table = || tc.draw(gs::text().max_size(6));
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
        0 => RowOp::PutObject { hash: hash(tc), media: tc.draw(gs::integers::<u8>()), bytes: tc.draw(gs::binary().max_size(16)) },
        1 => RowOp::Insert { table: table(), key: values(tc), values: values(tc) },
        2 => RowOp::Update { table: table(), key: values(tc), values: values(tc) },
        3 => RowOp::Delete { table: table(), key: values(tc) },
        _ => RowOp::DeleteResource,
    }
}

// Behaviour (DESIGN change log): an entry decodes to the change it encodes, for every op and
// value variant and across the integer bounds; the encoding is deterministic.
#[hegel::test]
fn row_changes_round_trip_through_cbor(tc: TestCase) {
    let change = RowChange {
        kind: KindCode(tc.draw(gs::integers::<u32>())),
        name: tc.draw(gs::text().max_size(8)),
        home: tc.draw(gs::text().max_size(8)),
        home_seq: tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64)),
        ops: (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(6))).map(|_| op(&tc)).collect(),
    };
    let bytes = change.encode().expect("encode");
    assert_eq!(RowChange::decode(&bytes).expect("decode"), change);
    assert_eq!(change.clone().encode().expect("encode"), bytes);
}

fn insert(n: i64) -> RowOp {
    RowOp::Insert { table: "message_edges".into(), key: vec![Value::Int(n)], values: vec![Value::Text("edge".into()), Value::Null] }
}

// Behaviour (CHANGE_LIMITS): an entry admits the largest frozen document (1 MiB, the cap of
// `Frozen::new`) together with many row ops; a byte string over 1 MiB is refused.
#[test]
fn limits_admit_a_maximal_frozen_document_with_its_rows() {
    let bytes = vec![7u8; 1024 * 1024];
    let mut ops = vec![RowOp::PutObject { hash: Hash::of(&bytes), media: 1, bytes }];
    ops.extend((0..10_000).map(insert));
    let change = RowChange { kind: KindCode(1), name: "m".into(), home: "h".into(), home_seq: 1, ops };
    let encoded = change.encode().expect("a maximal document fits");
    assert_eq!(RowChange::decode(&encoded).expect("decode"), change);

    let bytes = vec![7u8; 1024 * 1024 + 1];
    let over = RowChange { ops: vec![RowOp::PutObject { hash: Hash::of(&bytes), media: 1, bytes }], ..change };
    assert_eq!(over.encode().expect_err("over the byte-string bound").kind(), ErrorKind::ByteStringLimit);
}

// Glue: a hash is exactly 32 bytes on the wire; another length is refused on decode.
#[test]
fn a_hash_of_the_wrong_length_is_refused() {
    let change = RowChange {
        kind: KindCode(1),
        name: "m".into(),
        home: "h".into(),
        home_seq: 1,
        ops: vec![RowOp::Insert { table: "t".into(), key: vec![], values: vec![Value::Object(Hash([1; 32]))] }],
    };
    let mut bytes = change.encode().expect("encode");
    // The hash is the only 32-byte byte string: header 0x58 0x20, then the bytes.
    let at = bytes.windows(2).position(|pair| pair == [0x58, 0x20]).expect("hash header");
    bytes[at + 1] = 0x1f;
    bytes.remove(at + 2);
    assert!(RowChange::decode(&bytes).is_err());
}
