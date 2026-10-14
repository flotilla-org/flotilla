# CBOR profile conformance corpus

Golden vectors for the stack CBOR profile of
[ADR 0063](../../../../docs/adr/0063-cbor-is-the-stack-binary-format.md).
Every encoder and decoder in the stack should pass them; `tests/cbor_corpus.rs`
runs them against `flotilla_cbor`. The vectors are plain JSON, one per line,
in named `groups`, so other languages can reuse them.

| File | Check |
| --- | --- |
| `canonical.json` | Decoding `hex` and re-encoding the value gives `hex` again. |
| `encode.json` | Encoding `json` gives `hex`: keys sorted, whole-number floats kept as floats. |
| `decode.json` | Decoding `hex` under `limits` gives `expect`. Groups: `accepted` (not deterministic, but readers accept it), `rejected` and `limits` (just inside and just over each limit). |
| `frames.json` | Decoding one frame from `hex` under `limits` gives `expect`. Magic `JSTEST01`, and the body's `type` must be `hello` or `goodbye`. |

## Fields

- `name`: optional; failures name the vector by its `hex` otherwise.
- `hex`: lowercase hex, or a list of parts, each a hex string or
  `{"repeat": "<hex>", "count": n}`.
- `json`: where present, the decoded value. A number with a fraction or
  exponent is a float, otherwise an integer: `1.0` and `1` differ, as do
  `-0.0` and `0.0`. Floats are written in their shortest round-trip form.
- `limits`: `default` (1,048,576 bytes per message, depth 64, 65,536
  elements, byte strings up to the message limit) or `small` (the same, but
  65,536 bytes per message and 4,096 per byte string).
- `expect`: `accept`, or the error kind `flotilla_cbor` reports first. A
  decoder in another language may name errors differently, but must accept
  and reject exactly these inputs.

Error kinds: `truncated`, `trailing-bytes`, `malformed`, `invalid-utf8`,
`indefinite-length`, `nan`, `tag`, `unsupported-simple`, `non-text-key`,
`duplicate-key`, `depth-limit`, `element-limit`, `message-limit`,
`byte-string-limit`, `not-a-map`, `missing-type`, `unknown-type` (also a
`type` that is not text) and `bad-magic`.

Limits are checked against declared sizes before anything is allocated, so a
byte string declaring 2^64 - 1 bytes fails with `byte-string-limit`, and an
array declaring 65,536 elements in a short message fails with `truncated`.
Indefinite lengths are rejected on purpose: admitting them later is a corpus
change.

Writer refusals that JSON cannot express (NaN, duplicate keys, integers
outside the CBOR range) are covered by the crate's unit tests.
