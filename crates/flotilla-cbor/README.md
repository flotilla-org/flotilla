# flotilla-cbor

The stack's CBOR profile ([ADR 0063](../../docs/adr/0063-cbor-is-the-stack-binary-format.md))
as one Rust crate. Rust protocols in Flotilla and its sibling projects encode
and decode messages through it, never by calling a CBOR crate directly. It
depends on no other Flotilla crate, so other repositories use it as a Git
dependency.

## Profile

- **Deterministic writers** (RFC 8949 §4.2.1): shortest integer and float
  forms, definite lengths, and text map keys sorted bytewise by their
  encoding. Every value has one byte form.
- **Floats stay floats.** A float is written in its shortest form and never
  as an integer: an `f64` holding `1.0` is `f9 3c00`. A typed float field
  refuses an integer, so writers in every language write float fields as
  floats. NaN is rejected in every width; `-0.0` and the infinities pass
  through. These float details are current choices, not commitments.
- **Safe readers.** A reader rejects duplicate map keys (compared as decoded
  text, whatever their encoded form), any tag (the allowed-tag list is empty),
  non-text map keys, invalid UTF-8, trailing bytes, NaN, indefinite lengths,
  simple values other than `false`, `true` and `null`, and anything over its
  limits. It accepts well-formed input that merely is not deterministic: a
  longer integer, float or length form, or unsorted keys.
- **Canonical check on demand.** `check_canonical` also requires deterministic
  form, for bytes that are hashed, signed or compared. A signature or hash
  covers the bytes as received, never a re-encoding.
- **Evolution.** Typed decoding ignores unknown keys; a message of an unknown
  `type` is rejected.

## API

| Item | Purpose |
| --- | --- |
| `Limits`, `DEFAULT_LIMITS` | Message size, nesting depth, elements per array or map, byte-string size. The defaults are the ADR's (1 MiB, depth 64, 65,536 elements). Each protocol defines its own tighter limits where it needs them. |
| `to_vec(value, limits)` | Serialize deterministically. The output is scanned like any input, so a writer never produces what a reader rejects. |
| `from_slice(bytes, limits)` | Check as a safe reader, then deserialize. |
| `check(bytes, limits)` | The safe-reader check alone, without decoding. |
| `check_canonical(bytes, limits)`, `is_canonical(bytes, limits)` | The safe-reader check plus deterministic form. |
| `Message` (`TYPES`), `encode_message`, `decode_message`, `message_type` | Typed messages: one map whose `type` text field is in the message's list. |
| `Framing`, `MAGIC_LEN`, `HEADER_LEN` | Stream framing: an 8-byte protocol magic, a big-endian `u32` body length checked against the limit before the body is allocated, then one message body. |
| `Bytes` | A byte-string field; serde writes a plain `Vec<u8>` as an array of integers. |
| `Error`, `Error::kind()` | What went wrong, with the stable names the corpus uses. |

## Read path

1. The message limit, before anything else.
2. `cbor2::validate_slice`: exactly one well-formed item, UTF-8 text, no
   trailing bytes, nesting bounded, no allocation.
3. One scan of the raw bytes, using `cbor2::core`'s header decoder: the
   safe-reader rejects, duplicate keys at every level, and the limits. It
   builds no values and never allocates from a declared length or count; the
   only allocation is a set of key slices per map, which grows with the keys
   actually present. With the canonical check, the same scan also requires
   shortest heads and floats and ascending key order.
4. `cbor2` deserialization, with its recursion limit set from the depth limit.

When step 2 fails, the scan names the problem if it can, so errors are as
specific as the corpus expects.

## Why cbor2

`cbor2` (MIT) has a deterministic writer with bytewise key order, keeps
whole-number floats as floats, is serde-native, and exposes the low-level
header decoder the scan builds on. On its own its reader is too lenient for the
profile (it keeps the last of duplicate keys in a map field, strips unknown
tags, and accepts NaN and indefinite lengths), which is what the scan adds.

It is a young crate, so it is pinned to an exact version (`=1.1.6`) and the
corpus pins its behaviour: an upgrade is a deliberate change that reruns the
corpus. No `cbor2` type appears in the public API, so the crate underneath can
change without touching callers.

## Conformance corpus

[`tests/cbor_corpus`](tests/cbor_corpus/README.md) holds the golden vectors
that define the profile, as JSON any language can run: canonical values and
their bytes, encoder vectors, decoder vectors (accepted although not
deterministic, rejected by every reader, and limits) and frames.
`cargo test -p flotilla-cbor` runs them.
