# 63. CBOR is the stack's binary format for messages between our own programs

**Status:** Accepted (owner grilling, 2026-10-13)
**Date:** 2026-10-13
**Scope:** Flotilla and its sibling projects: Jackstay, Katzensteg, porthole, Wheelhouse, cleat, Tender
**Relates to:** ADR 0047 (wire formats change freely; stored data stays decodable for one generation), flotilla-org/jackstay#104 (first adopter)

## Context

Our programs talk to each other in several ad hoc encodings, none of which was chosen deliberately:

- Jackstay: fixed binary structs (bootstrap v2), length-prefixed JSON (CPU setup, affordances).
- Katzensteg: JSONL between the desktop WM and its producers.
- Flotilla: `serde_json` in `flotilla-protocol`, `flotilla-transport` and `tender`.

JSON has two concrete problems for machine-to-machine messages. Byte strings need base64, and 64-bit integers lose precision in any non-Rust reader. Several coming uses also need one shared choice:

- event streams from flotilla to presentation surfaces (Wheelhouse, terminals, browsers);
- possibly durable, Kafka-like event logs;
- alternate binary presentations of REST interfaces.

Our consumers are written in Rust, C, Zig and Swift. Most non-Rust code reaches messages through a C ABI.

## Decision

**CBOR (RFC 8949) is the binary format for messages between our own programs.** CDDL (RFC 8610) is the schema language and the cross-language spec.

### Profile

"CBOR" alone means many formats. Ours is:

- **Deterministic encoding** (RFC 8949 §4.2): shortest integer forms, definite lengths only, map keys sorted. Every value has one byte form, so messages and log entries can later be hashed, signed or content-addressed, as atproto's DAG-CBOR does.
- **Floats** are allowed, always in their shortest (preferred) serialization. An integer value is encoded as an integer, never as a float. **NaN is not allowed.** `-0.0` and the infinities are allowed and keep their exact sign and value.
- **Strict readers.** A reader rejects input that isn't in this profile: duplicate map keys (always an error), indefinite lengths, unsorted keys, non-shortest forms, NaN and tags outside the allowed list. Two byte strings never stand for the same value.
- **Text map keys**, so the JSON rendering of the same data stays natural.
- **A short allowed-tag list, initially empty.** Byte strings are native and need no tag; any tag (for example a content-ID tag, if logs need hashing) is added to the list deliberately.
- **Evolution:** readers ignore unknown keys and reject unknown message types. A key is never reused with a new meaning.
- **Hashing and signing use the original bytes.** Because readers drop unknown keys, a signed or content-addressed message is verified over the bytes as received, never over a re-encoding of the decoded value.
- **Stream framing:** a protocol-specific magic, a u32 length, then one CBOR map carrying a `type` field. A reader checks the length prefix (the size of one framed message body) against its limit **before** allocating. Each protocol sets its own limits; one that sets none uses the defaults: 1 MiB per message, nesting depth 64, and 65,536 elements per array or map.
- **One conformance corpus.** A shared set of golden vectors (values and their exact deterministic bytes, including floats, `-0.0`, infinities, large integers and rejected inputs) defines the profile. Every encoder and decoder in the stack must pass it; in Rust, protocols use one shared wrapper that enforces the profile rather than calling a CBOR crate directly. Both live with the first adopter until a second repo needs them.

### Where it applies, and where it doesn't

- **Applies:** control and setup messages, event streams between our programs, and event logs when we have them.
- **Shared memory stays fixed-layout.** Ring headers, frame and block descriptors and other hot-path layouts remain `repr(C)` structs and are never encoded.
- **JSON stays** for human- and agent-facing APIs (porthole's HTTP-over-UDS API, Katzensteg's loopback HTTP) and for config files. CBOR and JSON share a data model, so a REST endpoint may also serve `application/cbor` from the same value by content negotiation.
- **Large integers in JSON:** an integer outside ±(2^53 − 1), which JSON readers outside Rust cannot hold exactly, renders as an object rather than a bare number: `{"u64": "<decimal digits>"}` above the range, `{"i64": "-<decimal digits>"}` below it.
- **The JSON rendering is one-way.** It exists for people, agents and REST responses, and is never parsed back into CBOR. So `{"u64": …}` needs no reserved key, and a genuine map with a `u64` key is no ambiguity in practice.
- **Encodings stay private where possible.** A protocol with a C ABI exposes typed structs, not encoded bytes, so non-Rust consumers need no parser and a later format change stays local.

### Rollout

No repo converts now. Each protocol adopts this when it is next reshaped. Jackstay's setup plane (offer, selection and track notices, grants, affordances) is first, under flotilla-org/jackstay#104.

**Stored data is in scope, without urgency.** Flotilla's stored records may move to CBOR when a store is next reshaped. ADR 0047 governs the move: the new generation writes CBOR and still decodes the previous generation's JSON for one roll. Wire formats still change freely.

## Alternatives considered

- **Protocol Buffers.** Enforced schemas and field-number evolution, ubiquitous through gRPC and Go. Rejected for now: it needs a codegen step in Zig, C and Swift builds, isn't decodable without the schema, and its JSON mapping is a translation (64-bit integers become strings, field names change). It remains the right choice at a boundary with outside gRPC ecosystems, if one appears.
- **MessagePack.** The same niche as CBOR, with a looser spec, no schema language and awkward extension types.
- **FlatBuffers / Cap'n Proto.** Zero-copy schemas pay off for large or hot data; our messages are small, and hot data already lives in fixed-layout shared memory. Thin Zig and Swift support.
- **postcard / bincode.** Tie the wire to Rust struct layout; a dead end for C, Zig and Swift peers.
- **Keep JSON.** Base64 bytes and lossy u64 outside Rust, verbose parsing in C and Zig.

## Consequences

- One binary format across the stack, inspectable through CBOR diagnostic notation without schemas.
- Nothing enforces CDDL schemas at compile time. Validation lives in code and tests, ideally contract tests run by each implementation.
- Event logs, when they exist, still need their own decisions on message versioning and any schema registry; deterministic encoding keeps hashing and signing open.
- Rust code moves from `serde_json` to a serde CBOR crate (`ciborium` or `minicbor`) protocol by protocol. C, Zig and Swift peers that must parse use `tinycbor`/`QCBOR`, `zbor` or a Swift CBOR library.
