use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;

/// A tighter profile, like the one Jackstay's setup plane uses.
const SMALL_MAX_BYTES: usize = 4 * 1024;
const SMALL_LIMITS: Limits = Limits { max_message: 64 * 1024, max_depth: 64, max_elements: 65_536, max_byte_string: SMALL_MAX_BYTES };

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Setup {
    Offer { tracks: Vec<Track>, clock: f64 },
    Selection { track_ids: Vec<u64>, writer: Option<u64> },
}

impl Message for Setup {
    const TYPES: &'static [&'static str] = &["offer", "selection"];
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Track {
    id: u64,
    kind: String,
    gain: f32,
    descriptor: Bytes,
}

fn offer() -> Setup {
    Setup::Offer {
        tracks: vec![Track { id: u64::MAX, kind: "org.example.custom".into(), gain: 0.5, descriptor: Bytes(vec![0, 1, 2]) }],
        clock: 1.0,
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn typed_message_round_trips_with_sorted_keys_and_float_floats() {
    let bytes = encode_message(&offer(), &SMALL_LIMITS).unwrap();
    assert!(is_canonical(&bytes, &SMALL_LIMITS));
    // Shorter encoded keys sort first: "type", then "clock", then "tracks".
    // The whole-number clock stays a float: f9 3c00.
    // a3, "type": "offer", "clock": f9 3c00, ...
    assert!(bytes.starts_with(&hex("a36474797065656f6666657265636c6f636bf93c00")));
    assert_eq!(decode_message::<Setup>(&bytes, &SMALL_LIMITS).unwrap(), offer());
}

#[test]
fn unknown_keys_are_ignored_and_unknown_types_rejected() {
    #[derive(Serialize)]
    struct Future {
        r#type: &'static str,
        future: Vec<()>,
        track_ids: Vec<u64>,
        writer: Option<u64>,
    }
    let bytes = to_vec(&Future { r#type: "selection", future: vec![()], track_ids: vec![7], writer: None }, &SMALL_LIMITS).unwrap();
    assert_eq!(decode_message::<Setup>(&bytes, &SMALL_LIMITS).unwrap(), Setup::Selection { track_ids: vec![7], writer: None });
    let unknown = to_vec(&BTreeMap::from([("type", "grant")]), &SMALL_LIMITS).unwrap();
    assert!(matches!(decode_message::<Setup>(&unknown, &SMALL_LIMITS), Err(Error::UnknownType(t)) if t == "grant"));
    assert_eq!(message_type(&bytes, &SMALL_LIMITS, Setup::TYPES).unwrap(), "selection");
}

#[test]
fn encoding_checks_the_message_type() {
    #[derive(Serialize, Deserialize)]
    struct Untyped {
        a: u8,
    }
    impl Message for Untyped {
        const TYPES: &'static [&'static str] = &["x"];
    }
    assert!(matches!(encode_message(&Untyped { a: 1 }, &SMALL_LIMITS), Err(Error::MissingType)));
}

#[test]
fn non_deterministic_input_decodes_but_is_not_canonical() {
    // {"type": "selection", "track_ids": [7], "writer": null}, with keys out
    // of order and 7 in a two-byte head.
    let bytes = hex(concat!("a3", "66777269746572f6", "6474797065", "6973656c656374696f6e", "69747261636b5f696473", "811807"));
    assert_eq!(decode_message::<Setup>(&bytes, &SMALL_LIMITS).unwrap(), Setup::Selection { track_ids: vec![7], writer: None });
    assert!(matches!(check_canonical(&bytes, &SMALL_LIMITS), Err(Error::UnsortedKeys)));
}

#[test]
fn floats_keep_their_exact_value() {
    for f in [1.0, 0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY, 0.1, 1e300, -1e19, 18_446_744_073_709_551_616.0, 5e-324] {
        let bytes = to_vec(&f, &DEFAULT_LIMITS).unwrap();
        let back: f64 = from_slice(&bytes, &DEFAULT_LIMITS).unwrap();
        assert_eq!(back.to_bits(), f.to_bits(), "{f}");
    }
    assert_eq!(to_vec(&1.0f64, &DEFAULT_LIMITS).unwrap(), [0xf9, 0x3c, 0x00]);
    assert_eq!(to_vec(&1.0f32, &DEFAULT_LIMITS).unwrap(), [0xf9, 0x3c, 0x00]);
    assert!(matches!(to_vec(&f64::NAN, &DEFAULT_LIMITS), Err(Error::Nan)));
    // A float field refuses an integer.
    assert!(matches!(from_slice::<f64>(&[0x01], &DEFAULT_LIMITS), Err(Error::Schema(_))));
    let bytes = to_vec(&0.1f32, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<f32>(&bytes, &DEFAULT_LIMITS).unwrap(), 0.1f32);
}

#[test]
fn integers_cover_the_cbor_range() {
    for n in [i128::from(u64::MAX), -(1 << 64), i128::from(i64::MIN), -1] {
        let bytes = to_vec(&n, &DEFAULT_LIMITS).unwrap();
        assert_eq!(from_slice::<i128>(&bytes, &DEFAULT_LIMITS).unwrap(), n);
    }
    assert!(matches!(to_vec(&(1i128 << 64), &DEFAULT_LIMITS), Err(Error::IntegerOutOfRange)));
    assert!(matches!(to_vec(&(-(1i128 << 64) - 1), &DEFAULT_LIMITS), Err(Error::IntegerOutOfRange)));
    assert!(matches!(to_vec(&u128::MAX, &DEFAULT_LIMITS), Err(Error::IntegerOutOfRange)));
    let bytes = to_vec(&-1i64, &DEFAULT_LIMITS).unwrap();
    assert!(matches!(from_slice::<u64>(&bytes, &DEFAULT_LIMITS), Err(Error::Schema(_))));
}

#[test]
fn externally_tagged_enums_and_options() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Empty,
        Circle(f64),
        Pair(u8, u8),
        Rect { w: u32, h: u32 },
    }
    let shapes = vec![Shape::Empty, Shape::Circle(1.5), Shape::Pair(1, 2), Shape::Rect { w: 3, h: 4 }];
    let bytes = to_vec(&shapes, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<Vec<Shape>>(&bytes, &DEFAULT_LIMITS).unwrap(), shapes);
    let maybe: Vec<Option<u8>> = vec![None, Some(3)];
    let bytes = to_vec(&maybe, &DEFAULT_LIMITS).unwrap();
    assert_eq!(bytes, [0x82, 0xf6, 0x03]);
    assert_eq!(from_slice::<Vec<Option<u8>>>(&bytes, &DEFAULT_LIMITS).unwrap(), maybe);
}

#[test]
fn map_keys_must_be_text() {
    let map = BTreeMap::from([(1u32, 2u32)]);
    assert!(matches!(to_vec(&map, &DEFAULT_LIMITS), Err(Error::NonTextKey)));
}

#[test]
fn serializer_duplicates_are_rejected() {
    #[derive(Serialize)]
    struct Inner {
        a: u8,
    }
    #[derive(Serialize)]
    struct Outer {
        a: u8,
        #[serde(flatten)]
        inner: Inner,
    }
    let result = to_vec(&Outer { a: 1, inner: Inner { a: 2 } }, &DEFAULT_LIMITS);
    assert!(matches!(result, Err(Error::DuplicateKey(_))), "{result:?}");
}

#[test]
fn duplicate_keys_are_rejected_into_maps_and_structs() {
    // {"a": 1, "a": 2}, the second key in a non-shortest form.
    let bytes = hex("a261610178016102");
    assert!(matches!(from_slice::<BTreeMap<String, u8>>(&bytes, &DEFAULT_LIMITS), Err(Error::DuplicateKey(Some(k))) if k == "a"));
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct A {
        a: u8,
    }
    assert!(matches!(from_slice::<A>(&bytes, &DEFAULT_LIMITS), Err(Error::DuplicateKey(_))));
}

#[test]
fn tags_are_rejected_not_stripped() {
    // 1(1363896240), which cbor2 alone would read as the plain integer.
    assert!(matches!(from_slice::<u64>(&hex("c11a514b67b0"), &DEFAULT_LIMITS), Err(Error::Tag(1))));
}

#[test]
fn byte_strings_need_the_bytes_wrapper() {
    let bytes = to_vec(&Bytes(vec![1, 2]), &DEFAULT_LIMITS).unwrap();
    assert_eq!(bytes, [0x42, 1, 2]);
    // An array of integers is not a byte string.
    let array = to_vec(&vec![1u8, 2], &DEFAULT_LIMITS).unwrap();
    assert!(from_slice::<Bytes>(&array, &DEFAULT_LIMITS).is_err());
}

#[test]
fn encoding_enforces_limits() {
    let descriptor = Bytes(vec![0; SMALL_MAX_BYTES + 1]);
    assert!(matches!(to_vec(&descriptor, &SMALL_LIMITS), Err(Error::ByteStringLimit { .. })));
    #[derive(Serialize)]
    struct Nest(Vec<Nest>);
    let mut deep = Nest(vec![]);
    for _ in 0..64 {
        deep = Nest(vec![deep]);
    }
    assert!(matches!(to_vec(&deep, &DEFAULT_LIMITS), Err(Error::DepthLimit { limit: 64 })));
    let text = "a".repeat(SMALL_LIMITS.max_message);
    assert!(matches!(to_vec(&text, &SMALL_LIMITS), Err(Error::MessageLimit { .. })));
}

#[test]
fn deserializer_handles_the_full_depth() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Nest(Vec<Nest>);
    let mut deep = Nest(vec![]);
    for _ in 0..63 {
        deep = Nest(vec![deep]);
    }
    let bytes = to_vec(&deep, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<Nest>(&bytes, &DEFAULT_LIMITS).unwrap(), deep);
}

