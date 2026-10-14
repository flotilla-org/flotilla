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

fn kind<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorKind {
    result.unwrap_err().kind()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn typed_message_round_trips_with_sorted_keys_and_float_floats() {
    let bytes = encode_message(&offer(), &SMALL_LIMITS).unwrap();
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
    let error = decode_message::<Setup>(&unknown, &SMALL_LIMITS).unwrap_err();
    assert_eq!((error.kind(), error.to_string().as_str()), (ErrorKind::UnknownType, r#"unknown-type: "grant""#));
    assert_eq!(message_type(&bytes, &SMALL_LIMITS, Setup::TYPES).unwrap(), "selection");
    // Writers check the type too.
    #[derive(Serialize, Deserialize)]
    struct Untyped {}
    impl Message for Untyped {
        const TYPES: &'static [&'static str] = &["x"];
    }
    assert_eq!(kind(encode_message(&Untyped {}, &SMALL_LIMITS)), ErrorKind::MissingType);
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
    // A float field refuses an integer.
    assert_eq!(kind(from_slice::<f64>(&[0x01], &DEFAULT_LIMITS)), ErrorKind::Schema);
    let bytes = to_vec(&0.1f32, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<f32>(&bytes, &DEFAULT_LIMITS).unwrap(), 0.1f32);
}

#[test]
fn integers_cover_the_cbor_range() {
    for n in [i128::from(u64::MAX), -(1 << 64), i128::from(i64::MIN), -1] {
        let bytes = to_vec(&n, &DEFAULT_LIMITS).unwrap();
        assert_eq!(from_slice::<i128>(&bytes, &DEFAULT_LIMITS).unwrap(), n);
    }
    let bytes = to_vec(&-1i64, &DEFAULT_LIMITS).unwrap();
    assert_eq!(kind(from_slice::<u64>(&bytes, &DEFAULT_LIMITS)), ErrorKind::Schema);
}

#[test]
fn writers_refuse_what_readers_reject() {
    assert_eq!(kind(to_vec(&[1.0, f64::NAN], &DEFAULT_LIMITS)), ErrorKind::Nan);
    assert_eq!(kind(to_vec(&BTreeMap::from([(1u32, 2u32)]), &DEFAULT_LIMITS)), ErrorKind::NonTextKey);
    // cbor2 would write these as bignum tags.
    for n in [1i128 << 64, -(1i128 << 64) - 1] {
        assert_eq!(kind(to_vec(&n, &DEFAULT_LIMITS)), ErrorKind::Schema, "{n}");
    }
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
    // cbor2 reports this only through its "duplicate map key" message text,
    // which `to_vec` matches; a cbor2 upgrade that rewords it fails here.
    let result = to_vec(&Outer { a: 1, inner: Inner { a: 2 } }, &DEFAULT_LIMITS);
    assert_eq!(kind(result), ErrorKind::DuplicateKey, "cbor2's duplicate-key message may have changed");
}

#[test]
fn duplicate_keys_are_rejected_into_maps_and_structs() {
    // {"a": 1, "a": 2}, the second key in a non-shortest form.
    let bytes = hex("a261610178016102");
    let error = from_slice::<BTreeMap<String, u8>>(&bytes, &DEFAULT_LIMITS).unwrap_err();
    assert_eq!(error.to_string(), r#"duplicate-key: "a""#);
    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct A {
        a: u8,
    }
    assert_eq!(kind(from_slice::<A>(&bytes, &DEFAULT_LIMITS)), ErrorKind::DuplicateKey);
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
    assert_eq!(kind(to_vec(&descriptor, &SMALL_LIMITS)), ErrorKind::ByteStringLimit);
    let text = "a".repeat(SMALL_LIMITS.max_message);
    assert_eq!(kind(to_vec(&text, &SMALL_LIMITS)), ErrorKind::MessageLimit);
    // The deserializer handles the full depth, and the writer refuses one more.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Nest(Vec<Nest>);
    let mut deep = Nest(vec![]);
    for _ in 0..63 {
        deep = Nest(vec![deep]);
    }
    let bytes = to_vec(&deep, &DEFAULT_LIMITS).unwrap();
    assert_eq!(from_slice::<Nest>(&bytes, &DEFAULT_LIMITS).unwrap(), deep);
    assert_eq!(kind(to_vec(&Nest(vec![deep]), &DEFAULT_LIMITS)), ErrorKind::DepthLimit);
}

#[test]
fn deep_input_fails_without_exhausting_the_stack() {
    let limits = Limits { max_depth: 100_000, ..DEFAULT_LIMITS };
    let bytes = vec![0x81; 100_000];
    assert_eq!(kind(check(&bytes, &limits)), ErrorKind::DepthLimit);
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
    assert_eq!(kind(to_vec(&map, &DEFAULT_LIMITS)), ErrorKind::ElementLimit);
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
    assert!(matches!(framing.read::<Setup, _>(&mut reader), Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof));
    // A stream that ends inside a frame, in its header or its body, is truncated.
    let frame = framing.encode(&offer()).unwrap();
    for cut in [1, HEADER_LEN - 1, HEADER_LEN, frame.len() - 1] {
        assert_eq!(kind(framing.read::<Setup, _>(&mut &frame[..cut])), ErrorKind::Truncated, "cut at {cut}");
    }
    // A header over the limit fails before any body is read or allocated.
    let mut header = b"JSTEST01".to_vec();
    header.extend_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(kind(framing.read::<Setup, _>(&mut header.as_slice())), ErrorKind::MessageLimit);
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
        if check(&input, &SMALL_LIMITS).is_ok() {
            assert!(cbor2::validate_slice(&input).is_ok(), "{input:02x?}");
            from_slice::<cbor2::Value>(&input, &SMALL_LIMITS).unwrap();
        }
    }
}
