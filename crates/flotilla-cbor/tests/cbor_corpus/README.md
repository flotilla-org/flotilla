# CBOR profile conformance corpus

Golden vectors for the stack CBOR profile of
[ADR 0063](../../../../docs/adr/0063-cbor-is-the-stack-binary-format.md):
deterministic writers, safe readers and an on-demand canonical check. Every
encoder and decoder in the stack should pass them. `tests/cbor_corpus.rs` runs
them against `flotilla_cbor`.

The vectors are plain JSON so other languages can reuse them. Each file is an
object with a `description` and either a `vectors` list or, for
`decode.json`, a `groups` list whose entries have a `name`, a `description`
and their own `vectors`. Edit the vectors by hand, and check every new byte
sequence a writer produces against an independent encoder. The canonical and
encode vectors, and the decoded values of the accepted decode vectors, were
last checked against Python's `cbor2` package (6.1.5, `canonical=True`).

## Files

| File | Vectors | Check |
| --- | --- | --- |
| `canonical.json` | 81 | Decoding `hex` gives `diag`, encoding `diag` gives `hex`, and the canonical check accepts `hex`. |
| `encode.json` | 11 | Encoding `diag` gives `hex`, or fails with `error`. |
| `decode.json` | 131 in three groups | See below. |
| `frames.json` | 26 | Decoding one frame from `hex` under `limits` is accepted, or fails with `expect`. |

`decode.json` groups:

| Group | Vectors | Check |
| --- | --- | --- |
| `accepted-not-deterministic` | 35 | Well-formed input not in deterministic form: non-shortest integer, length and float forms, unsorted keys. A safe reader accepts it and decodes it to `diag`; the canonical check rejects it with `canonical`. |
| `rejected` | 70 | Every reader, safe or canonical, rejects it: indefinite lengths, NaN in every width, tags, unsupported simple values, duplicate keys (including copies in different encoded forms), non-text keys, invalid UTF-8, input that is not well-formed, truncated or followed by trailing bytes. |
| `limits` | 26 | Declared sizes and nesting against the limits, with the just-inside and just-over cases. |

In every group, a safe reader gives `expect` and the canonical check gives
`canonical` when the vector has it, or else `expect`. A vector carries
`canonical` where the canonical check meets a non-deterministic form before the
problem a safe reader reports.

**Indefinite lengths are in `rejected` on purpose.** The ADR currently rejects
them because they make limits harder to enforce. Admitting them later is a
deliberate corpus change: the vectors move to `accepted-not-deterministic`,
not a reader option.

## Fields

- `hex`: lowercase hex, or a list of parts. A part is a hex string or
  `{"repeat": "<hex>", "count": n}`, which keeps limit vectors small.
- `diag`: a value in the subset of CBOR diagnostic notation (RFC 8949 §8) the
  corpus needs: decimal integers; floats written with a fraction or exponent,
  plus `Infinity`, `-Infinity` and `NaN`; JSON string literals; `h'..'` byte
  strings; `[..]` arrays; `{..}` maps with text keys; `true`, `false`, `null`.
  A literal with a fraction or exponent is a float even when it is whole:
  `1.0` is the float, written `f93c00`, and `1` the integer. Floats compare by
  bit pattern, so `-0.0` differs from `0.0`. Map entries are written in the
  order they appear on the wire: canonical order in `canonical.json`, any
  order elsewhere.
- `limits`: `default` (ADR defaults: 1,048,576 bytes per message, depth 64,
  65,536 elements per array or map, byte strings up to the message limit) or
  `setup` (Jackstay's setup plane: 65,536 bytes per message, depth 64, 65,536
  elements, 4,096 bytes per byte string, which is where opaque descriptors
  travel).
- `expect`, `canonical` and `error`: `accept`, or the name of the first
  problem `flotilla_cbor` reports. A decoder in another language may report
  errors differently, but must accept and reject exactly these inputs.

| Error | Meaning |
| --- | --- |
| `truncated` | Input ends inside a data item or frame, or a declared length or count exceeds the bytes left. |
| `trailing-bytes` | Bytes follow the data item or frame. |
| `malformed` | Not well-formed (RFC 8949 §5.3.1): reserved additional information, a break outside an indefinite-length item, or a two-byte simple value below 32. |
| `invalid-utf8` | A text string, or map key, that is not UTF-8. |
| `indefinite-length` | An indefinite-length string, array or map. |
| `nan` | Any NaN, whatever its width, sign or payload. |
| `tag` | Any tag: the allowed-tag list is empty. |
| `unsupported-simple` | `undefined` or any simple value other than `false`, `true` and `null`. |
| `non-text-key` | A map key that is not a text string. |
| `duplicate-key` | Two keys in one map with the same decoded text, whatever their encoded form or position. |
| `integer-out-of-range` | An integer outside `-2^64 ..= 2^64 - 1` (encoding only). |
| `non-shortest-integer` | An integer whose head is longer than needed (canonical check only). |
| `non-shortest-length` | A length or count whose head is longer than needed (canonical check only). |
| `non-shortest-float` | A float that a shorter width holds exactly (canonical check only). |
| `unsorted-keys` | Keys not in ascending bytewise order of their encodings (canonical check only). |
| `depth-limit` | Arrays and maps nested deeper than the limit; the outermost is depth 1. |
| `element-limit` | An array or map declaring more elements than the limit. |
| `message-limit` | A message body, or a frame's declared length, over the limit. |
| `byte-string-limit` | A byte string declaring more bytes than the limit. |
| `not-a-map` | A frame body that is not a map. |
| `missing-type` | A frame body without a `type` key. |
| `type-not-text` | A `type` that is not a text string. |
| `unknown-type` | A `type` outside the protocol's list. |
| `bad-magic` | A frame magic other than the protocol's. |

Limits are checked against declared sizes before anything is allocated for
them. A byte string declaring 2^64 - 1 bytes therefore fails with
`byte-string-limit`, and an array declaring 65,536 elements in a one-byte
remainder fails with `truncated`, without either allocating.

## Floats

Whole-number floats stay floats in their shortest form (`1.0` is `f93c00`,
`-2^64` is `fadf800000`), and a reader may refuse an integer in a float field.
`-0.0` and the infinities pass through; NaN is rejected in every width. These
are current choices, not commitments: changing one is a corpus change.

## Frames

`frames.json` uses the test magic `JSTEST01` (hex `4a53544553543031`), a
big-endian u32 body length and a body map whose `type` must be `hello` or
`goodbye`. Other fields are ignored, and a body need not be in deterministic
form.

## Open questions

- **JSON rendering of infinities** is not decided. The proposal is
  `{"f64": "Infinity"}` and `{"f64": "-Infinity"}`, matching the ADR's integer
  escapes. Neither this corpus nor the codec renders JSON.
- **JSON integer cut-off**: ±(2^53 - 1) renders as a bare JSON number and
  ±2^53 as `{"u64": ..}` or `{"i64": ..}`. The canonical vectors include both
  boundaries so a future JSON rendering can pin against them.
