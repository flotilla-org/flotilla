# Annotation layer and materialized views over joined streams

**Date:** 2026-07-26

**Issue:** [#1089](https://github.com/flotilla-org/flotilla/issues/1089)

**Status:** Research recommendation, not an ADR

## Recommendation

Flotilla should model an authored annotation as a first-class, durable
`AnnotationStatement` resource carried by the resource store's existing event
log. It should **not** add an annotation map to every target, create a second
annotation-specific transport, or treat authored facts as view configuration.

The three layers have different jobs:

1. **Statements are resources.** An annotation is an immutable, attributed
   statement about a stable subject identity. A semantic retraction or
   correction is another statement; administrative hard deletion/redaction is
   a separate policy question. The statement remains present when its subject
   is renamed, unavailable, or deleted.
2. **The resource log is the stream.** Annotation statements use the same
   origin-authored logs, durable replicas, relay, and list/watch API as authored
   definitions in
   [ADR 0016](../../adr/0016-overlay-replication-for-cross-root-state.md),
   subject to the authored-resource ruling in
   [#1090](https://github.com/flotilla-org/flotilla/issues/1090). ADR 0016's existing
   per-field merge does not directly merge separate immutable statements; the
   annotation reducer needs slot-level causal context stamped at admission.
   "Separate stream" is useful as a logical description, as with Git notes,
   but it should not become a second Flotilla replication mechanism.
3. **Effective state and reverse indexes are views.** The annotation schema
   declares how statements reduce: retain all notes, add/remove a set member,
   or surface concurrent exclusive values as a conflict. Joins such as
   issue-to-convoys, board columns, effective triage state, and annotations by
   subject are disposable query results. No application writes them directly.

`AnnotationStatement` should be a resource **class/family**, not a universal
string map. Each admitted statement names a versioned annotation schema. That
schema declares its valid subject kinds, typed body, cardinality, reduction
rule, queryable fields, and fallback rendering. The initial implementation can
represent the admitted bodies as a closed Rust enum; future registry-generated
types must still preserve [ADR 0001's](../../adr/0001-k8s-isomorphic-resource-model.md)
typed-struct spine rather than accepting opaque JSON.

This is a hybrid answer to the issue's three choices, but not an ambiguous one:
the **source of truth is a resource kind**, its changes ride the **existing
resource stream**, and its consumers follow a **view convention**. A standalone
annotation stream is rejected.

## Why this fits Flotilla

Several existing rulings constrain the answer:

- Resources are k8s-isomorphic but typed Rust resources, not arbitrary
  Kubernetes objects or JSON blobs
  ([ADR 0001](../../adr/0001-k8s-isomorphic-resource-model.md)).
- The resource store's watch stream is already the event-log and replication
  primitive; aggregators are rebuildable functions over logs
  ([ADR 0002](../../adr/0002-multi-host-is-resource-store-federation.md)).
- Durable intent about an ephemeral observed object is a separate managed
  resource that references it, not a mutation of the observation
  ([ADR 0004](../../adr/0004-observed-resources-ephemeral-and-generational.md)).
- Curated query families have declared typed rows and explicit joins
  ([ADR 0011](../../adr/0011-aggregator-data-plane-is-named-query-result-sets.md)
  and
  [ADR 0014](../../adr/0014-curated-scoped-queries-demand-backed-materialization.md)).
- Definitions-class records already have masterless semantics: every root
  writes only its own log, durable replicas relay origin facts, causally later
  writes supersede, and concurrent writes surface as multi-value conflicts
  rather than being hidden by a clock
  ([ADR 0016](../../adr/0016-overlay-replication-for-cross-root-state.md)).
- Identity and grouping facts are stamped, while hierarchy and reverse
  relationships are derived by joins
  ([ADR 0018](../../adr/0018-presentation-attention-demands-regards-projection.md)).
- [The #1037 ruling](https://github.com/flotilla-org/flotilla/issues/1037)
  makes a convoy's project context an admission-time fact and makes appearance
  query-governed. The
  [#1089](https://github.com/flotilla-org/flotilla/issues/1089) starting position
  likewise treats issue provenance
  stamped at admission as constitutive. The annotation layer must not turn
  either fact into detachable commentary.
- [The #1072 ruling](https://github.com/flotilla-org/flotilla/issues/1072)
  establishes the relevant meta-model precedent: declare entity actions and
  their parameters once, then project them into several surfaces. It did not
  decide an annotation schema, but it shows why vocabulary should have one
  declared owner instead of accreting independently in producers and renderers.

The proposed model is therefore mostly composition of existing decisions. The
new concept is the typed, independently-lived statement and the schema contract
that reduces statements into queryable meaning.

## Evaluation criteria

An approach is suitable only if it:

1. addresses a subject by stable identity rather than current display name;
2. permits an annotation to be authored and read while the subject is offline;
3. distinguishes deletion/recreation from rename or move;
4. preserves author, origin, causal basis, and history;
5. converges across roots without a last-writer clock or a master chosen by CLI
   location;
6. admits typed, versioned vocabulary with readable fallback artifacts;
7. keeps derived reverse indexes and joined board state rebuildable;
8. reuses Flotilla's resource, watch, replication, and query contracts.

## Prior art

### Kubernetes labels, annotations, owner references, and field management

[Kubernetes annotations](https://kubernetes.io/docs/concepts/overview/working-with-objects/annotations/)
are string-to-string metadata stored on the target object. Keys may carry a DNS
prefix, but values can be arbitrary text, JSON, or YAML. Unlike
[labels](https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/),
annotations are not selectors. This is an effective distinction between small,
indexed identity/grouping facts and non-identifying commentary.

It is a poor lifetime model for Flotilla. The annotation is updated through the
same object and disappears with it, so an unavailable foreign object cannot be
annotated. Kubernetes names generally do not model rename:
[UIDs distinguish historical occurrences](https://kubernetes.io/docs/concepts/overview/working-with-objects/names/)
when an object is deleted and another is created under the same name.
[Owner references](https://kubernetes.io/docs/concepts/overview/working-with-objects/owners-dependents/)
store both a readable name and a UID, but they are lifecycle/garbage-collection
links. A Flotilla annotation must not be garbage-collected merely because its
foreign subject is temporarily absent.

[Server-Side Apply](https://kubernetes.io/docs/reference/using-api/server-side-apply/)
is more relevant to merging. It records field managers, derives ownership from
a declared schema, allows different writers to own granular map entries, and
reports a conflict when one manager changes another manager's field unless the
caller forces ownership. This validates schema-specific granularity and visible
conflict. It still assumes one reachable API server and one live canonical
object, so it is not the fleet merge mechanism.

**Take:** retain the labels-versus-commentary distinction and schema-aware field
granularity. Reject co-location, string-only values, and central field
management as the annotation substrate.

### Git notes

[`git notes`](https://git-scm.com/docs/git-notes) attaches a blob to a Git
object without changing that object. Notes live in separate `refs/notes/*`
histories; each update commits a new notes-tree state. This gives commentary an
independent history and lets the ordinary log show a commit together with its
notes.

The address is the annotated object ID. A branch rename or movement does not
matter, but a rewritten commit has a different object ID. Copying notes through
amend or rebase requires explicitly configured `notes.rewriteRef`, and a
collision chooses `overwrite`, `concatenate`, `cat_sort_uniq`, or `ignore`.
Distributed notes refs merge at whole-note granularity using manual, ours,
theirs, union, or line-oriented `cat_sort_uniq` strategies. Notes refs also
require deliberate display and rewrite configuration; propagating every ref,
including notes, is an explicit mirror-clone mode
([`git clone --mirror`](https://git-scm.com/docs/git-clone)).

**Take:** the separate, auditable statement addressed by stable identity is the
right shape. Arbitrary blobs, optional propagation, whole-value merging, and
implicit retargeting during rewrites are not.

### Fossil control artifacts, tags, and phantoms

[Fossil control artifacts](https://www.fossil-scm.org/home/doc/trunk/www/fileformat.wiki)
are immutable, attributed records that add, cancel, or propagate a named
property on another artifact ID. Cancellation is a later artifact. The target
is a cryptographic identity, so a symbolic branch or tag name can change
without moving the statement.

Fossil's
[sync protocol](https://www.fossil-scm.org/home/doc/trunk/www/sync.wiki)
defines repository state as an unordered, grow-only set of immutable artifacts
and describes it as a G-Set CRDT: peers exchange what the other side lacks.
Its [phantom artifacts](https://www3.fossil-scm.org/home/help/www/phantoms)
are known hashes whose content is not currently present. A reference can
therefore be stored and synchronized before the target arrives.

This is the closest precedent for offline Flotilla. Its limitation is the
effective-value rule: when multiple same-name tags target one artifact, the
most recent date wins. Artifact union converges, but timestamp last-writer-wins
can silently erase a real disagreement and depends on clock meaning.

**Take:** adopt immutable authored statements, explicit cancellation,
unresolved subject identities, and union of origin facts. Replace generic
timestamp selection with schema-declared reduction and ADR 0016 causal
conflict semantics.

### Code-review systems: Gerrit NoteDb and Change-Id

[Gerrit NoteDb](https://gerrit-review.googlesource.com/Documentation/note-db.html)
stores change metadata in Git. A numbered change has a meta ref whose commits
record votes and global comments; inline comments are structured JSON keyed by
patch-set commit. Gerrit cites co-replication, auditability, federation, and
offline review as consequences of this design.

[Gerrit Change-Id](https://gerrit-review.googlesource.com/Documentation/user-changeid.html)
separates the logical review identity from any one commit ID. Leaving the
Change-Id intact associates amended, rebased, or cherry-picked commits with the
same review. The effective identity is composite—Change-Id, repository, and
branch—not the footer string alone.

This cleanly separates:

- the stable logical subject (change);
- versions of its content (patch sets);
- commentary about a particular version (inline review);
- commentary about the logical whole (votes and global comments).

Gerrit does not provide masterless semantic merging. Ref updates serialize
through its service. Its lesson is identity and revision scope, not fleet
coordination.

GitHub Projects makes a related distinction. An issue or pull request is linked
as a project item, while the project owns typed field values such as status,
date, number, iteration, and single-select
([Projects GraphQL schema](https://docs.github.com/en/graphql/reference/projects),
[field documentation](https://docs.github.com/en/issues/planning-and-tracking-with-projects/understanding-fields)).
The item wrapper is a separate context about foreign content. It usefully
demonstrates that "Status in board A" is not necessarily a field on the issue.
It also shows the cost of storing a board's current arrangement directly:
multiple projects can hold independent status copies that must not be mistaken
for one global lifecycle fact.

**Take:** distinguish logical identity, revision identity, and contextual
commentary. A board-scoped human decision may be a statement; the board's
layout and mechanically derived lane membership are views.

### W3C Web Annotation, JSON-LD/RDF, and SHACL

The
[Web Annotation Data Model](https://www.w3.org/TR/annotation-model/)
explicitly represents an Annotation, its Body, and one or more foreign Targets.
A `SpecificResource` can constrain a target to a selector and state.
`TimeState` can record the source date and a cached representation, preserving
what the annotator saw even when the live target changes or disappears.
Properties copied from an external resource are hints rather than authoritative
facts.

This supplies two valuable design ideas:

- the statement is independently addressable and its target need not be;
- the statement may carry a non-authoritative subject snapshot/version basis.

Its RDF foundation is intentionally open. JSON-LD supplies global IRI node
identifiers, IRI-keyed vocabulary, typed values, and named graphs
([JSON-LD 1.1](https://www.w3.org/TR/json-ld11/)); RDF represents facts as a
set of subject-predicate-object triples
([RDF 1.2 Concepts](https://www.w3.org/TR/rdf12-concepts/)).
[SHACL](https://www.w3.org/TR/shacl/) can constrain cardinality, datatype,
allowed properties, and closed shapes, so RDF is not incapable of schema.
However, reaching Flotilla's desired contract would require importing an open
graph model and then rebuilding a closed schema, versioning, reduction,
conflict, projection, and fallback-rendering discipline around it.

The failure mode is not merely unfamiliar syntax. It is that arbitrary
predicates become an invisible product API: producers mint vocabulary that
consumers only discover at runtime, no readable canonical artifact owns the
meaning, and merge semantics remain unstated.

**Take:** borrow independent target/state and subject-snapshot concepts. Do not
adopt RDF/JSON-LD as the internal model. Versioned Flotilla schema identifiers
can be globally unambiguous without making every predicate open-ended.

### Event sourcing and materialized-view engines

In the
[Event Sourcing pattern](https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing),
immutable domain events form the system of record; current state is rebuilt by
replay, and query-optimized materialized views are read-only projections. The
guidance emphasizes intent-named events, optimistic concurrency per entity,
event versioning, compensating events, and the eventual consistency between a
write and its read model.

KurrentDB's first-party client contract makes concurrency concrete: an append
may state the stream revision it expects, and a mismatched revision fails
([appending events](https://docs.kurrent.io/clients/node/v1.3/appending-events)).
Its projection tutorial states that a read model is not the source of truth,
must contain no unique data, and can be rebuilt by replaying retained events
([pre-computed read models](https://docs.kurrent.io/dev-center/use-cases/time-travel/tutorial/tutorial-4.html)).

The
[Materialized View pattern](https://learn.microsoft.com/en-us/azure/architecture/patterns/materialized-view)
adds a crucial invariant: a view is disposable, rebuildable from source data,
and never updated directly by an application. Materialize makes the distinction
operational: an ordinary view names a query; indexed views and materialized
views are incrementally maintained; materialized results are durable
([Materialize views](https://materialize.com/docs/concepts/views/)).
Flink describes the same semantics for joined streams: a continuous query over
dynamic tables continuously updates another dynamic table, whose value at a
point in time equals the batch query over the corresponding input snapshot
([Flink dynamic tables](https://nightlies.apache.org/flink/flink-docs-master/docs/dev/table/concepts/dynamic_tables/)).

This does not imply that Flotilla needs a general stream-processing engine.
ADR 0011's named result sets and query projections already implement the small
domain-specific form. It does imply a sharp test:

> If losing a record would lose a human or agent decision, it is source data.
> If it can be recomputed from retained source data, it is a view.

**Take:** annotations are source facts, not projections. Effective annotation
state and joins are projections. Materialize only serving results that justify
the cost; ADR 0014's demand-backed queries remain appropriate for cold or
external joins.

## Comparison against the Flotilla criteria

| Approach | Stable foreign identity | Target may be absent | Multi-writer semantics | Declared vocabulary | Fit |
|---|---|---|---|---|---|
| Embedded k8s annotation map | UID exists, but metadata dies with object | No | Central optimistic update / SSA | Keys namespaced; values stringly | Reject as substrate |
| Git notes | Strong for immutable object; rewrite needs copy | Object can be unshown, but propagation is opt-in | Whole-note merge strategies | Arbitrary blob | Useful shape, insufficient semantics |
| Fossil control artifacts | Strong artifact hash | Yes, via phantom | Immutable fact union; current tag is clock-LWW | Restricted artifact grammar, open tag names | Strongest offline precedent |
| Gerrit NoteDb | Logical Change-Id plus patch-set identities | Offline replica possible | Coordinating service serializes refs | Structured review model | Strong identity precedent |
| Web Annotation / RDF | Global target IRI and target state | Yes; may cache representation | Graph union; application must define conflicts | Open vocabulary; SHACL can constrain | Borrow target/state only |
| View-only convention | Depends on joined inputs | Only while inputs/replicas exist | Deterministic if inputs converge | Query schema | Correct for derived state, cannot preserve authored judgment |
| Special annotation stream | Can be designed correctly | Yes | Must duplicate federation semantics | Can be typed | Reject duplicate substrate |
| **Typed statement resources on existing log** | **Stable UID plus locator hint** | **Yes, first-class unresolved state** | **Slot-causal reduction patterned on ADR 0016** | **Versioned declared schema** | **Recommend** |

## Subject identity, rename, move, and absence

An annotation target is not a URL, resource name, branch, issue number, or
display label. It is a source-qualified stable entity identity:

```text
SubjectId {
    authority: "github.com",
    kind: "Issue",
    uid: "I_kwDORel_088...",
}
```

For an internal resource, `uid` is the rename-stable UID to be ruled by
[#1039](https://github.com/flotilla-org/flotilla/issues/1039). For an external
entity, the provider adapter maps the source's native stable identifier into
the same envelope. A human-readable locator is stored separately:

```text
locator_hint: "flotilla-org/flotilla#1089"
```

The hint makes an orphaned statement understandable and debuggable. It is never
used to retarget automatically.

The resolution rules should be:

- **Rename with the same UID:** the statement follows automatically; the
  current locator is projected from the live entity.
- **Move represented by the same logical UID:** likewise.
- **Migration that changes native identity:**
  [#1039](https://github.com/flotilla-org/flotilla/issues/1039) must emit an explicit
  continuity/alias fact. A resolver may then present the successor, retaining
  both old and new identities in provenance.
- **Delete and recreate under the same name:** the new UID is a different
  subject. No annotation moves by name.
- **Unavailable/offline target:** the statement remains valid and queryable.
  Resolution status is `unavailable`, with the last-known locator and optional
  subject snapshot shown honestly.
- **Permanently retired target:** retain the statement and tombstone/retirement
  fact for audit. Whether a schema's effective view follows an explicit
  successor is schema policy; commentary about the historical occurrence
  should default to staying put.

An annotation target is deliberately not an `ownerReference`: annotations must
survive target absence and must not participate in target garbage collection.

The statement may record a small `subject_basis`—for example target
resourceVersion, forge `updated_at`, commit ID, or display snapshot—to answer
"what did the author see?" This is non-authoritative evidence, analogous to Web
Annotation `TimeState`, not a duplicate source record.

## Multi-writer semantics

The layer should not claim one universal merge operation. It should make the
operation part of the schema.

### Replication

Every statement is authored into the writer root's own log. It has an ordinary
resource UID; its authoring metadata carries an origin and monotonic dot. Roots
union statements through ADR 0016's durable replicas and relay. No root writes
a merged result back into an origin log. A semantic retraction references the
statement or logical slot it retracts and is itself immutable. It is not a
resource-store delete/tombstone of the asserted statement.

At admission, the annotation layer asks the local merged read path for the
frontier it has seen for `(subject, schema, slot)`, and the local store stamps
that causal basis. The client never fabricates a vector. This is **new
slot-level admission semantics**, patterned on but not supplied by ADR 0016's
record-field stamping. A concurrent assert and retract therefore remain
siblings until a causally later statement resolves them.

This makes statement collection convergent without deciding what the statements
mean. Meaning is the projection's job.

### Schema reduction classes

The first schemas need only a small set of declared reducers:

| Reduction class | Examples | Effective view |
|---|---|---|
| `Accumulate` | Notes, evidence, review remarks | Retain every non-retracted statement, ordered for display without treating time as authority |
| `AddRemoveSet` | Tags or memberships with explicit add/remove | Add/remove by stable element identity; concurrent independent additions survive |
| `PerAuthorRegister` | Agent assessment, reviewer vote | Latest causal value per author; show all authors |
| `ExclusiveRegister` | One human triage disposition, one board-scoped override | Causally later write supersedes; concurrent maximal values surface as conflict |
| `Derived` | Needs-rebase, readiness, reverse index | Not an annotation schema at all; compute from other facts |

Wall-clock time is display metadata only. An `ExclusiveRegister` applies ADR
0016's dot and seen-vector semantics to a logical annotation slot. A later write
made after seeing all siblings resolves them; concurrent human and agent
judgments remain visible. This reducer is new even though its causal rule is
not. Do not bury disagreement under root precedence or last timestamp.

Independent agents should normally write distinct per-author slots. A policy
projection may prefer a human principal, a designated governor, or a newer
observation, but it must preserve and expose the suppressed provenance.
Authority selection is a declared policy, not a mutation of the underlying
statements.

Collaborative editing of one large note body is a different problem. If it
eventually needs character-level offline merge, make that body a document kind
with an explicit CRDT. Do not force every annotation through a text CRDT.

## Proposed resource and schema contract

The exact Rust shape belongs in a design issue, but research is concrete enough
to require these semantics:

```text
AnnotationStatement.spec
├── subject
│   ├── authority
│   ├── kind
│   ├── uid
│   └── locator_hint?       # display/debug only
├── schema
│   ├── name               # e.g. flotilla.work/TriageDecision
│   └── version
├── slot?                   # schema-defined logical cardinality
├── author                  # Principal or persistent-agent identity
├── operation               # assert(body) | retract(statement/slot)
├── body                    # typed according to the admitted schema
└── subject_basis?          # non-authoritative version/snapshot evidence
```

The resource metadata supplies statement UID, origin root, and creation time.
Annotation admission supplies the slot dot/seen-vector described above.
Clients do not fabricate causal metadata. Semantic history is the immutable
assert/retract statement set, not resource tombstone history.

An annotation schema declaration owns:

- stable name and version;
- allowed subject kinds;
- typed body and validation;
- slot/cardinality rules;
- reducer and conflict behavior;
- queryable/indexed fields;
- redaction/access class;
- total-fallback label and readable serialization;
- upgrade/upcast rules.

Namespacing prevents collisions but is not a schema by itself. A payload that
only says `key: flotilla.work/foo, value: <arbitrary JSON>` should be rejected
for durable annotations. Experimental presentation decoration can continue to
use ADR 0018's namespaced annotation-tier facts, with a promotion path, but it
must not silently become durable domain truth.

The readable artifact matters. Raw inspection should always show:

```text
<schema> about <kind> <current-or-last-known-locator>
asserted by <principal> on <origin> at <time>
<schema-owned human rendering of the body>
resolution: current | unavailable | retired | conflicted
```

Unknown schema versions use the declaration's fallback field list and preserve
the original body; they never disappear from a surface.

## Which candidate facts are statements and which are views?

| Candidate | Source fact | View |
|---|---|---|
| Convoy's project association | Constitutive context stamped on the Convoy at admission, per [#1037](https://github.com/flotilla-org/flotilla/issues/1037) | "Appears in project" and project → convoys |
| Convoy's issue association | Constitutive provenance stamped on the Convoy at admission, per [#1089](https://github.com/flotilla-org/flotilla/issues/1089)'s starting position | Issue → convoys reverse index |
| Agent-authored note | `Note` annotation statement | Notes-by-subject, unread-note badge, summary |
| Human triage decision | `TriageDecision` statement, if the foreign issue owner would not accept it as issue state | Effective triage state after policy/conflict reduction |
| Mechanical triage readiness | Observed forge/resource facts and declared readiness rule | Ready / needs-info / stale queue |
| Manual board override | Board-scoped annotation statement with explicit author and reason | Effective lane |
| Rule-defined board status | Underlying lifecycle, dependency, attention, and settlement facts | Board lane, counts, ordering |
| "PR needs a rebase" | Observed base/head graph plus mergeability assessment, freshness-stamped | Change readiness / attention row |
| Rebase waiver or explanation | Annotation statement | Decorated readiness result |
| DAG dependency that governs execution | Constitutive workflow/convoy dependency fact | DAG board nodes, edges, critical path |
| Hypothesized or preference-grade dependency | Typed annotation statement | Optional planning overlay |
| Convoy ↔ issue reverse lookup | Nothing new | Join over Convoy admission provenance |

### Triage state

"Triage state" is too coarse to classify as one stored field. Decisions such as
`wontfix`, explicit ownership, priority, or "waiting for Robert's answer" are
authored judgments and must survive a source refresh. They are statements.
Mechanical consequences such as "ready because the required evidence exists
and no blocking demand remains" are views. The schema should store the smallest
irreducible decision, not cache the state-machine node that follows from it.

If the issue tracker's owner accepts a decision and Flotilla writes it back as a
native label/type, that external issue becomes the source of truth and Flotilla
observes it. A Flotilla-only dissent, note, or provisional classification remains
an annotation.

### Board status

A board is usually a materialized view, not a resource containing copied cards.
Its durable inputs are the entities, constitutive associations, dependency
facts, and irreducible human overrides. Grouping, reverse membership, counts,
ordering derived from priority, and lane selection derived from lifecycle are
query logic.

A hand-drag into "Later" that has no derivation is real intent. Store it as a
board-scoped statement, not as a silent mutation of a disposable view. This is
the same distinction GitHub Projects exposes between linked content and
project-owned field values, with stricter provenance and merge semantics.

### Needs-rebase

"Needs a rebase" expires when either branch moves and should include an `as_of`
basis. It is an observed/derived assessment, not durable commentary. An agent's
explanation, a human waiver, or "do not rebase; upstream is about to merge" is
an annotation joined into the readiness view.

### Reverse indexes

Reverse indexes are always views. The forward fact belongs where it is
constitutive: Convoy → issue/project provenance, annotation → subject,
dependency → prerequisites. Writing the inverse creates a second authority and
eventually a repair job. It may be persistently materialized for performance,
but remains disposable and application-read-only.

## Rejected alternatives

### Put `metadata.annotations` on every resource

This inherits Kubernetes' target lifetime, whole-object concurrency, string
payloads, and inability to annotate an absent foreign entity. It also mutates
observed resources contrary to ADR 0004.

### Create one mutable `AnnotationsForTarget` document

This turns every writer into a concurrent editor of one hot object, creates
whole-document conflicts, and makes retention/retraction provenance difficult.
Independent immutable statements give each writer an append path and let a
schema decide the read model.

### Create a dedicated annotation database or protocol

It would need identity, versioning, optimistic concurrency, watch, offline
replication, relay, administrative tombstones, and conflict UI—the transport
and storage substrate the resource store and ADR 0016 already own. Annotation
slots add reduction semantics, not another wire or database.

### Treat annotations only as projection decoration

Projection-only facts disappear when the producer or target is absent and
cannot preserve a human decision. ADR 0018's ephemeral annotation-tier
presentation facts are appropriate for experimental decoration, not durable
authored statements.

### Use RDF/JSON-LD as the internal graph

RDF supplies global identity and graph union but makes vocabulary and meaning
open by default. Adding SHACL validates shapes; it does not supply Flotilla's
domain ownership, reduction, conflicts, readable artifacts, or typed Rust
protocol. It solves a broader interoperability problem at the cost of weakening
the local contract.

### Store every board and projection durably

Materialized views may be persisted as caches when serving cost requires it.
Making them authoritative duplicates source facts and makes rule changes into
data migrations. A view must remain rebuildable and application-read-only.

## Consequences and open decisions

### Consequences of the recommendation

- Annotation statements join the definitions class and replicate across roots.
- The Aggregator gains curated typed joins such as annotations-by-subject and
  effective-triage; it does not gain a generic predicate query language.
- Target resolution tolerates missing subjects and exposes honest resolution
  state rather than dropping rows.
- Raw resource inspection remains possible even before a polished annotation
  surface exists.
- Annotations about external issue/PR data survive provider outages without
  persisting a shadow copy of the external entity.
- Conflicts become a first-class query/result condition and UI affordance,
  extending ADR 0016's definitions conflict path with slot-level siblings.

### Decisions still needed

1. **Stable identity
   ([#1039](https://github.com/flotilla-org/flotilla/issues/1039)).** Define
   rename/move continuity for internal
   Project, Repository, Convoy, and other logical subjects, plus the external
   provider mapping contract.
2. **Authored mastering
   ([#1090](https://github.com/flotilla-org/flotilla/issues/1090)).** The companion
   [issue #1090](https://github.com/flotilla-org/flotilla/issues/1090) may amend
   which authored kinds use ADR 0016 definitions semantics. Annotations should
   not invent a temporary master while that is open.
3. **Schema registry ownership.** Decide whether definitions are compiled into
   Rust first, declared through the future meta-model, or both. The acceptance
   bar above should remain invariant.
4. **Principal identity and authority.** Human, persistent-agent, workflow, and
   controller authors need stable identities and explicit policy precedence.
5. **Access and redaction.** Notes may contain private reasoning or credentials.
   Schema and resource ACLs must classify visibility before fleet replication.
6. **Retention and compaction.** Definitions-class permanence is safe for
   low-volume judgments. High-churn machine assessments should remain observed
   facts or use compaction below the fleet-wide causal frontier, not accumulate
   forever as annotations.
7. **External write-back.** Define when a Flotilla statement is promoted to a
   forge-native label/comment and how the resulting observed fact supersedes or
   links back to the proposal.

## Suggested first experiment

Do not begin with a generic annotation editor. Exercise the contract with three
schemas whose reducers differ:

1. `Note/v1` — accumulate Markdown notes from humans and persistent agents.
2. `TriageDecision/v1` — an exclusive register that visibly conflicts under
   concurrent offline decisions.
3. `RebaseWaiver/v1` — authored commentary joined with an ephemeral
   `needs_rebase` assessment.

Use one internal subject and one source-qualified GitHub issue/PR subject. Test:

- target present, renamed, unavailable, deleted/recreated, and explicitly
  migrated;
- two roots authoring offline then converging;
- a causal overwrite versus a concurrent conflict;
- retraction without erasing audit history;
- full projection rebuild from resource logs;
- raw fallback rendering with an unknown schema version.

The experiment should end in an ADR only after
[#1039](https://github.com/flotilla-org/flotilla/issues/1039) and
[#1090](https://github.com/flotilla-org/flotilla/issues/1090) rule their shared
identity and authored-resource substrate. The research recommendation does not
require another event transport or a general-purpose materialized-view engine.

## Final answer

Build the annotation layer as **typed, immutable statement resources over the
existing resource log**. Let stable UIDs and explicit continuity facts carry
statements through rename and move; tolerate unresolved targets and retain
last-known locator/version evidence while they are offline. Merge origin facts
by union and reduce them under declared schema semantics, applying ADR 0016's
causal model in a new annotation-slot reducer that exposes rather than erases
concurrent exclusive judgments.

Store only irreducible authored intent and commentary. Derive effective triage,
board lanes, needs-rebase, annotation indexes, and convoy↔issue reverse lookups
as curated typed views. Persist those views only as disposable serving caches.
This gives Flotilla the useful part of Git notes, Fossil, Gerrit, and Web
Annotation without importing their blob, timestamp, central-master, or
open-vocabulary failure modes.
