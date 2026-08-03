# Facet vs hand-table for the leaf evaluator (#1322) and the erasure seam (#928)

Status: research note, 2026-08-02. Written for #1322 (leaf evaluation engine)
and the HELD facet-vs-serde question from #928 threads 7/10; verdict also
sequences #1368 (descriptor-driven SQL column projection). Not a ruling.

Currency caveat: the repo-side facts below are verified against this checkout
and the issue tracker today. The facet-crate facts are from pre-2026 primary
sources (facet-rs repos, docs.rs, the author's announcements) known at time of
writing; a background verification pass against August-2026 crates.io/docs.rs
state did not complete before this note was finalized. Every facet claim whose
currency matters is marked **OPEN** with what to check.

## The question

Flotilla needs kind-erased field access over typed resource records. The ruled
motivators:

1. **Leaf evaluator** (#1322): evaluate `(record address, field path,
   operator, bound literal)` → True/False/Unknown against
   `ResourceObject<T>`, with loud admission-time validation that a path
   exists on the kind's type. Slice 1 **merged** (PR #1365) with a
   hand-written `LeafSubject` seam.
2. **ControllerLoop dyn-erasure seam** (#928 ruling, 2026-07-23): "loop over
   erased objects + thin per-kind codec … exactly where a facet-style
   descriptor would later slot in (compatible with leaving serde, not
   competing)". Held then because the only motivation was compile cost
   (14,238 LLVM lines — "a minnow vs daemon's 2.08M").
3. **SQL column projection** (#1368, ruled 2026-08-02): project declared
   fields of table-ish resources into queryable SQLite columns — wants a
   descriptor walkable *without a value instance*.

(#928's held ruling: "no investment in serde-shape tuning while moving off
serde (facet direction) is an open question — don't optimize what we may
leave." A functional consumer changes that calculus; that is what this note
weighs.)

## Repo-side facts (verified against this checkout)

### Type inventory

- 17 `define_resource!` kinds in `crates/flotilla-resources/src/` (Convoy,
  Vessel, Host, Checkout, Clone, Repository, Project, Environment,
  MaterialPool, Presentation, PlacementPolicy, Regard, Demand,
  TerminalSession, CredentialSpec, CredentialGrant, WorkflowTemplate).
- **137 types derive `Serialize` in the crate** — the true transitive derive
  surface for any reflection derive. "~15 resource types" undercounts by ~9×:
  every struct/enum reachable from a spec/status needs the derive, plus
  foreign leaf types (`chrono::DateTime<Utc>`, `serde_json::Value`,
  `flotilla_protocol::NodeId`).
- The `Resource` trait (`resource.rs:96`) bounds `Spec`/`Status` with
  `Serialize + DeserializeOwned` — the monomorphization drag #928 measured.

### Serde attribute surface (the underrated cost)

58 `rename`/`rename_all` occurrences across the crate (snake_case,
kebab-case, lowercase, camelCase variants), `rename = "apiVersion"` /
`"ownerReferences"` / `"resourceVersion"` / `"deletionTimestamp"` on meta,
internally tagged enums (`tag = "kind"`), **`untagged`** (`InputValue`,
`convoy.rs`), **`flatten`** over `BTreeMap<String, serde_json::Value>`
(`PlacementStatus`), and pervasive `skip_serializing_if` + `default`. The
store/wire format is serde's output; any path grammar must resolve against
*serialized* names to stay consistent with what operators see in the store.

### What leaf paths actually look like

ADR 0027 ("Conditions: leaves only") fixes the vocabulary: field comparison,
latest claim disposition/phase, before/after freshness — deliberately tiny,
no connectives. The #1262 scenario fixtures show the concrete shapes:

```yaml
- fact: convoy.phase        # → status.phase, unit enum ConvoyPhase
  equals: Landing
- fact: crew-work.phase     # → status.crew_work[vessel][role].phase
  subject: {vessel: work, role: shepherd}
  equals: Done
```

Note: leaves address **into BTreeMaps by key** via subject parameters, not
path-embedded keys. Timestamps (`started_at: Option<DateTime<Utc>>`) serve
the freshness leaves. Phases are unit enums serializing as PascalCase
variant names (no `rename_all` on `ConvoyPhase`/`WorkPhase`/`CrewWorkPhase`).

### The decisive precedent: field_ownership.rs already is the hand table

`crates/flotilla-resources/src/field_ownership.rs` (ADR 0024) already
implements the exact pattern the "hand-written accessor table" option
proposes:

- `FieldOwnedResource::FIELD_OWNERSHIP: &'static [FieldOwnership]` — a
  per-kind static table of declared dotted paths
  (`placement_policy.rs:25`, 11 rows).
- `serialized_spec_field_value<T>` — `serde_json::to_value(spec)` +
  `value_at_path` (`split('.')` + `Value::get`): a dotted-path walk over
  **serialized** names, inheriting every serde attribute for free.
- Per-field typed overrides via `spec_field_value` match arms where default
  serialization isn't the right comparison value.
- Loud validation against the declared table (paths must be rooted and
  listed).

So `LeafSubject` (now merged in #1365) is a generalization of an established
repo seam, not a new invention.

### Serde-as-reflection: what it can and cannot do

Because every type already serializes, `serde_json::to_value(&obj)` + a
pointer walk gives generic, serialized-name-faithful field access with zero
new derives — one allocation per eval, irrelevant at daemon rates. Its one
genuine gap: **absent ≠ typed**. `skip_serializing_if` means `None` fields
are *absent* from the JSON, so a value instance cannot answer "does this path
exist on this kind's type, and is it comparable?" — admission-time validation
needs either a declared vocabulary (the hand table; arguably a feature given
ADR 0027's smuggling check) or a real value-free schema. A value-free schema
is the one capability serde cannot provide and facet's `SHAPE` can.

## Facet (facet-rs, fasterthanlime) — pre-2026 knowledge, currency OPEN

- **What it is**: `#[derive(Facet)]` emits a `const SHAPE: &'static Shape` —
  *static data describing the type*, not monomorphized code. Format crates
  (facet-json etc.) are interpreters over shapes; `facet-reflect` provides
  `Peek` (read a value through its shape) and `Poke`/build (construct one).
  This is exactly Robert's "type descriptor as bytecode + interpreter" shape
  named in #928 thread 7/10.
- **Maturity**: 0.x throughout 2025 with rapid breaking-release cadence and
  loud author caveats about production use; sponsor-driven development.
  **OPEN**: August-2026 versions of facet/facet-core/facet-reflect, release
  cadence over the past year, MSRV, whether the stability caveats have been
  lifted, named production users.
- **Peek walking**: struct fields are reachable by name
  (`field_by_name`-style), enums expose active variant + fields, Option and
  map/list shapes have dedicated peek forms — composing these per path
  segment reaches a leaf scalar; a built-in string-path API may or may not
  exist. **OPEN**: exact current API names and whether map-by-key access
  (needed for `crew_work[vessel][role]`) is first-class.
- **Naming**: facet has its **own** attribute namespace (`#[facet(rename)]`,
  `rename_all`, …); it does not read `#[serde(...)]`. **OPEN** whether any
  serde-attr bridging has landed by Aug 2026 — if not, serialized-name
  parity means mirroring all 58 rename/rename_all sites plus tag semantics,
  with silent-drift risk unless a test diffs shape names against
  `serde_json` output per type. `untagged` and `flatten` equivalents are the
  weakest points — **OPEN** whether facet can represent either;
  `PlacementStatus` (flatten over `serde_json::Value`) and `InputValue`
  (untagged) are concrete blockers if not.
- **Schema without a value**: yes in principle — `T::SHAPE` is const static
  data; fields/variants/inner shapes are walkable with no instance. This is
  the genuine unique capability (admission validation, #1368 projection).
  **OPEN**: exact `Shape`/`Def`/`Type` structure on current docs.rs.
- **Foreign types**: every transitively-reached field type must implement
  `Facet` or the derive fails. `chrono` support existed via feature/impl
  crates — **OPEN** current status; `serde_json::Value` and
  `flotilla_protocol::NodeId` need impls or opaque handling. This alone
  makes "derive Facet on the resource tree" a cross-crate change, not a
  15-line diff.
- **Compile cost**: the design pitch is derive-emits-data, so per-type cost
  should undercut serde's monomorphized visitors; the author published
  comparisons. **OPEN**: numbers (llvm-lines per derived type; whether the
  proc-macro itself is heavy). Note the #928 context: the seam it would
  serve measured 14k LLVM lines — compile cost cannot *motivate* adoption
  here even if favorable.

## The honest baseline, costed

~12-leaf vocabulary over 4 kinds (Convoy, Vessel, + claim/turn kinds):

- Shared once (~80–120 lines): `LeafValue` (Str/Bool/Int/Time/Unknown),
  `LeafType`, operator eval (equals/before/after/exists), the `LeafSubject`
  trait (`leaf_schema() -> &'static [(&'static str, LeafType)]` +
  `leaf(&self, path, subject) -> LeafValue`). **Merged in #1365.**
- Per kind (~20–40 lines): a match over its 3–6 paths, one-line projections;
  map-keyed leaves take subject params. Default arm can route through the
  existing `serde_json::to_value` walk so hand code is only the typed
  overrides.
- Total ≈ 200–280 lines + one fixture test asserting every schema row
  resolves to its declared type (keeps table and accessors in sync).
- Maintenance: one schema row + one match arm per new leaf; one impl block
  per new kind. Admission validation = membership in the declared schema —
  which ADR 0027's "deliberately tiny" condition language and #1322's
  smuggling check treat as a feature, not a limitation.
- #1368 fit: the same declared-row shape (`path`, `LeafType`, now +
  `column`) drives SQL projection; `FIELD_OWNERSHIP` proves the per-kind
  static-table pattern scales to ~11 rows/kind without pain.

## Weighing the five-motivator gravity

What changed since the HELD ruling: three *functional* consumers now exist
(leaf admission validation, the erasure-seam codec, #1368 projection), and
facet's unique capability — a value-free schema — is precisely what two of
them want. That is real gravity toward a descriptor.

What did not change: (a) the store format is serde's, so facet enters as a
*second* description of the same types, and the serde-attribute parity
problem (58 renames, tag/untagged/flatten, absent-field semantics) is a
correctness risk across ~137 transitive types plus foreign impls; (b) facet's
0.x churn (currency OPEN) would sit under admission validation — a
load-bearing, operator-facing surface; (c) every confirmed consumer's actual
vocabulary is *declared and small* — ADR 0027 leaves, #1368 projected
columns, FIELD_OWNERSHIP rows — which a static table serves with loud
validation and zero new dependency risk.

## Recommendation: **seam-now-facet-later** (seam already landed)

Keep the #1365 `LeafSubject` seam and grow the declared-table pattern:

1. Extract one shared "declared paths" module (schema row + serde-JSON walk +
   typed override), unifying `FIELD_OWNERSHIP`, leaf schemas, and #1368's
   column projection — three consumers of one ~150-line pattern. #1368 should
   sequence on this, **not** on facet.
2. Before any facet commitment, run a one-kind spike behind the seam: derive
   `Facet` on the Convoy tree only; measure derive compile cost, check
   chrono/`serde_json::Value` impl coverage, and diff `SHAPE` names against
   `serde_json` output. The spike also closes every OPEN above.
3. Re-open adopt-facet when any trigger fires: the vocabulary outgrows
   declared tables (arbitrary operator-authored paths); #929 llvm-lines makes
   leaving serde a live plan (facet then replaces serde rather than
   duplicating it — the "compatible with leaving serde, not competing" slot
   the #928 ruling named); or facet reaches a stability posture fit for an
   admission-control dependency.

Adopt-facet-now is premature while facet would *duplicate* serde rather than
replace it; hand-table-only is too weak a frame — the table should be built
as the seam's first implementation, which is exactly what #1365 did.

## Sources

- `crates/flotilla-resources/src/resource.rs` (Resource trait, meta renames),
  `convoy.rs` (ConvoyStatus/phases/maps), `field_ownership.rs`
  (`FIELD_OWNERSHIP`, `serialized_spec_field_value`, `value_at_path`),
  `placement_policy.rs` (table example), `controller/mod.rs:379`
  (`ControllerLoop`).
- `docs/adr/0027-workflow-substrate-verbs-over-the-existing-log.md`
  ("Conditions: leaves only"); `prototypes/1262-scenarios/*.yaml` (leaf
  shapes); issue #1322 (contract), #928 comments 2026-07-23/24 (HELD ruling,
  erasure-seam quote, thread 7/10 facet note), PR #1365 (slice 1), #1368
  (SQL projection motivator).
- Facet: github.com/facet-rs/facet, docs.rs facet-core/facet-reflect,
  fasterthanlime's announcements — pre-2026 state; all currency-sensitive
  claims marked OPEN pending the one-kind spike or a fresh docs pass.
