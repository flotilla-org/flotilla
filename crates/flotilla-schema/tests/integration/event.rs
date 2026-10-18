//! Log entries through their CBOR encoding.

use std::collections::BTreeMap;

use flotilla_cbor::ErrorKind;
use flotilla_schema::{Cause, Change, Code, Effect, Event, Hash, Identity, KindCode, Named, Step, Timestamp, Value, LOG_LIMITS};
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
        5 => Value::Ref(identity(tc)),
        _ => Value::Object(hash(tc)),
    }
}

fn identity(tc: &TestCase) -> Identity {
    Identity {
        kind: KindCode::new(tc.draw(gs::sampled_from(vec!["a", "k1", "zz_9", "abcdefgh"]))),
        name: tc.draw(gs::text().max_size(4)),
        home: tc.draw(gs::text().max_size(4)),
    }
}

fn hash(tc: &TestCase) -> Hash {
    let bytes = tc.draw(gs::binary().min_size(32).max_size(32));
    Hash(bytes.try_into().expect("32 bytes"))
}

fn name(tc: &TestCase) -> String {
    tc.draw(gs::text().max_size(6))
}

fn named(tc: &TestCase) -> Named {
    (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(3))).map(|_| (name(tc), value(tc))).collect()
}

/// The root (empty) and nested rows, up to two tables deep.
fn row(tc: &TestCase) -> Vec<Step> {
    (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(2))).map(|_| Step { field: name(tc), key: named(tc) }).collect()
}

/// Every change variant.
fn change(tc: &TestCase) -> Change {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
        0 => Change::Set { row: row(tc), field: name(tc), values: named(tc) },
        1 => Change::State { row: row(tc), field: name(tc), values: named(tc) },
        2 => Change::Put { row: row(tc), collection: name(tc), key: named(tc), values: named(tc) },
        3 => Change::Remove { row: row(tc), collection: name(tc), key: named(tc) },
        _ => Change::Append { row: row(tc), records: name(tc), position: tc.draw(gs::integers::<u32>()), values: named(tc) },
    }
}

fn code(tc: &TestCase) -> Code {
    Code::new(tc.draw(gs::sampled_from(vec!["a", "k1", "zz_9", "abcdefgh"])))
}

fn cause(tc: &TestCase) -> Cause {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(4)) {
        0 => Cause::Create,
        1 => Cause::Edit,
        2 => Cause::Edge { edge: code(tc), from: code(tc), to: code(tc) },
        3 => Cause::Delete,
        _ => Cause::Expunge,
    }
}

fn effect(tc: &TestCase) -> Effect {
    Effect {
        changes: (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(5))).map(|_| change(tc)).collect(),
        objects: (0..tc.draw(gs::integers::<usize>().min_value(0).max_value(2)))
            .map(|_| (hash(tc), tc.draw(gs::binary().max_size(16))))
            .collect(),
        expunged: tc.draw(gs::booleans()).then(|| identity(tc)),
    }
}

fn sequence(tc: &TestCase) -> u64 {
    tc.draw(gs::integers::<u64>().min_value(0).max_value(i64::MAX as u64))
}

// Behaviour (#3392 G8): an entry and its effect decode to what they encode, for every cause,
// change and value variant, across the integer bounds, with and without a payload, a master and
// objects; the encoding is deterministic.
#[hegel::test]
fn events_round_trip_through_cbor(tc: TestCase) {
    let event = Event {
        topic: flotilla_schema::Code::new("all"),
        kind: KindCode::new(tc.draw(gs::sampled_from(vec!["a", "k1", "zz_9", "abcdefgh"]))),
        name: name(&tc),
        home: name(&tc),
        home_seq: sequence(&tc),
        prev_seq: sequence(&tc),
        cause: cause(&tc),
        writer: name(&tc),
        at: Timestamp(tc.draw(gs::integers::<i64>())),
        about: tc.draw(gs::booleans()).then(|| identity(&tc)),
        payload: tc.draw(gs::booleans()).then(|| tc.draw(gs::binary().max_size(8))),
        effect: effect(&tc),
    };
    let bytes = event.encode().expect("encode");
    assert_eq!(Event::decode(&bytes).expect("decode"), event);
    assert_eq!(event.clone().encode().expect("encode"), bytes);
    let effect = event.effect.encode().expect("encode");
    assert_eq!(Effect::decode(&effect).expect("decode"), event.effect);
}