#[test]
fn deep_input_fails_without_exhausting_the_stack() {
    let limits = Limits { max_depth: 100_000, ..DEFAULT_LIMITS };
    let bytes = vec![0x81; 100_000];
    assert!(matches!(check(&bytes, &limits), Err(Error::DepthLimit { .. })));
}

#[test]
fn a_map_at_the_element_limit_decodes() {
    let mut map = BTreeMap::new();
    for n in 0..DEFAULT_LIMITS.max_elements {
        map.insert(format!("{n:05}"), ());
    }
    let bytes = to_vec(&map, &DEFAULT_LIMITS).unwrap();
    assert_eq!(&bytes[..5], [0xba, 0x00, 0x01, 0x00, 0x00]);
    assert_eq!(from_slice::<BTreeMap<String, ()>>(&bytes, &DEFAULT_LIMITS).unwrap(), map);
    map.insert("x".into(), ());
    assert!(matches!(to_vec(&map, &DEFAULT_LIMITS), Err(Error::ElementLimit { .. })));
}

#[test]
fn framing_reads_and_writes_streams() {
    let framing = Framing::new(*b"JSTEST01", SMALL_LIMITS);
    let mut wire = Vec::new();
    framing.write(&mut wire, &offer()).unwrap();
    framing.write(&mut wire, &Setup::Selection { track_ids: vec![], writer: Some(3) }).unwrap();
    let mut reader = wire.as_slice();
    assert_eq!(framing.read::<Setup, _>(&mut reader).unwrap(), offer());
    assert!(matches!(framing.read::<Setup, _>(&mut reader).unwrap(), Setup::Selection { writer: Some(3), .. }));
    assert!(matches!(framing.read::<Setup, _>(&mut reader), Err(Error::Io(_))));
    // A header over the limit fails before any body is read or allocated.
    let mut header = b"JSTEST01".to_vec();
    header.extend_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(framing.read::<Setup, _>(&mut header.as_slice()), Err(Error::MessageLimit { .. })));
}

/// Mutated messages never panic the reader, and whatever the checks accept
/// also deserializes as a dynamic value.
#[test]
fn mutated_input_is_rejected_or_decodes() {
    let message = encode_message(&offer(), &SMALL_LIMITS).unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let mut input = message.clone();
        for _ in 0..1 + next() % 4 {
            let at = (next() % input.len() as u64) as usize;
            match next() % 3 {
                0 => input[at] = next() as u8,
                1 => input.truncate(at.max(1)),
                _ => input.insert(at, next() as u8),
            }
        }
        let checked = check(&input, &SMALL_LIMITS);
        let canonical = check_canonical(&input, &SMALL_LIMITS);
        assert!(checked.is_ok() || canonical.is_err(), "{input:02x?}");
        if checked.is_ok() {
            assert!(cbor2::validate_slice(&input).is_ok(), "{input:02x?}");
            from_slice::<cbor2::Value>(&input, &SMALL_LIMITS).unwrap();
        }
    }
}
