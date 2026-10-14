# flotilla-cbor

The stack's CBOR profile ([ADR 0063](../../docs/adr/0063-cbor-is-the-stack-binary-format.md))
as one Rust crate. Rust protocols encode and decode messages through it, never
through a CBOR crate directly. It depends on no other Flotilla crate, so other
repositories use it as a Git dependency.

## Profile

- **Deterministic writers** (RFC 8949 §4.2.1): shortest integer and float
  forms, definite lengths, text map keys sorted bytewise.
- **Floats stay floats**: an `f64` holding `1.0` is `f9 3c00`, and a typed
  float field refuses an integer. NaN is rejected; `-0.0` and the infinities
  pass through.
- **Safe readers** reject duplicate keys (compared as decoded text), any tag,
  non-text keys, invalid UTF-8, trailing bytes, NaN, indefinite lengths,
  simple values other than `false`, `true` and `null`, and anything over the
  limits. They accept input that merely is not deterministic.
- **Evolution**: typed decoding ignores unknown keys; an unknown `type` is
  rejected.

The on-demand canonical check that ADR 0063 describes, for bytes that are
hashed, signed or compared, is not implemented yet. It will be added with its
first user.

## API

| Item | Purpose |
| --- | --- |
| `Limits`, `DEFAULT_LIMITS` | Message size, nesting depth, elements per array or map, byte-string size. The defaults are the ADR's. |
| `to_vec`, `from_slice`, `check` | Encode, decode, or only check any value. A writer's output is checked like any input. |
| `Message`, `encode_message`, `decode_message`, `message_type` | Typed messages: one map whose `type` text field is in the message's list. |
| `Framing`, `MAGIC_LEN`, `HEADER_LEN` | An 8-byte magic, a big-endian `u32` body length checked against the limit before the body is read, then one message body. |
| `Bytes` | A byte-string field; serde writes a plain `Vec<u8>` as an array of integers. |
| `Error`, `ErrorKind` | What went wrong; `ErrorKind::name()` gives the corpus's names. |

## Read path

1. The message limit.
2. `cbor2::validate_slice`: one well-formed item, UTF-8 text, bounded nesting.
3. One scan of the raw bytes for the safe-reader rejects and the limits. It
   never allocates from a declared length or count, and stops at depth 255
   whatever the limits say, so its recursion is bounded.
4. `cbor2` deserialization, with its recursion limit set from the depth limit.

`cbor2` is pinned to an exact version and the [conformance corpus](tests/cbor_corpus/README.md)
pins its behaviour; an upgrade is a deliberate change that reruns the corpus.