fn append(n: u32) -> Change {
    let values = BTreeMap::from([("edge".to_owned(), Value::Text("edge".into()))]);
    Change::Append { row: Vec::new(), records: "receipts".into(), position: n, values }
}

// Behaviour (LOG_LIMITS): an effect admits the largest frozen document (1 MiB, the cap of
// `Frozen::new`) together with many changes; a byte string over 1 MiB is refused.
#[test]
fn limits_admit_a_maximal_frozen_document_with_its_rows() {
    let bytes = vec![7u8; 1024 * 1024];
    let effect =
        Effect { changes: (0..10_000).map(append).collect(), objects: BTreeMap::from([(Hash::of(&bytes), bytes)]), expunged: None };
    let encoded = effect.encode().expect("a maximal document fits");
    assert_eq!(Effect::decode(&encoded).expect("decode"), effect);

    let bytes = vec![7u8; 1024 * 1024 + 1];
    let over = Effect { objects: BTreeMap::from([(Hash::of(&bytes), bytes)]), ..effect };
    assert_eq!(over.encode().expect_err("over the byte-string bound").kind(), ErrorKind::ByteStringLimit);
}

// Glue: a hash is exactly 32 bytes on the wire; another length is refused on decode.
#[test]
fn a_hash_of_the_wrong_length_is_refused() {
    let values = BTreeMap::from([("doc".to_owned(), Value::Object(Hash([1; 32])))]);
    let effect = Effect { changes: vec![Change::Set { row: Vec::new(), field: "doc".into(), values }], ..Effect::default() };
    let mut bytes = effect.encode().expect("encode");
    // The hash is the only 32-byte byte string: header 0x58 0x20, then the bytes.
    let at = bytes.windows(2).position(|pair| pair == [0x58, 0x20]).expect("hash header");
    bytes[at + 1] = 0x1f;
    bytes.remove(at + 2);
    assert!(Effect::decode(&bytes).is_err());
}

// Behaviour (#3320 ruling 5): an object travels as its hash and its bytes only, which are always
// ADR 0063 canonical CBOR, so the stored effect has no media field: an effect holding one object
// and nothing else encodes as exactly `{"o": [{"b": bytes, "h": hash}]}` (canonical key order)
// and decodes back. Empty changes and an absent expunged identity are omitted.
#[test]
fn objects_travel_without_a_media_code() {
    let effect = Effect { objects: BTreeMap::from([(Hash([1; 32]), vec![2, 3])]), ..Effect::default() };
    let encoded = effect.encode().expect("encode");
    let mut expected = vec![0xa1, 0x61, b'o', 0x81, 0xa2, 0x61, b'b', 0x42, 2, 3, 0x61, b'h', 0x58, 0x20];
    expected.extend([1; 32]);
    assert_eq!(encoded, expected);
    assert_eq!(flotilla_cbor::from_slice::<Effect>(&encoded, &LOG_LIMITS).expect("decode"), effect);
}

// Behaviour (#3392 G8): the cause column holds the five texts its CHECK allows.
#[test]
fn causes_name_their_column_text() {
    let causes = [
        Cause::Create,
        Cause::Edit,
        Cause::Edge { edge: Code::new("go"), from: Code::new("a"), to: Code::new("b") },
        Cause::Delete,
        Cause::Expunge,
    ];
    assert_eq!(causes.map(|cause| cause.name()), ["create", "edit", "edge", "delete", "expunge"]);
}
