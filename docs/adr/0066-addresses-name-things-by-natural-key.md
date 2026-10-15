# 66. Addresses name things by natural key, in one typed-segment grammar

**Status:** Proposed (owner grilling, 2026-10-15 and 2026-10-16; #3350)
**Date:** 2026-10-15
**Amends:**
- [ADR 0047](0047-stored-data-stays-decodable-across-one-generation.md): a stored-data shim retires only after an explicit re-encode step has run on every host.
- [ADR 0064](0064-primitives-model-resources-records-gated-machines-inboxes.md): its typed-segment inbox addresses become the grammar for every address, receivers and things alike.

**Relates to:**
- [ADR 0055](0055-agent-messages-are-durable-receiver-homed-records.md): messages name their subject and references;
- [ADR 0063](0063-cbor-is-the-stack-binary-format.md): addresses are not opaque documents;
- #3320, the storage derive stack, where the 79-column Message table exposed this;
- #3351 (CR record-name case drift) and #3352 (stall nudges that never settle).

## Context

Flotilla has no single way to name an object. There are 14 overlapping types and string dialects:
- `LeafAddress`;
- two `Subject` types;
- `IssueRef` and `IssueSource`;
- `MessageReference`;
- `ResourceRef`;
- the CR, issue, artifact and usage record-name helpers;
- `LeafTemplate` and `SubjectVariable`;
- message address strings and `RoleAddress`;
- `ChangeRequestRef` and the core `IssueRef`;
- the manifest's `EntityRef`;
- the untyped `Submission.reference`.

Code converts between them by hand at more than 40 sites ([explainer](https://claude.ai/artifact/KayuzRBw8YUvfeCxRxJAMu), §3–4).

The consequences are concrete:
- **One GitHub PR has five spellings.** Some keep the original case and some lowercase it. The service is sometimes a host and sometimes a forge id. Record names are computed from different spellings (#3351).
- **One CR has two spellings inside `MessageReference`:** `ChangeRequest{head}` when its head is known, `ControlRecord{ResourceRef}` when it isn't.
- **The text forms lose information when re-read.**
  - Leaf literals lose spaces and quotes.
  - A label with spaces can't be written.
  - A service or producer containing `/` silently re-parses as different fields.
  - `LeafTemplate`'s text *is* its serde form, so the loss is in stored templates and frozen convoy snapshots.
- **Addresses spread into sparse columns.** Each variant of `MessageReference` and `LeafAddress` contributes its own columns, and they make up 60 of Message's 79.

## Decision

### 1. An address names a thing by its natural key

An address says what a thing is, for example "CR 42 in this repo on this forge" or "the reviewer's review round for this head". It does so whether or not flotilla holds a record of it. It covers flotilla's own objects (project, convoy, role, artifact) and outside objects (repo, change request, issue, comment, commit).

`ResourceRef` stays separate. It means "this particular stored record": its host, namespace and API version. The only join between the two is `record_name(&Address)`:
- one CR can have several observation records;
- an artifact's record name is a one-way hash of its key.

### 2. The namespace comes from context

- **Outside objects have no namespace.** A forge object is the same from any namespace.
- **Flotilla's own objects are relative.** An address is relative to the namespace of whatever holds it.
- **No stored reference crosses namespaces.** A qualified form, `<namespace>:<address>`, exists only for surfaces and the CLI. If cross-island references arrive, the qualified form enters stored data then, and addresses stored before then keep meaning "my own namespace".

### 3. Normalisation happens once, when an address is built

- **A forge is named by its `forge_id`.** GitHub gets a built-in Forge, `github`. Hosts, SSH aliases and URLs are inputs only, resolved through Forge resources. A host that doesn't resolve to a Forge is an error, not a fallback spelling.
- **Case is decided by forge kind, in the constructor only.** GitHub and Forgejo both look up owner and repo case-insensitively (Forgejo compares `lower_name` after lowercasing, `models/repo/repo.go`), so both lowercase owner and repo.
  - **Display case is not stored in the address.** The address is an identity, not a display name. A surface that wants the forge's own casing reads it from the observation record or the forge.
  - **`ForgeKind` is a closed enum.** Adding a forge kind means declaring its case rule in the same change. There is no default, so a case-sensitive forge can't be lowercased by accident.
- **Numbers are numbers.** `007` and `+7` are refused.

### 4. One grammar: ADR 0064's typed segments

Every address is a list of `kind:value` segments over a closed kind enum. Receivers and things use the same grammar.
- **Segments.** Segments are separated by `/`. Each value is percent-encoded on its own, as `ViewAddress` already does (`crates/flotilla-protocol/src/view_address.rs`). Parsing splits on `/` and decodes each value, so nothing is positional and nothing is guessed.
- **The encoding is canonical: one address has exactly one spelling.**
  - Only RFC 3986 unreserved characters (`A–Z a–z 0–9 - . _ ~`) are left bare. Every other byte of the UTF-8 value is encoded, including `/`, `:`, `%` and space.
  - Hex digits are uppercase (`%2F`, never `%2f`).
  - The parser refuses non-canonical input in stored data. Lenient input is canonicalised first; see below.
- **The kind enum is closed.** Adding a kind changes the parser. Under ADR 0047 that is fine, but it is a deliberate change, not an extension point.
- **The path shows ownership.** "Everything in this convoy" or "everything under this repo" is a prefix match **on segment boundaries**. `X` is under `P` when `X = P` or `X` starts with `P/`. A raw string prefix is wrong: `…/convoy:demo` must not match `…/convoy:demo2`. Queries use `= P OR (>= P || '/' AND < P || '0')`, since `0` is the byte after `/`.

```
project:flotilla/convoy:demo/role:coder                     a convoy role's inbox (ADR 0064)
project:flotilla/role:gui-approver                          a standing role's inbox (ADR 0064)
project:flotilla/convoy:demo                                a convoy
forge:github/repo:flotilla-org%2Fflotilla                   a repository
forge:github/repo:flotilla-org%2Fflotilla/cr:42             a change request
forge:github/repo:flotilla-org%2Fflotilla/issue:7           an issue
forge:github/repo:flotilla-org%2Fflotilla/commit:<sha>      a commit (the full 40-character SHA)
```

- **A reference is an address plus a revision.** This replaces `MessageReference` and its two spellings of a CR.
- **A condition (Leaf)** is an address, a field path, an operator and a literal. Field-path segments and literals use JSON string quoting when they contain anything other than a bare word. The printer always produces the canonical form, so print-then-parse returns exactly what went in.
- **Templates** (`$cr .checks == passing`) use the same field-path and literal rules.
- **Input may be lenient, but only in what it resolves.** Short forms (`cr/github/org/repo/42`), PR URLs, hosts and mixed case are accepted when parsing and resolved to the canonical form. Only the canonical form is stored and printed.
  - **Canonical text parses with no lookups.**
  - **Lenient input may need Forge resources** to resolve a host or URL. Where they aren't available, for example during offline template validation or CLI completion, the parse is refused with the unresolved host named. The exception is the built-in `github` Forge, which needs no record.

### 5. Storage: interned canonical text

- **Interned canonical text.** In Rust an address is a typed value. It is stored as its canonical print through the storage derive's `#[store(intern)]`, as one integer column into the `strings` table. Equality is integer equality, and a prefix query is a range scan on the intern table's unique index. That index must use byte-wise (`BINARY`) collation, which the segment-prefix range query in §4 depends on.
- **Address or `Ref`.** A field means "the thing named this", which may not exist or may be outside flotilla, holds an Address. A field means "this stored record, which must exist" holds a `Ref` foreign key.
- **The canonical printer is part of the stored shape.** Interned rows are keyed by the canonical print, so any change to it is a stored-shape change. That includes a new escape rule or a normalisation fix like #3351. Before the cut-over it needs the re-encode step (§6); after it, an explicit migration step.
- **Unused intern entries are cleaned up later.** That is part of archiving: a reference-count or distinct query over the live set, then removing unreferenced intern rows.

### 6. Stored-data shims retire through an explicit re-encode step (amends ADR 0047)

ADR 0047 lets a shim go "once records have been rewritten or reaped", but nothing rewrites records:
- Messages are immutable and kept forever.
- Artifact keys can't change.
- Frozen workflow snapshots are written once.

For those kinds a shim never actually becomes removable, and the roll-time live decode blocks its removal.

- **A re-encode step.** A stored-shape change that lands before the ADR 0064 cut-over uses a general step, "re-encode every record". On startup it decodes each stored record through the current shims, writes it back in the current shape, and records a marker in `resource_store_migrations`.
- **What the step is, and isn't.** It is a storage-level rewrite. It changes how a record is encoded, never its content.
  - **Stays the same:** the decoded value, the `resourceVersion` and the history.
  - **No events:** it emits no change event.
  - **Immutability is untouched:** Message and snapshot immutability are rules about content, which doesn't change.
  - **Replication is untouched:** each host rewrites everything it stores, its own records and its replica copies alike. Replication digests are computed over (name, `resourceVersion`) (`crates/flotilla-store-sqlite/src/sqlite/digest.rs`), so they don't change, and peers at different stages of the step still agree.
- **When a shim can go.** It is removable once that step has run on every host since the shim was added.
  - **The marker is per host,** in that host's own store.
  - **The pre-roll live-store gate (#2167) enforces it.** The gate refuses a candidate that removes a shim while any host's store lacks the step's marker. Retirement is then checked rather than procedural.
- **After the cut-over,** ADR 0064's explicit migration steps cover the same need.

### 7. Rollout

1. **A small crate for the address type**, for example `flotilla-address`. It holds the kinds, segments, encoding, canonical printing, normalisation, `record_name`, and the condition and template grammar. It gets property tests for print-then-parse, and each case in the appendix below as a fixed test.
2. **Live code uses it internally; stored shapes stay as they are.** Conversion happens at the read and write boundary. The hand-written conversions and the old types go, and #3351's drift ends, with no stored-data shims.
3. **The ADR 0064 model stores addresses natively.** The cut-over importer converts old records once, and normalisation happens there. The combined engine and storage stack (#3319, #3320) uses addresses from the moment the crate lands.
4. **Receivers move to ADR 0064 inbox addresses as part of the primitives inbox work.** The code still uses ADR 0055's `project/convoy/vessel/role`, `principal:` and `system:` strings.

### 8. Artifacts, work and usage (owner rulings, 2026-10-16)

- **An artifact address names the slot, and the file's digest is its revision.**
  - The blob store already names every file by its SHA-256 (`crates/flotilla-daemon/src/blob_store.rs`), so files are immutable.
  - The artifact record is a named **slot** pointing at the current file. Publishing to the same slot stores the new file and moves the pointer. Files no longer referenced are swept (`reap_expired`, `crates/flotilla-daemon/src/artifact.rs`).
  - Code finds artifacts by slot ("the coder's brief", "the convoy's decision ledger", "the reviewer's review round for this head"), so the slot is what an address names. This is the same pattern as a branch name pointing at a commit, or a container tag pointing at an image digest.
  - A slot sits under the role that produces it: `project:P/convoy:C/role:reviewer/artifact:review-round/of:<subject>`. Each artifact kind declares what its subject means; a review round's subject is a commit.
  - A reference that pins an exact version adds the digest as its revision.
  - Image-build logs, which today have no convoy and no role and are named outside the artifact scheme (`crates/flotilla-daemon/src/image_build.rs`), need an owning role or root when they move to this scheme.
- **There is no `work` kind.**
  - `ConvoyStatus.work` is the convoy's per-vessel progress record, and `crew_work` holds per-role progress. Neither is a thing of its own.
  - A condition on a crew's progress addresses the role: `project:P/convoy:C/role:coder .phase == Done`. This replaces `work/<convoy>/<vessel> .crew.<role>.phase == Done`.
- **Usage is a small root kind: `usage:<provider>/account:<account>`.**
  - It is a host- or account-level fact, not owned by a project or forge.
  - Its expected main user is a scheduler checking capacity before a vessel is deployed. Wait conditions on it, which the condition machinery already supports, are a secondary use.

## Consequences

- **One type replaces the 14,** and `record_name` replaces four helpers.
- **The Message table loses its sparse address columns.**
  - Its conditions are one text column each. They stay on the message, because they are fixed when it is sent and judged on the receiver's host (#3350 investigation: deriving them would read a convoy replica that can lag or be deleted).
  - A reference becomes an interned address plus a revision.
  - The subject becomes a marker on one reference.
- **Fixtures need changes.** Existing replay fixtures and the golden corpus are unaffected while stored shapes stay as they are (rollout step 2). The importer's fixtures carry the conversion.
- **Text that people type now has a strict grammar.** That covers workflow templates and CLI conditions. Plain cases such as `convoy/demo .status.phase == Landed` parse as before through the lenient input path.

## Appendix: round-trip failures today

These cases were found by running the current parsers on 2026-10-15. Each one becomes a fixed test in the address crate, and print-then-parse must return exactly the input. They are recorded here because the explainers linked above are not durable.

| Form | Input | Today's result |
|---|---|---|
| Leaf literal | `a  b` (two spaces), ` a` (leading space), `a<TAB>b` | `a b`, `a`, `a b` |
| Leaf literal | `"quoted"`, `'q'` (the quotes are part of the value) | `quoted`, `q` |
| Leaf literal | empty | parse error |
| Leaf field path | `.a b`; empty | parse error |
| Leaf | label path `.labels.good first issue` | can't be written |
| Convoy address | name `my convoy` | parse error |
| CR address | service `forge.example/team` | service `forge.example`, scope `team/o/r`, no error |
| CR address | service `https://forge.example` | service `https:`, scope `/forge.example/o/r`, no error |
| Artifact address | producer `p/q` | producer `p`, kind `q`, subject shifted, no error |
| CR address | `cr/s//1` (empty scope); artifact with an empty subject | accepted |
| CR number | `…/007`, `…/+7` | `…/7` |
| LeafTemplate (stored) | literal `a  b`; literal `"q"` | decodes as `a b`; `q` |
| LeafTemplate (stored) | field path without a leading `.` | encodes, then fails to decode |
| LeafTemplate (stored) | artifact producer or kind containing `.`, `,` or whitespace | can't be written |
