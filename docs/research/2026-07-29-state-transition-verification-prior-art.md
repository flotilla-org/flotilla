# State-transition verification prior art

**Date:** 2026-07-29
**Status:** complete (research; no code changes)

Start at [§0 Local baseline](#0-local-baseline-what-we-already-have) for what our
harness already provides, or jump to [Synthesis](#synthesis) for the
recommendation.

## Motivating bug classes

A week of dogfooding the flotilla control plane produced three classes of
state-transition bug:

1. **Multi-writer fields with no declared owner.** An operator-applied spec field
   silently stomped by a periodic registration upsert.
2. **Restart/recovery paths that don't commute with in-flight lifecycle states.**
   Recovery resurrecting deleted records, releasing leases held by mid-deletion
   holders, dropping capabilities on restart.
3. **Reads against the wrong store/authority** in a federated multi-store setup.

Question: how do you *prove* these can't happen, and is the proof machinery
per-resource or generic (properties as folds over the event log / over a generic
transition-system structure)?

---

## 0. Local baseline (what we already have)

Read first, so the recommendations attach to real code rather than to a generic
control plane.

**The reconcile function is already pure.** `Reconciler` in
`crates/flotilla-resources/src/controller/mod.rs` splits into an async
`fetch_dependencies(&obj) -> Deps` and a *synchronous, total* 

```rust
fn reconcile(&self, obj: &ResourceObject<Self::Resource>, deps: &Self::Dependencies, now: DateTime<Utc>)
    -> ReconcileOutcome<Self::Resource>;
```

`ReconcileOutcome` carries `patch: Option<StatusPatch>`, `actuations: Vec<Actuation>`,
`events: Vec<Event>`, `requeue_after: Option<Duration>`. Effects are *returned as
data*, not performed. This is the single most important local fact in this
document: it is precisely the shape Anvil's `Controller::step()` requires (§A.3)
and precisely the "Stateright must have visibility of every input and output"
precondition (§B). Most codebases wanting the techniques below have to earn this
seam; we already have it.

**`ObjectMeta`** (`crates/flotilla-resources/src/resource.rs:207`) carries `name`,
`namespace`, `resource_version`, `labels`, `annotations`, `owner_references`,
`finalizers`, `deletion_timestamp`, `creation_timestamp`, and an optional
`merge: MergeMetadata`. Notably **absent: `generation` and `observedGeneration`**,
and absent: any per-field manager record. `ObjectMeta::is_pending_finalization()`
= `!finalizers.is_empty() && deletion_timestamp.is_some()`.

**Ownership today is object-scoped, not field-scoped.**
`LifecycleAuthority` (`crates/flotilla-protocol/src/lifecycle.rs:7`) is
`Observed | Adopted | Managed`, stored as the label `flotilla.work/authority`.
That answers "may this control plane write this object at all", not "who owns
this field".

**`MergeMetadata` is causal, not managerial.**
`MergeMetadata { fields: BTreeMap<String, FieldMergeMetadata>, seen, conflicts }`
with `FieldMergeMetadata { dot: CausalDot, seen, written_at }` and
`MergeConflictSibling`. This is a per-field multi-value register for *cross-host*
definitions replication — it records which **node** last wrote a field and
surfaces causally-concurrent siblings. It does **not** record which **writer
role** (operator vs. registration upsert) owns a field, and two writers on the
same host produce no dot conflict at all. This distinction is the crux of §A.1
and the synthesis: we have SSA-shaped *storage* with none of the SSA *ownership
semantics*.

**Writes are optimistic-concurrency read-modify-write.**
`apply_status_patch` / `apply_status_patch_checked`
(`crates/flotilla-resources/src/status_patch.rs`) loop up to `MAX_RETRIES = 3`:
get → `StatusPatch::apply(&mut status)` → `update_status(name, resource_version, …)`,
retrying on `ResourceError::Conflict`. `apply_status_patch_checked` re-runs the
caller's `check` after every conflict, which is the right shape for
state-dependent guards. Note this path covers **status only**; spec writes
(including periodic registration upserts) go through other paths, which is where
bug class 1 landed.

**Deletion flow already follows the kubebuilder shape**
(`controller/mod.rs:496-532`): add the finalizer only when
`deletion_timestamp.is_none()`; when `deletion_timestamp.is_some()` and our
finalizer is present, `run_finalizer()`, then remove the finalizer; a
`finalizer_error_patch` hook maps finalizer failure to a status transition rather
than a wedge.

**The store is event-sourced and compacted.** `crates/flotilla-resources/src/sqlite.rs`
defines `resource_events(group_name, version, kind, namespace, event_version,
event_type, body_json)` keyed by a monotone per-(kind, namespace) `event_version`,
plus `resource_event_compaction(compacted_through)`. Safety-as-a-fold over this
log is directly available — with the caveat that **compaction truncates the
prefix**, so any fold whose accumulator is not reconstructible from a snapshot
plus the retained suffix is unsound in production (fine in tests, where nothing
is compacted).

**Federation is explicit in the schema.** `replica_objects`, `replica_cursors`,
`replica_tombstones` are all keyed by `origin_root`, and
`ResourceProvenance` (`crates/flotilla-resources/src/replica.rs:23`) is
`Local | Replica { origin_root: NodeId, last_synced_at }`, with
`ReplicationClass { None, Definitions, HomeBoundRuntime }` declared per resource
type. Bug class 3 is therefore *typed but unenforced*: the authority of a read is
derivable from `ReplicationClass` + `ResourceProvenance`, but nothing checks that
a given read resolved against the right one.

**The liveness harness is already a generic engine + per-resource enrollment.**
`crates/flotilla-resources/src/test_support/liveness.rs` defines the traits
`WorldBuilder` (`build(scenario) -> World`), `ReconcileStep<W>`
(`reconcile_step`, `apply_patch`, `apply_actuation`), and `FixpointPredicate<W>`
(`at_fixpoint`, `write_count`, `reset_write_count`, `held`, `probe_count`);
`LivenessEnrollment` bundles those with an `Arc<VirtualClock>`, `staleness_edges:
Vec<Duration>`, and `pass_bound` (default 10). The assertion battery is:

| Assertion | Property it encodes |
|---|---|
| `assert_bounded_convergence` | reaches a fixpoint within `pass_bound` passes |
| `assert_quiescence_at_fixpoint` | a pass at the fixpoint writes nothing and emits no actuation |
| `assert_staleness_edges` | crossing each TTL re-probes external state |
| `assert_actuation_drop_recovery` | a dropped actuation is re-emitted next pass |
| `assert_degradation_not_wedging` | a contradictory world becomes `held`, never a false success |

`LivenessScenario` is a closed enum of three fixtures: `Normal`, `ActuationDrop`,
`Contradictory`. Enrollments live in
`crates/flotilla-controllers/tests/liveness_contract.rs`;
`WriteCountingBackend` (`test_support/write_counting.rs`) decorates the backend so
"a fixpoint pass wrote" is observable at the store, not just at the reconciler.

Two structural observations that drive the synthesis:

1. **Scenarios are hand-enumerated and fixed.** There are exactly three worlds and
   a straight-line pass loop. There is no *sequencing* — no interleaving of
   external mutation with reconcile passes, no restart, no deletion mid-flight.
   Every one of our three bug classes lives in sequencing, not in the single-pass
   behaviour the harness currently exercises.
2. **The properties are already generic and the fixtures already per-resource.**
   The architecture the literature converges on (§C.5) is the one we have. The
   gap is in *what* is enumerated, not in *how* it is factored.

## A. Kubernetes prior art

### A.1 Server-side apply and managedFields

#### How ownership is declared and tracked

Field ownership lives in `.metadata.managedFields`, an array of
`ManagedFieldsEntry`
([reference](https://kubernetes.io/docs/reference/using-api/server-side-apply/)):

```yaml
managedFields:
- manager: kubectl                 # opaque writer identity
  operation: Apply                 # "Apply" | "Update"
  apiVersion: v1
  time: "2010-10-10T0:00:00Z"
  fieldsType: FieldsV1
  fieldsV1:                        # a set of field PATHS, never values
    f:metadata:
      f:labels:
        f:test-label: {}
  subresource: status              # empty for the main resource
```

`operation: Apply` means the change came from a Server-Side Apply patch
(`Content-Type: application/apply-patch+yaml`); `operation: Update` means HTTP
`PUT` or a non-apply `PATCH`. Apply requests **must** carry a `fieldManager`
query parameter; Update requests may omit it. The docs warn: "The
`.metadata.managedFields` field is managed by the API server. You should avoid
updating it manually."

Note the storage model: `fieldsV1` records **paths only**. Ownership is a set of
leaves; the live object holds the values. You cannot recover "what did manager X
intend?" from the object — a limitation cluster-api hit directly (below). This is
the same shape as our `MergeMetadata.fields`, which likewise stores per-path
metadata (a causal dot) rather than per-writer intent.

The merge engine is [kubernetes-sigs/structured-merge-diff](https://github.com/kubernetes-sigs/structured-merge-diff),
whose README frames the distinction: PUT/PATCH means "make the object exactly
like X" with ownership *inferred from the diff*; APPLY means "these fields I
manage should look exactly like this", with explicit management and automatic
deletion of previously-managed fields no longer mentioned. The load-bearing
invariant: **"Any time a manager begins managing some new field, that field is
removed from all other managers."**

Granularity is schema-driven, via markers
([merge strategy table](https://kubernetes.io/docs/reference/using-api/server-side-apply/#merge-strategy)):
`x-kubernetes-list-type` (`atomic` / `set` / `map`), `x-kubernetes-list-map-keys`,
`x-kubernetes-map-type` and `x-kubernetes-struct-type` (`atomic` / `granular`).
**Defaults matter enormously**: lists default to `atomic` — one manager owns the
whole list. For CRDs without markers the
[Google OSS blog by the SSA authors](https://opensource.googleblog.com/2021/10/server-side-apply-in-kubernetes.html)
states the deduced behaviour: "Keys are treated as fields in a struct and lists
are assumed to be atomic." A CRD author who does not annotate gets coarse,
whole-list ownership, which is the most common source of surprise conflicts.

Schema evolution is asymmetrically dangerous (docs, "Compatibility across
topology changes"): map/set/granular → atomic causes ownership to *inflate*
("the whole list, map, or struct… will end-up being owned by actors who owned an
element"); atomic → map/set/granular causes it to *evaporate* silently ("the API
server is unable to infer the new ownership… no conflict will be produced").

#### Conflict rules

A conflict "occurs when an `Apply` operation tries to change a field that another
manager also claims to manage", surfaced as HTTP **409 Conflict**. Three
documented resolutions:

- **Force** (`force=true`, `kubectl --force-conflicts`) — "forces the operation to
  succeed, changes the value of the field, **and removes the field from all other
  managers' entries in `managedFields`**". Force is a *takeover*, not a
  suppression.
- **Yield** — remove the field from your config and reapply.
- **Share** — set your value equal to the live value; both managers co-own, and
  any subsequent change by either conflicts.

Ownership transfer: "Whenever a field's value does change, ownership moves from
its current manager to the manager making the change." Field removal is
conditional: "If you remove a field from a manifest and apply that manifest,
Server-Side Apply checks if there are any other field managers that also own the
field. If the field is not owned by any other field managers, it is either
deleted from the live object or reset to its default value." **That conditional
clause is the source of most SSA pathology.**

The Update path never fails this way: "if you make a change using **update** that
would affect a managed field, a conflict never provokes failure of the
operation."

#### Why this does not, by itself, solve class 1

Four independent reasons, each documented:

1. **Update writers stomp unconditionally.** [KEP-555](https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/555-server-side-apply/README.md)
   gives Update writers `managedFields` entries, but ownership never blocks them.
   The only concurrency control on that path is `resourceVersion` optimistic
   concurrency — a staleness check, not an ownership check. **This is exactly our
   situation**: `apply_status_patch` retries on `Conflict` and then *reapplies the
   patch to the newer state*, which is correct for a status accumulator and
   completely silent about a second writer's intent for the same field.
2. **Manager identity is unstable.** `fieldManager` is optional for Update; when
   absent the API server derives it from the User-Agent via `prefixFromUserAgent`
   ("the characters preceding the first `/`"; see
   [apiserver patch.go](https://github.com/kubernetes/apiserver/blob/master/pkg/endpoints/handlers/patch.go),
   original PR [#74760](https://github.com/kubernetes/kubernetes/pull/74760)).
   Change your User-Agent and you become a new manager. Additionally,
   `internal/capmanagers.go` merges the oldest Update entries once they exceed
   `maxUpdateManagers`, renaming the bucket **`ancient-changes`**; and
   `internal/skipnonapplied.go` synthesizes an Update entry named
   **`before-first-apply`** claiming everything already on the object on first
   SSA — the most-reported papercut,
   [#89954](https://github.com/kubernetes/kubernetes/issues/89954), closed by
   lifecycle rot rather than fix.
3. **Official guidance tells controllers to disable the mechanism.** The docs:
   "It is strongly recommended for controllers to always force conflicts on
   objects that they own and manage, since they might not be able to resolve or
   act on these conflicts." The Kubernetes blog
   ["Advanced Server Side Apply"](https://kubernetes.io/blog/2022/10/20/advanced-server-side-apply/)
   is blunter: "**You should probably force conflicts when using SSA**… Your
   controller probably doesn't know what to do when some other entity in the
   system has a different desire than your controller about a particular field."
4. **Per-field ownership does not align with per-invariant validity.**
   [#108081](https://github.com/kubernetes/kubernetes/issues/108081) (cert-manager
   v1.6→v1.7): `cainjector` owned `crd.spec.conversion.webhook.clientConfig.caBundle`;
   v1.7 removed the conversion webhook; SSA could not remove the field (owned
   elsewhere) and `conversion.strategy: None` *plus* a set `caBundle` is invalid,
   so the upgrade hard-failed with no manager able to repair it. Maintainers
   raised whether components "should take ownership of *all* fields that are
   validated together". Resolution required hand-patching `managedFields`.

Other documented limits: `managedFields` bloat counting against the etcd 1.5 MiB
request limit (KEP-555's own Risks section concedes "objects grow substantially,
impacting memory, network bandwidth, and controller caches";
[#90066](https://github.com/kubernetes/kubernetes/issues/90066)); the CSA→SSA
migration leaving permanently un-removable fields because
`kubectl-client-side-apply` co-owns them as an Update
([#99003](https://github.com/kubernetes/kubernetes/issues/99003)); irrecoverable
multi-manager states ([#131476](https://github.com/kubernetes/kubernetes/issues/131476),
umbrella [#73723](https://github.com/kubernetes/kubernetes/issues/73723)); and
subresource gaps — the docs state outright, "Server-Side Apply does not correctly
track ownership on sub-resources that don't receive the resource object type", and
see [#133704](https://github.com/kubernetes/kubernetes/issues/133704)
("managedFields got wiped out by writing status to different version"). In
practice almost all controllers write status via `UpdateStatus` (an Update), so
**SSA disciplines spec among appliers and effectively does not discipline status
at all**.

#### The cluster-api field report

[cluster-api's in-place-propagation proposal](https://github.com/kubernetes-sigs/cluster-api/blob/main/docs/proposals/20221003-In-place-propagation-of-Kubernetes-objects-only-changes.md)
is the most honest write-up. Their problem is co-authored maps (labels and
annotations written by cluster-api, users, and third parties) where deletion
requires knowing who set the entry. Their enumerated alternatives: (1) become
authoritative and reject third-party writes; (2) never delete; (3) track
ownership in a status field — **rejected specifically because status "doesn't
survive cluster moves or backup/restore operations"**. They chose SSA on the
grounds that "using API server built-in capabilities is a stronger, long term
solution", while warning "this change requires a lot of testing and validation".
See also [cluster-api#6736](https://github.com/kubernetes-sigs/cluster-api/issues/6736),
"Topology controller should not take ownership of `cluster.spec.topology`" — the
canonical "my controller accidentally became owner of the user's intent" bug,
which is our class 1 verbatim.

Alternative (3) is worth flagging for us: we would have the same durability
objection, since replicated definitions cross hosts.

#### Assessment

**SSA is a coordination protocol for cooperative appliers, not an enforcement
mechanism.** It is advisory locking whose reference guidance tells the strongest
party to break the lock. Its real guarantees are narrow: the mechanical
"new manager ⇒ removed from all others" invariant, an audit trail of who last
touched what, and — the undisputed operational win — elimination of the `0.5N²`
optimistic-concurrency thrash when N actors edit disjoint fields (the blog's
example: 10 concurrent GET-modify-PUT actors cost up to 50 GET/PUT attempts;
under SSA, disjoint changes all land in any order).

The transferable lesson for flotilla is not "adopt SSA". It is: **the value is in
the declaration, and the declaration must be enforced at the write path with a
loud failure**, because k8s's own experience is that an advisory mechanism plus a
`force` escape hatch degrades to "everyone forces".

### A.2 API conventions: phase vs conditions, observedGeneration, finalizers

Source throughout:
[api-conventions.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-architecture/api-conventions.md).

#### Why `phase` was abandoned — the exact wording

> "Some resources in the v1 API contain fields called `phase`, and associated
> `message`, `reason`, and other status fields. The pattern of using `phase` is
> **deprecated**. Newer API types should use conditions instead. **Phase was
> essentially a state-machine enumeration field, that contradicted system-design
> principles and hampered evolution, since adding new enum values breaks backward
> compatibility.** Rather than encouraging clients to infer implicit properties
> from phases, we prefer to explicitly expose the individual conditions that
> clients need to monitor."

Two distinct arguments are bundled there, and they should be separated before
importing the conclusion:

1. **Compatibility.** A closed enum cannot be extended without breaking clients
   that switch exhaustively. Conditions are an open set.
2. **Inference.** Phase forces clients to infer properties; conditions expose
   them, and permit a *cross-type vocabulary* (`Ready`, `Available`) that
   per-type phase enums cannot.

And the load-bearing claim about what conditions are:

> "conditions are observations and not, themselves, state machines, nor do we
> define comprehensive state machines for objects."

Elaborated: "Conditions may oscillate or be monotonic depending on resource and
type… the system is **level-based rather than edge-triggered**, assuming an **Open
World**." Supporting rules: apply conditions on first visit even as `Unknown`, so
other components know reconciliation is progressing; "the absence of a condition
should be interpreted as `Unknown`"; name them for current observed state
(adjectives `Ready`, past-tense verbs `Succeeded`) and **not** present-tense verbs
(`Deploying`) — intermediate states use `Unknown` status.

**Caveat for us.** This is a *published-API* argument, not a general
state-modelling one. It says: do not force external clients to switch on a closed
lifecycle enum. It does **not** say "do not have a state machine" — Kubernetes
controllers plainly do, they just decline to publish it as an enum. Our `Phase`
enums (`CheckoutPhase`, `ConvoyPhase`, `WorkPhase`) are in a
no-backwards-compatibility phase with a single first-party consumer, so the
compatibility argument does not currently bite. What *does* transfer is the
second argument: readers that need "is it usable?" should get a condition, not a
phase comparison. And §C.4 argues the phase enum is worth keeping precisely
because a *declared, introspectable* transition table is the artefact you fold
over.

The canonical conditions list shape ties straight back to §A.1:

```go
// +listType=map
// +listMapKey=type
Conditions []metav1.Condition
```

`+listType=map +listMapKey=type` is what allows different managers to own
different conditions. Without those markers the list is `atomic` and
single-owner.

#### `metav1.Condition` and `SetStatusCondition`

[KEP-1623](https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/1623-standardize-conditions/README.md)
standardized `Type`, `Status` (True/False/Unknown), `ObservedGeneration`,
`LastTransitionTime`, `Reason`, `Message`. Design rationale worth quoting:

- `reason` required and non-empty: "the actor setting the value should always
  describe why the condition is the way it is, even if that value is 'unknown
  unknowns'. **No other actor has the information to make a better choice.**"
- `lastHeartbeatTime` was removed: "this field caused excessive write loads as we
  scaled." Directly relevant to any liveness-in-status design of ours.

[`meta.SetStatusCondition`](https://github.com/kubernetes/apimachinery/blob/master/pkg/api/meta/conditions.go)
has one behaviour that matters: on an existing condition of the same type,
**`LastTransitionTime` is refreshed only when `Status` changes**. Changes to
`Reason`/`Message`/`ObservedGeneration` leave it alone. That is what makes it a
genuine transition marker rather than a write timestamp — and a hand-rolled
setter that stamps `now` unconditionally destroys "how long has this been broken"
*and* breaks quiescence (every pass becomes a write). Our
`assert_quiescence_at_fixpoint` is precisely the assertion that catches this
class, which is a point in the harness's favour.

#### `generation` / `observedGeneration`

Conventions: "**generation**: a sequence number representing a specific generation
of the desired state. Set by the system and monotonically increasing,
per-resource… it may be compared for RAW and WAW consistency."
"**observedGeneration** … is the `generation` most recently observed by the
component responsible for acting upon changes to the desired state. This can
ensure reported status reflects the most recent desired status."

The bump rule for CRDs is generic and visible in
[apiextensions-apiserver strategy.go](https://github.com/kubernetes/apiextensions-apiserver/blob/master/pkg/registry/customresource/strategy.go):
if the `/status` subresource is installed, status is copied over from the old
object on a main-resource update, and **any non-`metadata` change increments
generation**. Two consequences: without a status subresource, status writes bump
generation and `observedGeneration` is meaningless; and metadata-only changes
(labels, annotations, finalizers) do **not** bump it. One more bump site:
`BeforeDelete` in
[apiserver delete.go](https://github.com/kubernetes/apiserver/blob/master/pkg/registry/rest/delete.go)
bumps generation when it initiates graceful deletion, so an advancing generation
does not strictly mean "spec changed" — it can mean "deletion started".

**We have no `generation` field.** We have `resource_version`, which bumps on
*any* write including status, so it cannot serve the same role. The idiom
`status.observedGeneration != metadata.generation ⇒ my status is stale, do not
trust it` is currently inexpressible in our model. This is worth noting as a gap
independent of any verification work: it is the standard way a *reader* detects
that a controller has not yet caught up, and several of our "did the reconciler
act on the new spec or the old one?" questions reduce to it.

#### Finalizers and the deletion flow

[Using Finalizers](https://kubernetes.io/docs/concepts/overview/working-with-objects/finalizers/):
a DELETE against an object with non-empty `finalizers` "marks the object for
deletion by populating `.metadata.deletionTimestamp`, and returns a `202` status
code". The object "remains in a terminating state" until controllers do their
work and remove their finalizers; "when the `metadata.finalizers` field is empty,
Kubernetes considers the deletion complete and deletes the object."

The rule you cannot violate is enforced in validation, not by a webhook —
[`ValidateNoNewFinalizers`](https://github.com/kubernetes/apimachinery/blob/master/pkg/api/validation/objectmeta.go):

```go
extra := sets.NewString(newFinalizers...).Difference(sets.NewString(oldFinalizers...))
if len(extra) != 0 {
    allErrs = append(allErrs, field.Forbidden(fldPath,
        fmt.Sprintf("no new finalizers can be added if the object is being deleted, found new finalizers %#v", extra.List())))
}
```

invoked from `ValidateObjectMetaUpdate` **only when `oldMeta.DeletionTimestamp != nil`**.
Alongside it: `deletionTimestamp` is immutable once set, `generation` must not be
decremented, and — the sentence to internalize — "**After the deletion is
requested, you can not resurrect this object. The only way is to delete it and
make a new similar object.**"

This is why the [kubebuilder finalizer pattern](https://book.kubebuilder.io/reference/using-finalizers)
puts `AddFinalizer` strictly inside the `DeletionTimestamp.IsZero()` branch — it
is not stylistic, it is the difference between working and a hard `Forbidden`.
Kubebuilder also stresses the cleanup "should be reentrant and safe for multiple
invocations". **Our `controller/mod.rs:496-532` already implements exactly this
shape.**

Note what the API server does *not* enforce: the object's `spec` remains mutable
after `deletionTimestamp` is set. Terminating is not read-only. Only the metadata
immutability set and the no-new-finalizers rule are hard.

Garbage collection ([docs](https://kubernetes.io/docs/concepts/architecture/garbage-collection/),
[design proposal](https://github.com/kubernetes/design-proposals-archive/blob/main/api-machinery/synchronous-garbage-collection.md)):
foreground deletion adds a `foregroundDeletion` finalizer and blocks on
dependents with `ownerReference.blockOwnerDeletion=true` *that are in the GC
controller's cache*; background (the default) deletes the owner immediately and
reaps dependents asynchronously; setting `blockOwnerDeletion=true` requires delete
permission on the owner (HTTP 422 otherwise), closing a documented security
loophole. Cross-namespace owner references are disallowed by design.

Two documented races bear directly on our class 2. The design doc calls
synchronous GC **best-effort**: "if the garbage collector observes the owner's
finalizer before observing all dependents' creation, it may remove the finalizer
prematurely." And in `processItem()`, GC "**treats owners with `DeletionTimestamp
!= nil && !Finalizers.Has(OrphanFinalizer)` as non-existent.**"

That second one is Kubernetes deliberately collapsing "terminating" into "gone"
to make progress. It is acceptable *only* because GC's job is destruction, where
over-eagerness is roughly idempotent. **For anything holding exclusive state it is
the anti-pattern, and it is our lease bug exactly.**

#### The lease-released-by-recovery failure mode: what k8s actually does

There is no single primitive. There are five idioms, and the sources are explicit
about what each does and does not guarantee.

**(a) The Lease object is not a fencing token.** The
[leaderelection package doc](https://github.com/kubernetes/client-go/blob/master/tools/leaderelection/leaderelection.go)
states outright: "This implementation does not guarantee that only one client is
acting as a leader (a.k.a. fencing)." It "relies on locally-captured timestamps
rather than trusting timestamps in the election record"; validity is
`observedTime + leaseDurationSeconds > now` where `observedTime` is local.
[`LeaseSpec`](https://github.com/kubernetes/api/blob/master/coordination/v1/types.go)
carries `holderIdentity`, `leaseDurationSeconds`, `acquireTime`, `renewTime`,
`leaseTransitions`, `strategy`, `preferredHolder`. The actual safety comes from
two places that are *not* the lease semantics: **optimistic concurrency on
`resourceVersion`** (two racing acquirers cannot both win the write) and
**`leaseTransitions` as a fencing-token-shaped monotone counter** you can carry
into the downstream effect. `renewTime` is evidence the holder *was* alive, never
proof it is dead.

**(b) The "at most one" doctrine.**
[Force-delete StatefulSet pods](https://kubernetes.io/docs/tasks/run-application/force-delete-stateful-set-pod/)
is the canonical statement: "StatefulSet ensures that, at any time, there is at
most one Pod with a given identity running in a cluster." Force deletion "**do[es]
not** wait for confirmation from the kubelet that the Pod has been terminated…
it will immediately free up the name from the apiserver. This would let the
StatefulSet controller create a replacement Pod with that same identity; this can
lead to the duplication of a still-running Pod… will violate the at most one
semantics." And the design principle: "A Pod is not deleted automatically when a
node is unreachable." **Kubernetes deliberately declines to convert *unreachable*
into *dead*.** Release requires positive confirmation from the holder's own agent,
node deletion by an admin, or an explicit human assertion — framed exactly that
way: "you are asserting that the Pod in question will never again make contact
with other Pods in the StatefulSet and its name can be safely freed up."

**(c) Finalizer on the holder as an ordering barrier.**
[KEP-2307](https://github.com/kubernetes/enhancements/blob/master/keps/sig-apps/2307-job-tracking-without-lingering-pods/README.md):
"The Job controller creates Pods with a finalizer to prevent finished Pods from
being removed by the garbage collector." The enforced order is (1) record the
Pod's terminal state into Job status, (2) remove the finalizer, (3) update
counters. The constant is
[`JobTrackingFinalizer = "batch.kubernetes.io/job-tracking"`](https://github.com/kubernetes/api/blob/master/batch/v1/types.go).
Generalized: **put a finalizer on the holder, keyed to the resource it holds**, so
release is *ordered before* the holder's disappearance rather than racing it. Same
shape as `kubernetes.io/pv-protection`.

**(d) Check `deletionTimestamp` before acquisition, not only before cleanup.**
The reclaimer must treat terminating-but-present as *held*, and must verify
`holderIdentity` still matches what it expects before releasing.

**(e) Make forcible reclamation an explicit assertion**, not a timeout.

Synthesized as a rule we can adopt directly: **`deletion_timestamp.is_some()`
never means "gone" for a resource-holding object.** `is_pending_finalization()`
already exists on our `ObjectMeta`; the bug was not the absence of the predicate
but the absence of an obligation to consult it on the recovery path.

#### Cross-cutting observation

The two halves of §A.1 and §A.2 connect through one theme: **Kubernetes
consistently declines to build enforcement and builds legibility instead.** SSA
does not stop stomping, it records who stomped. Conditions do not encode a state
machine, they record observations. `observedGeneration` does not block stale
action, it lets a reader detect staleness. Finalizers do not prevent deletion,
they convert deletion into an ordered, observable update. The only hard-coded
enforcement is remarkably narrow: `ValidateNoNewFinalizers` plus the metadata
immutability set.

That is a defensible choice for a platform with unbounded third-party
controllers. It is *not* obviously the right choice for a control plane where we
own every writer — which is the strongest argument in this document for
enforcement over convention on our side.

### A.3 How the ecosystem tests controllers (Sieve, Acto, Anvil, TLA+, envtest)

#### Sieve (OSDI '22) — perturb the controller's view, diff the outcome

"Automatic Reliability Testing for Cluster Management Controllers", Sun, Luo, Gu,
Ganesan, Alagappan, Gasch, Suresh, Xu.
[Paper](https://tianyin.github.io/pub/sieve.pdf) ·
[USENIX](https://www.usenix.org/conference/osdi22/presentation/sun) ·
[repo](https://github.com/sieve-project/sieve).

The core insight is that controllers interact with cluster state through a
*narrow waist* of state-centric interfaces (read / write / notify over uniformly
schema'd objects), and that waist is simultaneously highly introspectable and a
perfect injection point. Sieve needs **no formal spec, no bug hypotheses, no
expert assertions** — only a manifest to build and deploy the controller, and 2-5
test workloads of 6-12 lines each.

Three perturbation patterns (§3.1):

- **Intermediate-state** — crash and restart the controller *between* two updates
  it issues within a single reconcile. Sieve reads a reference trace, finds every
  reconciliation issuing multiple updates U₁…Uₙ, and generates one plan per Uᵢ.
  Fig. 4: the RabbitMQ operator resizes a volume with two updates
  (`VolCur`→15GB, then `VolReq`→15GB); crashing between them leaves
  `VolCur=15GB, VolReq=10GB`, and the guard `if Desired > Current` no longer
  fires — the resize never completes. Fig. 1 is the same shape in a Cassandra
  operator: crash between `Delete(pod)` and the `Finalizing` phase → stuck
  forever, storage leaked.
- **Stale-state ("time travel")** — stand up **two API servers**, let one lag, and
  reconnect the controller to the stale one. Motivation quoted: "Time traveling
  occurs when there are multiple API servers operating in a high-availability
  setup, when the controller reconnects to a stale API server that has not yet
  seen updates to the cluster state." Generation uses a *conflict rule*: find a
  notification/update pair (N,U), then a later N′ whose effect conflicts with U
  (U deletes an object, N′ creates it). Fig. 5: the Percona MongoDB operator
  time-travels, sees a `DeletionTS` belonging to an **already-deleted** cluster,
  and deletes all pods and volumes of the **newly created** one. Root cause:
  matching clusters by *name* instead of *UID*.
- **Unobserved-state** — pause the informer→reconciler handoff so the controller
  *misses* a transient state. Every bug found this way was latent edge-triggering
  in a system meant to be level-triggered.

**The differential oracle is the part most worth stealing.** Sieve runs the
workload *unperturbed* to obtain a reference trace, then compares:

1. **End-state check (§3.6.1)** — object counts by type and *field values of all
   objects* at end of run vs. reference. Found **28 of 46** bugs, including
   non-crashing ones: K8SPSMDB-578, where a crash caused the operator to skip
   creating an SSL certificate and silently fall back to insecure comms. None of
   the project's 71 hand-written tests asserted on that certificate.
2. **State-update summaries (§3.6.2)** — per-object *counts* of CREATE/DELETE
   across the run, deliberately **not** the sequence (sequences are
   nondeterministic and would false-alarm). Found **17 more**. K8SPXC-725 shows 2
   CREATE + 1 DELETE of the proxy pod vs. 1 CREATE in the reference — identical
   end state, buggy trajectory.

Nondeterminism handling (§3.7): run the unperturbed workload several times, diff
fields across runs, mask the ones that vary; exclude objects whose identifying
metadata is nondeterministic.

Results: **46 new bugs in 10 controllers** — 11 intermediate-state, 19
stale-state, 7 unobserved-state, 9 indirect; 35 confirmed, 22 fixed, **zero
rejected**. False-positive rate **3.5%**. **45 of 46 bugs were flagged by the
differential oracles; only 5 by log/exception checking.** Pruning removes
46.7%-99.6% of candidate plans. Adoption cost: 5,500 LOC Python + 3,100 LOC Go
instrumenting **10 client-library API methods**; "It took us on average three
hours to apply Sieve to each controller."

#### Acto (SOSP '23) — state transitions as test cases, two oracles

"Acto: Automatic End-to-End Testing for Operation Correctness of Cloud System
Management", Gu, Sun, Zhang, Jiang, Wang, Vaziri, Legunsen, Xu.
[PDF](https://www.cs.cornell.edu/~legunsen/pubs/GuETAlActoSOSP23.pdf) ·
[ACM](https://doi.org/10.1145/3600006.3613161) ·
[repo](https://github.com/xlab-uiuc/acto).

Explicitly complementary to Sieve (§8): "Sieve is a fault injector that checks
fault tolerance, while Acto is an end-to-end test generator that checks
functional correctness. **Sieve cannot find the bugs Acto detects, because it
assumes that the operator works correctly without faults.**" Acto injects no
faults at all; it mutates the *desired state*.

An operation is a pair (Sᶜ, D) — current state and a desired-state declaration —
and a correct operation drives Sᶜ →ᴰ Sᴰ with Sᴰ ⊨ D. Three requirements: reconcile
to valid desired states **regardless of current or previous state**
(level-triggering); recover from error states by rolling back; resist
*misoperations* (semantically invalid declarations that pass syntactic
validation). Tests are a **campaign**, not single operations from S₀: each Dᵢ₊₁ is
chained off the resulting Sᵢ, so every operation starts from a different,
non-initial state, with error injection and rollback edges.

The two oracles (§5.3):

- **Consistency oracle** — check Sᵢ ⊨ Dᵢ *from two independent views*: the
  operator's view and the management platform's view. "A buggy operator may show
  Sᵢ ⊨ Dᵢ while the management view shows Sᵢ ⊭ Dᵢ. Such view inconsistencies
  likely indicate the presence of bugs." Caught **23 of 56 bugs (41%)**.
- **Differential oracle** — does *not* check against Dᵢ. It exploits
  **level-triggering as a metamorphic relation**: for each transition
  Sᵢ₋₁ →ᴰⁱ Sᵢ, also run S₀ →ᴰⁱ S′ᵢ and assert **Sᵢ ≡ S′ᵢ**. Same desired state
  from two different start states must produce the same system state. It also
  checks rollback: after an error state Sᵉ, roll back with Dᵢ₋₁ and assert the
  result matches Sᵢ₋₁. Caught **25 bugs on normal transitions (44.6%) and 10 on
  rollback transitions (17.9%)**.

Taxonomy (Table 5), 56 new bugs in 11 operators: **undesired state 32**
(operator stops reconciling before desired state, latent, no error surfaced);
error state — managed system 4; error state — operator 10; **recovery failure
10**. Plus 630 misoperation vulnerabilities. False-positive rate **0.19%**
blackbox, **zero** whitebox.

Two findings worth internalizing. §6.4: "most bugs that Acto finds do not
manifest when performing operations from the initial state S₀." And the
recovery-failure root cause (§6.1.1): "operators perform new operations only
after the system is in a stable state… it makes failure recovery difficult,
because **it also blocks rollback operations if the system is in an error
state**." Over 35% of the 630 misoperation vulnerabilities cannot be mitigated by
rollback because of this. That is a structural anti-pattern to grep our own
reconcilers for: an "is stable?" gate that also gates recovery.

#### Anvil (OSDI '24) — the one that matters most for us

"Anvil: Verifying Liveness of Cluster Management Controllers", Sun, Ma, Gu, Ma,
Chajed, Howell, Lattuada, Padon, Suresh, Szekeres, Xu. **Jay Lepreau Best
Paper.** [PDF](https://tianyin.github.io/pub/anvil.pdf) ·
[USENIX](https://www.usenix.org/conference/osdi24/presentation/sun-xudong) ·
[repo](https://github.com/anvil-verifier/anvil). Written in **Verus**, i.e. in
Rust.

**What is proved — ESR (Eventually Stable Reconciliation):**

> **∀d. □(□desire(d) ⇒ ◇□match(d))**

Read outward-in: `◇□match(d)` is **progress** (eventually reach the desired
state) **and stability** (and *stay* there). `□desire(d) ⇒` is the necessary
premise — a forever-changing desired state cannot be guaranteed. The outer `□`
means this holds regardless of past execution, so the controller delivers
*multiple* successful reconciliations over a series of slow desired-state
changes. Fig. 5 gives four cases: never matches → violated; matches then
**deviates** → violated (the stability half, which is the one that catches
multi-writer stomping); satisfied; desired state changes forever → vacuously
satisfied.

**Power of ESR (§3):** of 70 bugs across 16 controllers found by Sieve and Acto,
**69% are precluded by ESR**.

**The framework/per-controller split** — the key structural question, answered in
Fig. 6:

*Anvil provides* (5,353 lines of reusable lemmas + 7,817 lines of trusted code):
a **TLA embedding in Verus of just 85 lines** (an execution is `nat → State`, a
temporal predicate is a Boolean over executions, `◇ □ ⇝` are functions from
temporal predicate to temporal predicate, `lift()` promotes a state predicate);
an **environment model (1,846 lines)** as a compound state machine with inner
machines for an asynchronous unordered network, the cluster-state store + API
server **with MVCC version checks**, other controllers (GC, StatefulSet, DaemonSet
are explicitly modelled), and clients that can change desired state at any time —
so **TOCTOU is captured natively**; a **fault model** where the controller crashes
an arbitrary number of times, **losing all in-memory state and restarting
reconciliation from the beginning**, and any request can fail, with a
partial-synchrony-flavoured assumption that faults eventually stop (modelled as a
"disable-fault" action with weak fairness); **70+ temporal reasoning lemmas**
(`leads_to_transitive` used 50+ times, `leads_to_stable`, `wf1`,
`invariant_by_induction`) so developers never unfold temporal-operator
definitions or touch execution indices; **60 environment lemmas** including the
flagship GC lemma `eventually_always_has_an_existing_owner` (200+ lines, reused
by all three controllers); a Kubernetes integration layer of 5,886 lines, of
which the paper notes "**67% of the trusted code is for defining wrapper
types**".

*Developer writes per controller*: a `Controller` trait with `initial_state()`,
`step(d, r, s) -> (S, Req)` returning the next local state and **at most one
external request** (making each step atomic w.r.t. cluster-state changes),
`done(s)`, `error(s)` — and the generic `reconcile()` loop over `step()` is **the
same for every controller**; a `ControllerModel` in Verus *ghost code*
(structurally identical, over ghost types, erased before compilation, **zero
runtime overhead**); two theorems — **conformance** (the model's `m_step` produces
the same output given the same input, a postcondition on `step`, largely
automated by Verus) and **ESR**; and a per-controller `match(d)` predicate.

**Proof burden (Table 1):**

| Controller | Trusted | Exec | Proof | Verify |
|---|---|---|---|---|
| ZooKeeper | 950 | 1,134 | 8,352 | 520s |
| RabbitMQ | 548 | 1,598 | 7,228 | 341s |
| FluentBit | 828 | 1,208 | 8,395 | 347s |

**Proof-to-code ratio 4.5×-7.4×**, ~2.5 person-months per controller — but the
*first* cost ~2 person-months because the strategy was being invented, and the
other two took ~**2 person-weeks each**. Incremental evolution is cheap: 28
features added to FluentBit averaged **<1 day and 47 lines changed, 19 of them
proof**. ~40% of proof effort is invariant proving.

**The reusable proof strategy (§5.1)** is the part that transfers even if we never
write a proof. Split ESR into (1) `env_is_eventually_stable` — faults stop,
conflicts with other controllers cease, desired state stops changing; and (2)
`liveness_in_stable_env` — from **any** state (any interleaving of prior
executions and faults, controller possibly mid-reconcile at any internal step),
converge. Lemma 2 decomposes into: the current reconciliation *terminates*
regardless of internal state; a new reconciliation *restarts*; from the initial
internal state the controller realizes the desired state. Plus a **parameterized
per-object-workflow lemma**, because controllers use the same
query-then-create-or-update shape for every object — one lemma parameterized by
state object discharges all of them.

**Bugs found, and the honest limits.** ZooKeeper: an intermediate-state bug that
"**Sieve applied extensive fault-injection testing on this controller but failed
to find**, because the bug only manifests in specific timing under specific
workloads"; and a bug updating *immutable* StatefulSet fields, caught because the
environment model encodes API-server validation. RabbitMQ safety (`replicas never
decreases`): violated by a GC race where a StatefulSet created by an old,
already-deleted desired state carried a larger `replicas` — "Safety can still be
violated because the GC may not immediately remove orphan stateful sets", fixed
by **making the controller wait for the GC to delete orphan stateful sets**.
RabbitMQ liveness: a naming rule assigning the **same service-object name to
different clusters**, so two CRs flip the shared object forever — caught precisely
because ESR demands `◇□match`, not `◇match`.

And the boundary, stated honestly (§4.3.3): "the built-in StatefulSet controller
can compete with the target controller forever… the environment model can
adversarially keep letting the target controller lose the race." Anvil
**assumes** competing controllers eventually stop; **it does not prove multi-writer
conflicts resolve.** The state of the art assumes our class 1 away. The direct
successor, "Compositional Verification of Cluster Control Planes"
([SOSP 2026, accepted](https://sigops.org/s/conferences/sosp/2026/accepted.html)),
is the first attempt at multiple interacting controllers.

Cross-validation (Table 2): they ran Acto (1,003 functional tests) and a **Rust
reimplementation of Sieve** (582 crash tests) against the verified controllers.
**Zero crash-test bugs.** One functional bug, traced to an incomplete trusted spec
of the external ZooKeeper API — a hole in the trust boundary, not the proof. The
repo has since moved beyond the paper's three and now verifies ReplicaSet,
Deployment, and StatefulSet.

#### Related work

- **"Reasoning about modern datacenter infrastructures using partial histories"**,
  HotOS 2021 ([PDF](https://sigops.org/s/conferences/hotos/2021/papers/hotos21-s11-sun.pdf))
  — the conceptual ancestor of both Sieve and Anvil, and the right framing for
  our class 3: components learn cluster state via a centralized store or via
  streamed notifications, but no observer sees the complete linear history. Each
  controller reasons from a **partial history** with gaps, staleness, and
  reordering relative to other observers.
- **SandTable**, EuroSys 2024 ([DOI](https://doi.org/10.1145/3627703.3650077),
  [repo](https://github.com/tangruize/SandTable)) — explore the state space at the
  TLA+ spec level, then confirm candidate bugs against the real implementation.
- **Kivi**, USENIX ATC 2024 ([PDF](https://www.usenix.org/system/files/atc24-liu-bingzhe.pdf))
  — model each controller and event as a process and exhaustively model-check
  their interleavings; found two new bugs in real Kubernetes controller source.
- **"Who Watches the Watchers?"**, NSDI 2026
  ([PDF](https://www.usenix.org/system/files/nsdi26-gu.pdf)) — empirical study of
  412 real-world failures across 13 Kubernetes operators.

**TLA+ in the Kubernetes ecosystem — honest assessment:** there is **no serious
upstream TLA+ effort** for kube-apiserver, kubelet, or scheduler. The formal
methods energy is in **etcd**:

- The [etcd/raft TLA+ spec](https://github.com/etcd-io/raft/pull/113) (merged Apr
  2024) adds `etcdraft.tla`, `MCetcdraft.tla`, and — most interestingly —
  **`Traceetcdraft.tla`**, doing **trace validation**: real execution traces
  captured from the library constrain the state space so TLC checks the
  *implementation's actual trace* against the spec, not just the spec's internal
  consistency. Modelled on Microsoft CCF's approach. This is the closest published
  thing to "fold a temporal spec over your production event log", and it is worth
  reading before we build ours.
- [etcd robustness testing](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md)
  — real cluster plus client traffic plus concurrent fault injection, recording
  every client operation and outcome as a complete history, then validating
  strict serializability with [Porcupine](https://github.com/anishathalye/porcupine)
  and validating watch guarantees. 16+ real bugs; now running continuously on
  Antithesis
  ([post](https://etcd.io/blog/2025/autonomus_testing_with_antithesis/)).

#### envtest and the Go testing conventions — mostly a cautionary tale

[envtest](https://book.kubebuilder.io/reference/envtest.html) "helps write
integration tests for your controllers by setting up and starting an instance of
etcd and the Kubernetes API server, **without kubelet, controller-manager or
other components**". The documented consequences:

- **No garbage collection.** "because no controllers monitor built-in resources,
  Kubernetes does not delete objects, even if you set up an `OwnerReference`."
  The prescribed workaround is explicit: "**test the ownership instead of
  asserting on existence**". **Our class 2 is precisely the class envtest
  structurally cannot test.**
- **Namespace deletion is broken.** "Deleting a namespace seems to succeed, but
  Kubernetes just puts the namespace in a Terminating state, and never actually
  reclaims it."
- **No built-in controllers at all**, so Deployments never produce Pods.

Important asymmetry: **finalizers *are* faithfully exercised** under envtest,
because finalizer semantics live in the apiserver's deletion handling. It is
ownerRef cascade GC that is absent. So envtest tests "my controller handles the
deletionTimestamp lifecycle" but cannot test "the platform actually reaped the
orphan".

[controller-runtime's FAQ](https://github.com/kubernetes-sigs/controller-runtime/blob/main/FAQ.md)
is blunt on both points that matter to us: "The fake client exists, but we
generally recommend using `envtest.Environment`… In our experience, tests using
fake clients gradually re-implement poorly-written impressions of a real API
server, which leads to hard-to-maintain, complex test code." And on assertion
style: "**Structure your tests to check that the state of the world is as you
expect it, *not* that a particular set of API calls were made.**" That single
sentence is the principle Sieve and Acto industrialize.

The [fake client](https://pkg.go.dev/sigs.k8s.io/controller-runtime/pkg/client/fake)
documents that `Generation` and `ResourceVersion` "don't behave properly. Patch or
Update operations that rely on these fields will fail or give false positives" —
i.e. **optimistic-concurrency emulation is unreliable, making it useless for
class 1**. Our `InMemoryBackend` maintaining real `resource_version` semantics and
being contract-tested against the sqlite backend is a genuine advantage here, not
an incidental detail.

On step-driven testing: `k8s.io/utils/clock/testing`'s `FakeClock` provides
`Step(d)`, `SetTime(t)`, and — the key to race-free determinism — `HasWaiters()`,
"returns true if Waiters() returns non-0 (so you can write race-free tests)".
The Job controller's `newControllerWithClock(...)` exists solely so tests can pass
a fake. But "reconcile until stable" as a harness is **convention by folklore**
(see [kubebuilder discussion #3290](https://github.com/kubernetes-sigs/kubebuilder/discussions/3290)):
either `Eventually` around a live controller under envtest, or a hand-rolled
`for i := 0; i < maxRequeues; i++ { r.Reconcile(...) }`. **Nobody in the Go
ecosystem ships a first-class step-driven reconcile harness.** We already have
one.

## B. Deterministic simulation and interleaving exploration

### B.1 FoundationDB — the canonical account, and its precondition list

The maintainers' doc is [Simulation and Testing](https://apple.github.io/foundationdb/testing.html);
the peer-reviewed account is the SIGMOD 2021 paper
[FoundationDB: A Distributed Unbundled Transactional Key Value Store](https://www.foundationdb.org/files/fdb-paper.pdf),
§4 and §6.2. Will Wilson's 2014 Strange Loop talk is the earliest public account
([video](https://www.youtube.com/watch?v=4fFDFbi3toc)).

The mechanism, quoted from §4: "FDB was built from the ground up to make this
testing approach possible. **All database code is deterministic; accordingly
multithreaded concurrency is avoided** (instead, one database node is deployed
per core)." And: "the simulator process of FDB, where **all sources of
nondeterminism and communication are abstracted, including network, disk, time,
and pseudo random number generator**." Flow "provides the Actor programming model
that abstracts various actions of the FDB server process into a number of actors
that are scheduled by the Flow runtime library… **The production implementation
is a simple shim to the relevant system calls.**"

That last clause is the whole trick and the precondition list: *time, network,
disk, randomness, and task scheduling all go behind an interface, with production
as the thin shim and simulation as the interesting implementation.* Pierre Zemb's
[deep-dive](https://pierrezemb.fr/posts/diving-into-foundationdb-simulation/)
names the concrete seam: `INetwork` with `Net2` (Boost.ASIO, real TCP) vs `Sim2`
(in-memory `Sim2Conn` buffers), and `deterministicRandom()` replacing every RNG.
Time acceleration falls out of discrete-event scheduling: "Discrete-event
simulation can run arbitrarily faster than real-time if CPU utilization within
the simulation is low, as the simulator can fast-forward clock to the next
event."

**BUGGIFY** (§4): "FDB itself cooperates with the simulation in making rare states
and events more common… At many places in its code-base, the simulation is given
the opportunity to inject some unusual (but not contract-breaking) behavior such
as unnecessarily returning an error from an operation that usually succeeds,
injecting a delay in an operation that is usually fast, choosing an unusual value
for a tuning parameter." And the underrated line: "**Randomization of tuning
parameters also ensures that specific performance tuning values do not
accidentally become necessary for correctness.**"

**Conditional coverage macros** are the ancestor of Antithesis "sometimes
assertions": "a developer concerned that a new piece of code may rarely be invoked
with a full buffer can add the line `TEST(buffer.is_full());`… If the number is
too low, or zero, they can add buggification, workload, or fault injection
functionality to ensure that scenario is adequately tested." See
[Antithesis on sometimes-assertions](https://antithesis.com/docs/concepts/properties_assertions/sometimes_assertions/).
The point is coverage-of-interesting-states, not correctness — and it is the
cheapest idea in this whole document to steal.

The stated limitations are the paragraph to weigh: "Simulation is not able to
reliably detect performance issues… **It is also unable to test third-party
libraries or dependencies, or even first-party code not implemented in Flow. As a
consequence, we have largely avoided taking dependencies on external systems.**"
Against which §6.2's payoff claim: "On numerous occasions, the FDB team executed
ambitious, ground-up rewrites of major subsystems. **Without simulation testing,
many of these projects would have been deemed too risky or too difficult, and not
even attempted.**"

Note §7's positioning, in the authors' own words: "**While model checking can be
more exhaustive than simulation, it can only verify the correctness of a model
rather than that of the actual implementation.**"

### B.2 TigerBeetle's VOPR

[docs/internals/vopr.md](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md),
[safety concepts](https://docs.tigerbeetle.com/concepts/safety/). Same
precondition statement, more tersely: "**In the simulator, all non-deterministic
parts of the system are stubbed out. This includes the clock, network, and disk
operations.**" Reproduction is by *(seed, git commit)* pair.

The part specific to our question is the invariant checking, which comes in two
layers:

1. **In-code assertions kept on in production.** Thousands of assertions enabled
   in release builds; a violated assertion halts rather than proceeding in a
   wrong state. This is not a test-only oracle — it is the same predicate in both
   worlds, which is exactly the "one property definition, two execution contexts"
   shape we want.
2. **External state checkers beside the simulated cluster** — a `StateChecker`
   comparing replica state machines, plus the strong structural invariant that
   caught-up replicas' data files are byte-for-byte identical. That is a
   cross-node equality invariant only a simulator holding all nodes in one process
   can cheaply evaluate.

Liveness in VOPR is checked operationally: after faults heal within the tolerance,
assert progress within a bounded number of ticks. That is **bounded liveness — a
safety property in disguise**, which is the only kind checkable on a finite trace.
Same reduction as our `pass_bound`. Independent corroboration:
[Jepsen's TigerBeetle 0.16.11 analysis](https://jepsen.io/analyses/tigerbeetle-0.16.11).

### B.3 The Rust ecosystem, concretely

**[madsim](https://github.com/madsim-rs/madsim)** — closest thing to FDB-in-Rust.
Intercepts **time, task scheduling, network, randomness, and filesystem**
(modules `time`, `task`, `net`, `rand`, `fs`, plus a `buggify` module). Delivered
as shim crates (`madsim-tokio`, `madsim-tonic`, `madsim-etcd-client`,
`madsim-rdkafka`, `madsim-aws-sdk-s3`) that are byte-identical re-exports of the
originals unless built with `RUSTFLAGS="--cfg madsim"`. The cost is real and
RisingWave says so
([their post](https://www.risingwave.com/blog/deterministic-simulation-a-new-era-of-distributed-system-testing/)):
rewrite `tokio = { package = "madsim-tokio", … }`, then `[patch.crates-io]` a tail
of transitive crates that sneak in nondeterminism — `quanta`, `getrandom`,
`tokio-retry`, `tokio-postgres`, `tokio-stream`. Their own summary of the
drawbacks: "high intrusiveness and engineering complexity." The `--cfg madsim`
flag also means a **second full compilation matrix** with no shared build cache.
The failure mode that bites: madsim gives no "you left determinism" alarm, only
irreproducibility — you find out because a seed does not replay.

**[turmoil](https://github.com/tokio-rs/turmoil)** — much narrower, much cheaper.
It is a **network (and now filesystem / io_uring) simulator, not a scheduler
simulator**. Confirming the specific question: **yes, it uses tokio's paused
time** — `src/rt.rs` builds the runtime with
`Builder::new_current_thread().enable_time().start_paused(true)`
([source](https://docs.rs/turmoil/latest/src/turmoil/rt.rs.html)), and `tick()`
block_on's a `sleep(duration)` inside a `LocalSet`, letting tokio's auto-advance
fire when the LocalSet goes idle rather than calling `tokio::time::advance`
directly. All hosts run on one thread in a `LocalSet`, so there is no OS-thread
nondeterminism and time is logical. But it does **not** control intra-runtime task
poll order beyond current-thread scheduling, does not intercept `rand`, and does
not intercept `std::time`. The README is honest: "Turmoil is not yet opinionated
on how to structure your application code to swap in simulated types under test."

**[loom](https://docs.rs/loom/)** — wrong tool for reconcilers, and the docs say
why. It implements the CDSChecker algorithm doing **exhaustive** permutation over
the C11 memory model, modelling only `loom::sync::atomic`, `Mutex`, `RwLock`,
`Condvar`, `UnsafeCell`, `thread`, `Lazy` via `cfg(loom)` type substitution.
Anything using `std` sync types is invisible to it. `LOOM_MAX_PREEMPTIONS` (2-3
recommended) is the escape valve for state explosion. Its unit of analysis is "does
this lock-free queue have a race", not "does this controller converge".

**[shuttle](https://github.com/awslabs/shuttle)** — same substitution shape,
randomized scheduling implementing **PCT** (Burckhardt et al., ASPLOS 2010), with
a probabilistic bound on finding a bug of depth *d*. The authors state the
tradeoff: "**Shuttle is not sound… but it scales to much larger test cases than
Loom**". loom = exhaustive/small; shuttle = randomized/large. Neither is a
distributed-systems simulator.

**[kani](https://model-checking.github.io/kani/)** — bounded model checker on
CBMC. Targets UB in `unsafe`, panics, arithmetic overflow, `assert!`, function
contracts. **No concurrency support at all**, explicit unwind bounds required,
unbounded data structures blow up the SAT instance. Right for "this parser cannot
panic on any input"; categorically wrong for whole-reconciler proofs.

**[stateright](https://github.com/stateright/stateright)**
([docs](https://docs.rs/stateright/), [book](https://www.stateright.rs/)) — the one
that could plausibly check reconcilers directly. The `Model` trait is deliberately
tiny: associated `State` and `Action`; `init_states()`; `actions(&self, state,
&mut Vec<Action>)`; `next_state(&self, state, action) -> Option<State>`;
`properties()`; optional `within_boundary()` to bound the search. Properties carry
an [`Expectation`](https://docs.rs/stateright/latest/stateright/enum.Expectation.html):
`Always` (all reachable states), `Sometimes` (at least one — the coverage check),
`Eventually` (eventually true on all behaviour paths). The checker reports
*discoveries*: a counterexample for `Always`, an example for `Sometimes`, and for
`Eventually` it looks for terminal states or cycles where the property never
became true. `model.checker().threads(n).spawn_bfs().join()` with
`.assert_properties()`; BFS trades memory for shorter counterexamples; the
interactive Explorer (`checker.serve("0:3000")`) renders sequence diagrams.

The actor story is the interesting part: `Actor` requires `type Msg / State /
Timer / Random / Storage` and `on_start` / `on_msg(&self, id, state: &mut
Cow<State>, src, msg, out: &mut Out<Self>)`, and **the same code runs both under
model checking and over real UDP**. The hard requirement, also stated:
"**Stateright must have visibility of every input and output**" — all side effects
must be expressed as messages/timers/storage-writes routed through `Out<Self>`,
never performed inline. That is the FDB precondition restated at actor
granularity, and **it is the precondition our `reconcile() -> ReconcileOutcome`
already satisfies.**

The book's own TLA+ comparison chapter is candid about the gap: TLA+ wins on
"complex temporal properties and fairness" and symmetry reduction; stateright wins
on checking the *implementation* rather than the design, on speed, on `Sometimes`
sanity checks, and on the Rust ecosystem. Read: **`Eventually` without fairness is
weak**, so bounded-liveness-as-safety remains the pragmatic encoding.

**proptest state-machine testing** — the direct Rust analogue of
`quickcheck-state-machine`. Two real crates:

- [`proptest-state-machine`](https://docs.rs/proptest-state-machine)
  ([book chapter](https://proptest-rs.github.io/proptest/proptest/state-machine.html)),
  in the proptest org. Split across two traits. `ReferenceStateMachine`:
  `type State`, `type Transition`, `init_state() -> BoxedStrategy<State>`,
  `transitions(state) -> BoxedStrategy<Transition>`, `apply(state, &transition) ->
  State`, and an overridable `preconditions(state, &transition) -> bool`.
  `StateMachineTest`: `type SystemUnderTest`, `type Reference`,
  `init_test(ref_state) -> SUT`, `apply(sut, ref_state, transition) -> SUT`
  (postconditions here), `check_invariants(&SUT, &ref_state)` (run after **every**
  transition), `teardown(sut)`. Driven by
  `prop_state_machine! { #[test] fn t(sequential 1..20 => MyTest); }`. Shrinking is
  genuinely good: `SequentialValueTree` deletes transitions from the back, then
  shrinks individual transitions front-first, then shrinks the initial state, with
  `complicate()` to back out over-aggressive cuts.
- [`proptest-stateful`](https://github.com/readysettech/proptest-stateful) from
  ReadySet ([design writeup](https://readyset.io/blog/stateful-property-testing-in-rust)),
  a single-trait design built because they needed **async SUTs** and richer
  generation control. Worth checking against our `async fn reconcile_step`.

Note the shape of both: **generic engine + per-system model.** Generation,
sequencing, and shrinking are written once; you supply `Transition`, `apply`,
`preconditions`, `check_invariants`.

### B.4 Adoption verdict for our harness

Given an already-injectable `Clock` and an already step-driven reconcile driver,
the verdict is mostly *negative* on the heavyweight options, and that is the
useful finding.

**Do not adopt madsim.** Its entire purpose is to recover determinism over task
scheduling and background loops. A step-driven reconcile driver means **there is
no background loop whose interleaving is the source of nondeterminism** — the test
already picks the schedule. Paying a doubled build matrix, `[patch.crates-io]` on
`getrandom`/`quanta`/`tokio-*`, and brittleness to every new dependency, in order
to determinise something we determinised structurally, is a bad trade. RisingWave
adopted it because they *could not* restructure — thousands of long-lived
streaming actors over real gRPC. Not our situation.

**Do not adopt loom, shuttle, or kani for reconcilers.** They operate at
atomics/locks granularity (loom, shuttle) or need bounded state and have no
concurrency support (kani). If we ever hand-roll a lock-free structure inside
cleat's VT engine or the resource store, shuttle is the cheap one to reach for.

**turmoil is the only DST-family crate worth considering, and only for
`flotilla-transport` / peer federation.** It is cheap because it is honest about
scope, and the adoption cost is one seam plus no build-matrix change. It does not
help with reconcilers.

**stateright is genuinely interesting for one or two protocols.** `State` = store
contents; `Action` = {reconcile(id), deliver(actuation), drop(actuation),
advance_clock(δ), external_write(…), restart_controller}; `next_state` = call the
real `reconcile()`. `Always` gives the safety invariants, `Sometimes` gives the
FDB-style coverage check that a fixture actually reaches the interesting states.
The realistic caveat is state-space size — a store full of strings and UUIDs is
not finite, so it needs `within_boundary()` plus aggressive name abstraction.
Treat it as a *targeted* tool (convoy lifecycle, replication) rather than a
blanket obligation.

**The highest-value, lowest-cost move is `proptest-state-machine` on top of the
harness we already have.** Today `LivenessScenario` enumerates three hand-written
worlds and the driver runs a straight-line pass loop. Swapping the scenario source
for a generated one costs no new execution model: `Transition` becomes our step
vocabulary plus the missing ones (external spec write, delete, restart, clock
advance), `apply` is the reference model's expectation, `check_invariants` is the
existing `assert_*` battery, `preconditions` is phase legality. The real prize is
**automatic shrinking to a minimal failing transition sequence**, which a
hand-written scenario list can never give. It composes with `VirtualClock`
(`AdvanceClock(δ)` as a transition) and with `WriteCountingBackend` ("no writes at
fixpoint" as an invariant checked after *every* transition rather than only at the
end).

## C. The generic shape: runtime verification, typestate, event-sourcing invariants

### C.1 A monitor *is* a fold — this is a theorem, not a metaphor

The canonical citation is Bauer, Leucker & Schallhart, **"Runtime Verification for
LTL and TLTL", ACM TOSEM 20(4), Article 14, 2011**
([DOI](https://dl.acm.org/doi/10.1145/2000799.2000800), open copy at
[ORA Oxford](https://ora.ox.ac.uk/objects/uuid:68149609-5c67-49d1-ba69-192ced2aaa6f)).
Three results matter here:

1. **LTL₃ three-valued semantics.** A finite prefix *u* of an infinite behaviour
   evaluates to `⊤` (every infinite continuation satisfies φ), `⊥` (none does), or
   `?` (inconclusive). This is the correct semantics for "I have seen a prefix of
   the log so far", and it is exactly why a green test run over a finite trace is
   not a proof of a temporal property.
2. **Monitor synthesis produces a minimal DFA.** The construction is "optimal in
   two respects: First, the size of the generated deterministic monitor is
   minimal, and, second, the monitor identifies a continuously monitored trace as
   either satisfying or falsifying a property **as early as possible**." A minimal
   DFA stepped over a trace is, operationally, `trace.fold(q0, delta)` with a
   verdict read off the state. **The fold framing is literally the monitor
   construction.**
3. **Monitorability.** "The set of monitorable properties does not only encompass
   the safety and cosafety properties but is strictly larger." So "safety =
   foldable, liveness = not" is a good first approximation but formally
   conservative.

The tooling lineage is **MOP / JavaMOP** (Roşu et al.,
[ICSE 2012 PDF](https://fsl.cs.illinois.edu/publications/jin-meredith-lee-rosu-2012-icse.pdf),
[STTT overview](https://link.springer.com/article/10.1007/s10009-011-0198-6)),
which is deliberately **specification-formalism-agnostic**: write a property in
LTL, ERE, CFG, or FSM and MOP compiles it to a monitor; the engine
(instrumentation, indexing, monitor stepping) is shared.

The single most transferable idea from MOP is **parametric trace slicing** (Chen &
Roşu, formalised in [arXiv:1112.5761](https://arxiv.org/pdf/1112.5761)): a single
global event trace is *sliced* by parameter binding into one sub-trace per
instance, and each slice is fed to an ordinary non-parametric monitor. The
factoring theorem is stated explicitly — slicing "enables leveraging **any**
non-parametric, conventional trace analysis technique to the parametric case."
That is the precise answer to "I have one event log but N resources each with its
own state machine": you do not write a per-resource monitor engine. You write one
monitor and one slicing key. Our `resource_events` rows already carry
`(group, version, kind, namespace)` in the primary key and the object name in the
body — the slicing key is sitting there.

The process-mining branch reaches the same automaton conclusion independently:
**LTLf/LDLf and DECLARE**. Declarative process models are LTLf constraint patterns;
monitoring and conformance checking translate each constraint to a finite-state
automaton and replay the trace. Maggi, Montali et al., "Monitoring Constraints and
Metaconstraints with Temporal Logics on Finite Traces", ACM TOSEM 2022
([arXiv:2004.01859](https://arxiv.org/pdf/2004.01859)); the automata-theoretic
approach in [arXiv:2111.13136](https://arxiv.org/pdf/2111.13136). Their operational
argument: LTLf "brings an operational advantage over infinite-trace counterparts
since reasoning can be carried out by manipulating finite-state automata."

Rust implementations are thin. The most substantial is **RTLola** (stream-based
runtime verification, Rust implementation, `rtlola-frontend` on crates.io), whose
model is stream equations over an input trace rather than raw LTL — arguably a
better fit for resource conditions than LTL. There is no widely-used Rust
LTL-monitor crate. Given that our alphabet is our own event enum, synthesising a
DFA per property is a few hundred lines if we ever want it.

### C.2 Safety folds, liveness does not — and why bounded liveness is the escape

Lamport, **"Proving the Correctness of Multiprocess Programs"**, IEEE TSE SE-3(2),
1977 ([PDF](https://lamport.azurewebsites.net/pubs/proving.pdf)) is the origin of
the safety/liveness split; Lamport's own retrospective calls the paper's major
contribution its informal definitions of safety and liveness, with an invariance
proof method for safety.

Alpern & Schneider, **"Defining Liveness"**, IPL 21(4), 1985
([PDF](https://www.cs.cornell.edu/fbs/publications/DefLiveness.pdf)) gives the
topological characterisation and the decomposition theorem: over the natural
topology on infinite sequences, **safety properties are exactly the closed sets,
liveness properties exactly the dense sets, and every property is the intersection
of a safety property and a liveness property.** Two consequences we can lean on:

- **Safety = prefix-closed = foldable.** A safety property is violated by some
  *finite* prefix. A fold over the trace, carrying whatever inductive state the
  invariant needs, is therefore a *complete* checker for safety: no false
  negatives, no false positives, and the violation localises to a specific event
  index.
- **Liveness is not foldable on a finite trace.** No finite prefix can refute it —
  that is what dense means. So any finite-trace "liveness test" is really either
  (a) a **bounded** liveness property ("progress within N steps"), which *is* a
  safety property and *is* foldable, or (b) an LTL₃ inconclusive verdict.

Our `assert_bounded_convergence` with `pass_bound` is case (a), and so is
TigerBeetle's VOPR. That is the correct and honest encoding, and it is worth
saying out loud in the harness's own documentation: **we check bounded
convergence, not liveness.**

TLA+ frames it in exactly the shape needed: a spec is
`Init ∧ □[Next]_vars ∧ Fairness`. Invariants are predicates over a *single* state;
**action properties** are predicates over *consecutive state pairs*. Both are
foldable — the accumulator for an action property is `(prev_state, verdict)`. Only
the fairness conjunct escapes. Stateright's book concedes the same gap.

### C.3 Event-sourcing invariants: decide/evolve, and the cross-aggregate problem

Chassaing's
[**Functional Event Sourcing Decider**](https://thinkbeforecoding.com/post/2021/12/17/functional-event-sourcing-decider)
is the cleanest statement of the functional core:

```fsharp
type Decider<'c,'e,'s> =
    { decide: 'c -> 's -> 'e list
      evolve: 's -> 'e -> 's
      initialState: 's
      isTerminal: 's -> bool }
```

with state reconstruction as `List.fold evolve initialState pastEvents`. The
correspondence to §C.1 is exact: **`evolve` *is* the monitor transition function
δ**, `initialState` is q₀, `isTerminal` is the absorbing-state predicate, and
`decide` is the part a monitor does not have. Chassaing's discipline — "`evolve`
should probably not be more than a few lines of code" — is what keeps δ a DFA
rather than an interpreter.

**Aggregates as the consistency boundary.** Vaughn Vernon, *Effective Aggregate
Design*, [Part I](https://www.dddcommunity.org/wp-content/uploads/files/pdf_articles/Vernon_2011_1.pdf),
[Part II](https://www.dddcommunity.org/wp-content/uploads/files/pdf_articles/Vernon_2011_2.pdf),
[Part III](https://www.dddcommunity.org/wp-content/uploads/files/pdf_articles/Vernon_2011_3.pdf).
The rule that matters: *model true invariants in consistency boundaries* — a
properly designed aggregate keeps its invariants consistent within a single
transaction, and a properly designed bounded context modifies only one aggregate
instance per transaction. Cross-aggregate rules are pushed to eventual
consistency.

**The cross-aggregate prior art is Dynamic Consistency Boundaries (DCB).** Sara
Pellegrini's "Kill the Aggregate!" is the origin; the community spec and worked
examples are at [dcb.events](https://dcb.events/) (see
[Course subscriptions](https://dcb.events/examples/course-subscriptions/), the
canonical example of a rule spanning two aggregates). The idea: instead of
pre-partitioning the log by aggregate id, a command declares a **query** over the
event log (event type × tags); matching events are folded to build exactly the
decision state; and the append is guarded by an optimistic-concurrency condition
on *that same query* — append only if no new matching events since position P.
The boundary is computed per command rather than baked into the schema.
Implementations: [bwaidelich/dcb-eventstore](https://github.com/bwaidelich/dcb-eventstore),
[eventsourcing (Python) DCB docs](https://eventsourcing.readthedocs.io/en/latest/topics/dcb.html);
a balanced skeptical take at
[planetgeek.ch](https://www.planetgeek.ch/2026/06/23/event-sourcing-aggregates-dynamic-consistency-boundaries-or-what/).

**DCB is parametric trace slicing, arrived at independently by the DDD
community.** A query + tags is a slicing criterion; the fold over the slice is the
monitor; the append-condition is the verdict acted upon. If we want cross-resource
invariants over our event history, DCB gives the *decision-time* version and MOP's
trace slicing gives the *observation-time* version, and they are the same
construction. The two literatures do not cite each other, which is a genuine gap
and a mild confidence boost that the construction is forced.

On **replaying the log to assert invariants**: well-established in practice,
under-written-up. The strongest primary statements are (a) the Decider pattern
makes it trivially available because `evolve` is total and pure; (b) TigerBeetle's
`StateChecker` is the production-grade version; (c) FDB's workload CHECK phase and
etcd's `Traceetcdraft.tla` trace validation (§A.3) are the same idea at two
different levels of rigour.

### C.4 Typestate in types vs. transitions as data

**Typestate in types.** The
[Rust Embedded Book's typestate chapter](https://docs.rust-embedded.org/book/static-guarantees/typestate-programming.html)
is the canonical short statement: encode state in the *type*, use move semantics so
`into_next()` consumes the previous state, making illegal transitions
unrepresentable at compile time at zero runtime cost. Hoverbear's
["Pretty State Machine Patterns in Rust"](https://hoverbear.org/blog/rust-state-machine-pattern/)
is the widely-cited long form. Follow-ups worth reading for the tradeoff: Yoshua
Wuyts' [State Machines](https://blog.yoshuawuyts.com/state-machines) and
[State Machines II](https://blog.yoshuawuyts.com/state-machines-2), and Deis Labs'
[A Fistful of States](https://deislabs.io/posts/a-fistful-of-states/) — which is
specifically about a **Kubernetes kubelet written in Rust**, i.e. exactly our
domain, and lands on typestate-with-an-enum-wrapper.

**The crates.** [`rust-fsm`](https://docs.rs/rust-fsm/) is the most
data-table-shaped: a `StateMachineImpl` trait with input alphabet, state set, and
transition function, plus a `state_machine!` DSL that *is* a declarative
transition table. [`statig`](https://crates.io/crates/statig) is hierarchical, and
its README contains the clearest public statement of the tradeoff — the typestate
pattern "is useful for designing an API by enforcing validity of operations at
compile time", but for a dynamic system where "event order is determined at run
time… you'd need to use an enum to wrap different states, resulting in extra
boilerplate for little advantage since operation order is unknown and can't be
checked at compile time." The [`typestate`](https://crates.io/crates/typestate)
crate generates the encoding from a DSL and, tellingly, emits a **DOT diagram** —
an admission that pure typestate loses introspection and you have to bolt it back
on at macro-expansion time.

**The tradeoff, sharply.** Types give a compile-time proof at zero runtime cost,
but: (1) **no introspection** — you cannot enumerate states, render the graph,
diff two versions of the machine, or generate docs without a macro separately
emitting that metadata; (2) **no dynamic dispatch over states** without an enum
wrapper, which reintroduces the runtime match; (3) **it does not survive
serialization** — the moment a state is persisted and rehydrated you are back to
matching on a tag, and the compile-time proof covers only the in-memory segment
between deserialize and serialize.

**Point (3) is decisive for us.** Our phases live in sqlite and cross a wire.
Conversely, transitions-as-data gives one generic engine plus a per-resource table
that can be *validated* (no unreachable states, no missing terminal, every phase
reachable), *introspected* (rendered, documented, diffed), and *checked against the
log* (does the observed sequence of phase changes conform to the table? — which is
exactly DECLARE conformance checking). The cost is that illegal transitions become
a runtime error rather than a compile error.

The honest synthesis, which `statig`'s docs and the Deis Labs kubelet post both
converge on: **typestate for linear, in-memory, API-shaped sequences** (builders,
connection handshakes, PTY setup); **a data table for persisted, event-driven,
externally-sequenced lifecycles** (resource phases, convoy lifecycle).

This is also the answer to the §A.2 tension. Kubernetes deprecated `phase` as a
*published API element*. Nothing in that argument tells us to stop *modelling* the
transition relation — and modelling it as declared data is what makes it foldable.

### C.5 Per-resource proofs vs. one generic harness — the principled answer

There is a principled answer, and it is nearly unanimous across every community
surveyed: **a generic engine with a per-system property specification.** Not
per-system engines. The evidence, with who says it:

- **MOP is the strongest explicit statement**, because it is the framework's design
  thesis: MOP is deliberately **logic-agnostic** — the engine is fixed, the
  specification formalism is a plugin. The parametric-trace-slicing paper states
  the factoring theorem outright: slicing "enables leveraging **any**
  non-parametric, conventional trace analysis technique to the parametric case."
- **Bauer/Leucker/Schallhart** give the same factoring as a *synthesis result*:
  property in → minimal DFA out → one generic stepping loop. Nobody writes a
  bespoke monitor per property.
- **Anvil** states it structurally: 5,353 lines of reusable lemmas and a generic
  `reconcile()` loop that is "the same for every controller"; the developer writes
  a model and two theorems. Its per-object-workflow lemma is *parameterized by
  state object* precisely to avoid per-object proofs.
- **Stateright**: the `Model` trait is the only thing you implement; `Checker`,
  DFS/BFS, the Explorer, and counterexample reporting are generic over
  `M: Model`. TLA+/TLC is the same shape one level up.
- **proptest-state-machine**: `SequentialValueTree` (generation, sequencing,
  shrinking) is generic; `ReferenceStateMachine` + `StateMachineTest` are
  per-system.
- **FDB and TigerBeetle**: one simulator, per-workload CHECK phases and
  per-subsystem assertions. FDB §4: "FDB uses a variety of test oracles… Most of
  the synthetic workloads used in simulation have assertions built in to verify
  the contracts and properties of the database."
- **DCB**: one event store and one append-condition mechanism; the *query* is the
  per-invariant specification.

The one dissenting consideration worth naming: per-system *engines* win only when
a property is so specialised that expressing it in the generic vocabulary costs
more than writing the checker — TigerBeetle's byte-identical-data-files invariant
is of that kind, and is implemented as a bespoke `StateChecker`. The rule of
thumb: **generic engine plus per-resource property spec by default; bespoke
checkers only for whole-ensemble structural invariants the per-resource vocabulary
cannot express.**

Applied to us: this is already our architecture. `test_support/liveness.rs` is the
generic engine; each reconciler supplies the per-resource spec. The literature
points at two upgrades, not a rewrite — (a) replace hand-written
`LivenessScenario`/`LivenessStep` sequences with generated-and-shrunk ones, and
(b) make the per-resource phase transition table *data* rather than code, so one
table drives the reconciler, validates itself, and serves as the monitor δ for a
fold over the event history.

## Synthesis

### S.1 Prior art mapped to our three bug classes

**Class 1 — multi-writer fields with no declared owner.**

This is the **worst-covered class in the literature**, which is itself the
finding. Anvil §4.3.3 *assumes* competing writers eventually stop rather than
proving conflict resolution; SSA is advisory and officially tells controllers to
force. The relevant matches:

| Source | Entry | Bearing on us |
|---|---|---|
| Anvil §6.2 | RabbitMQ liveness bug: the same service-object name assigned to different clusters, so two desired states flip the shared object forever | Two logical owners, one field, oscillation. Caught *only* because ESR demands `◇□match` (stability), not `◇match`. A convergence-only test — which is what `assert_bounded_convergence` is — passes this bug. |
| Anvil §4.3.3 | The explicit assumption that competing controllers eventually stop | The state of the art assumes class 1 away. SOSP 2026's compositional verification is the first attempt. Do not expect to buy a solution. |
| Sieve §3.6.2 | State-update summaries — per-object CREATE/DELETE counts diverging from an unperturbed reference (17 of 46 bugs) | The cheapest possible stomp oracle: a field written N+1 times in one run and once in the reference is a stomp even when end states match. |
| Acto §5.3.1 | Consistency oracle: declared value vs. stored value, from two views (23 of 56 bugs) | "I declared X, the store says Y" is the observable symptom of a stomp. |
| Acto §5.3.2 | Differential oracle: same desired state from S₀ and from Sᵢ₋₁ must yield identical state (25 bugs) | Path-dependence of a field's final value *is* what a stomping writer produces. |
| SSA (§A.1) | Declared per-field ownership with a loud 409 | The right *idea*; k8s's own experience says an advisory version degrades to "everyone forces". |

**Class 2 — restart/recovery paths that don't commute with in-flight lifecycle
states.**

The **best-covered class**, by a wide margin.

| Source | Entry | Bearing on us |
|---|---|---|
| Sieve, intermediate-state | **11 bugs** | Definitionally this class. Fig. 1 (crash between `Delete(pod)` and `Finalizing` → stuck forever, volumes leaked) is "recovery doesn't commute with an in-flight deletion". |
| Sieve §5.1.1 root cause | "these condition checks only detect states from running the reconciliation loop in its entirety… controllers lack mechanisms analogous to write-ahead logging or journaling to guarantee atomicity of each reconcile action" | The general statement. Every one of our recovery paths deserves this audit. |
| Sieve, stale-state K8SPSMDB-430 | Controller sees a `DeletionTS` belonging to an **already-deleted** cluster and deletes the **newly created** one; fix = **match by UID, not by name** | Our "recovery resurrecting deleted records" almost verbatim. We have no UID field — `name` is the identity. |
| Sieve, unobserved-state | Controller misses the transient non-nil `deletionTimestamp`, so volumes leak | "Dropping capabilities on restart" / missing a mid-deletion lifecycle state. |
| Acto, recovery failure | **10 bugs**, all 10 caught by the rollback differential oracle | "neither addressed by restarting the operator nor by issuing new operations". |
| Acto §6.1.1 | "operators perform new operations only after the system is in a stable state… it also blocks rollback operations if the system is in an error state" | A specific anti-pattern to grep our reconcilers for. |
| Anvil, crash fault model + ESR | Crash arbitrarily often, lose all in-memory state, restart from the beginning, still converge and stay converged | The general *specification* for this class. |
| Anvil §5.1.2 | After restart, a request built from an older desired state can still be in flight | "Recovery doesn't commute with in-flight requests" — a distinct proof obligation from "recovery doesn't commute with stored state". |
| Anvil GC lemma / RabbitMQ fix | "wait for the GC to delete orphan stateful sets" | An in-flight lifecycle state (orphan awaiting reap) observed by a controller assuming reaping is instantaneous. **This is our lease bug.** |
| k8s §A.2 (b)-(e) | at-most-one doctrine; finalizer on the holder as an ordering barrier; check `deletionTimestamp` before *acquisition*; forcible reclaim as explicit assertion | The idiomatic fix set, ready to adopt. |

**Class 3 — reads against the wrong store/authority.**

| Source | Entry | Bearing on us |
|---|---|---|
| Sieve, stale-state | **19 bugs — the largest single category** | Two API servers, controller reconnects to the lagging one. The closest published analogue: more than one authority for the same data. |
| Sieve §5.1.1 lessons | "controllers were not adequately using Kubernetes' mechanisms to tolerate asynchrony and staleness: like object versioning and unique IDs (instead of referring to objects by names, that need not be unique), or using coordination mechanisms to enforce ordering" | The design checklist for a federated store, and it indicts identity-by-name. |
| Sieve prioritisation | Stale-state generation focuses on **deletions**, "because they are destructive operations" | A free prioritisation heuristic for where to look first. |
| Acto §5.3.1 | Consistency oracle across two views of the same fact (23 bugs) | Directly portable: for each declared field, resolve it in each store and diff. |
| Anvil §4.3.1 | Store and API server as separate machines with MVCC checks; TOCTOU captured natively | The formal core of class 3 — but models *staleness*, not *disagreeing authorities*. |
| HotOS '21 partial histories | Every controller reasons from a partial view with gaps and reordering | The conceptual framing. |
| etcd robustness tests | Full-history recording + Porcupine strict-serializability checking | The template if the *store* rather than the reconciler is under test. |

**The honest gap: none of Sieve, Acto, or Anvil targets genuine multi-store
federation** — different stores with genuinely different authority over
overlapping data. The closest is HA-replica staleness (one logical store, lagging
replicas). Sieve's mechanism is right (redirect reads to a different backend
mid-run) but its oracle assumes a single ground truth. **The per-read authority
assertion we need does not exist in any of these systems.** We would be inventing
it — which, given that `ReplicationClass` and `ResourceProvenance` already exist in
our type system, is a small invention rather than a research project.

### S.2 What should be generic, what should be per-resource

The literature's unanimous factoring (§C.5), instantiated for our crates:

**Generic infrastructure (write once, in `flotilla-resources/src/test_support/`
and, for the ownership piece, in the write path itself):**

1. **A declared field-ownership table, enforced at the write path.** Per resource
   type, a mapping from field path → owning writer role, checked on write with a
   loud error. Not advisory, and with **no `force` parameter** — §A.1 is a
   thoroughly documented account of what happens when you provide one. The
   storage shape already exists in `MergeMetadata.fields`; what is missing is a
   *writer role* alongside the causal dot, and a check.
2. **A transition-system harness that sequences.** Extend the existing
   `LivenessEnrollment` from a straight-line pass loop to a driver over a
   `Transition` vocabulary: `Reconcile`, `ExternalSpecWrite(field, value)`,
   `Delete`, `RestartController`, `AdvanceClock(δ)`, `DropActuation`,
   `DeliverActuation`, `PartitionStore(origin_root)`. This one addition is what
   makes all three bug classes *reachable* by the harness — today none of them is.
3. **`proptest-state-machine` on top of that vocabulary**, for generation and —
   the real prize — **shrinking to a minimal failing transition sequence**.
4. **A differential oracle.** Run the transition sequence unperturbed to get a
   reference end state and per-object write counts; run it perturbed; diff, with
   calibrated masking of nondeterministic fields. Sieve found **45 of 46 bugs**
   with this and nothing else, and it needs no injection machinery to be useful.
5. **A safety-fold runner over `resource_events`.** One generic
   `fold(events, δ) -> verdict` stepping a per-resource monitor, sliceable by
   object name (§C.1 parametric trace slicing). Usable as a test oracle now and as
   a live monitor later. Constraint from §0: any accumulator must be
   reconstructible from a snapshot plus the retained suffix, because
   `resource_event_compaction` truncates the prefix.
6. **A per-read authority assertion** in the test backend: every read records
   which store it resolved against; the harness fails a run where a read resolved
   against a non-authoritative store for that `ReplicationClass`.

**Per-resource data (small, declarative, one file per resource type):**

- The **phase transition table as data** (§C.4), validated for reachability and
  termination, and reused as the monitor δ.
- The **field-ownership table**.
- **Safety predicates** in the existing `FixpointPredicate` style, plus the ones
  our bug classes demand: "a lease is never released while its holder is pending
  finalization", "a deleted object is never recreated by a recovery path", "a
  field owned by role R is never written by role S".
- **`Sometimes`-style coverage assertions** (FDB's `TEST()` macro, Antithesis's
  sometimes-assertions, stateright's `Expectation::Sometimes`) — the cheapest
  idea in this document, and the one that tells us whether a fixture actually
  reaches the interesting states rather than passing vacuously.

**Not per-resource, and worth stating explicitly:** the temporal reasoning, the
sequencing, the shrinking, the diffing, and the fold. If we find ourselves writing
a second one of any of those, the factoring is wrong.

### S.3 Recommendation: what to build, in order

Ordered by (bugs caught) ÷ (cost), with our starting position taken into account.

1. **Declare and enforce field ownership on the write path.** This is the only
   item that is a *fix* rather than a *detector*, and class 1 has no detector in
   the literature worth relying on. Enforce it, do not advise it; refuse the
   write, do not merge it. The k8s lesson is unambiguous: an advisory mechanism
   with a `force` escape hatch degrades to universal forcing.

2. **Add the transition vocabulary to the liveness harness** —
   `ExternalSpecWrite`, `Delete`, `RestartController`, `AdvanceClock`,
   `PartitionStore`, plus the existing reconcile/actuation steps — and keep the
   scenarios hand-written for one iteration. This is cheap, it fits the existing
   traits, and it makes all three bug classes reachable. Write the three
   dogfooding bugs as explicit sequences first; they should fail.

3. **Add Sieve's two differential oracles** over that vocabulary: end-state field
   diff and per-object write-count diff against an unperturbed reference run.
   45/46 bugs, no injection machinery, and the nondeterminism-masking calibration
   is roughly fifty lines.

4. **Add Acto's rollback differential oracle**: after driving into an error state,
   roll back to the previous desired state and assert the result matches the
   previous system state. It caught **10 of 10** recovery-failure bugs, and
   recovery failure is our class 2.

5. **Swap hand-written sequences for `proptest-state-machine`.** Same engine, same
   invariants, generated sequences and automatic shrinking. Do this *after* 2-4 so
   that the vocabulary and the oracles are already right — generating over a bad
   vocabulary just produces noise faster.

6. **Add the per-read authority assertion** for class 3. Small, invented here,
   and the only thing on this list with no prior art to copy.

7. **Make the phase transition table data**, validate it, and reuse it as the
   monitor δ over `resource_events`. This is the "catamorphism-ish" answer the
   question was reaching for, and it is real — but it is worth doing *after* the
   harness sequences properly, because a monitor over traces the harness cannot
   generate is a monitor with nothing to watch.

8. **Adopt `turmoil` for `flotilla-transport` peer/federation tests** if and when
   multi-host bugs justify it. Independent of everything above.

### S.4 What is overkill for a five-crate Rust codebase

**Anvil-style verification: not now, but steal three things for free.** The
numbers are unambiguous — 4.5×-7.4× proof-to-code, ~2.5 person-months for the
first controller. Two mitigations do apply to us specifically: **67% of Anvil's
trusted code is Kubernetes wrapper types we would not need** (we own our resource
types and could define them Verus-friendly from the start), and the second and
third controllers cost ~2 person-weeks once the strategy exists. But that is still
the wrong order of magnitude for a codebase this size at this stage. What to take
for free, today, at zero proof cost:

- **The ESR statement itself** — `∀d. □(□desire(d) ⇒ ◇□match(d))` — as *written
  documentation of what our reconcilers promise*. Fig. 5's four cases (never
  matches / matches then deviates / satisfied / vacuous) is a free thinking tool,
  and the "matches then **deviates**" case is exactly the class-1 shape our
  current convergence-only assertions cannot see.
- **The environment model as prose**: an asynchronous network, a store with
  version checks, other writers, clients changing desired state at any time,
  crashes that lose all in-memory state. Writing that down is most of the value of
  building it.
- **The proof decomposition as a test decomposition**: current reconciliation
  terminates from *any* internal state; a new reconciliation restarts; from the
  initial internal state it realizes the desired state. Those are three
  independently testable properties, and only the first is currently covered.

**madsim: no.** Its purpose is recovering determinism over task scheduling and
background loops; we determinised structurally with the step driver. A doubled
build matrix and `[patch.crates-io]` on `getrandom`/`quanta`/`tokio-*` to
re-solve a solved problem is a bad trade, and it degrades silently (you find out
because a seed does not replay).

**loom, shuttle, kani for reconcilers: no.** Wrong granularity (atomics and locks)
or no concurrency support at all. Shuttle is the right reach if we hand-roll a
lock-free structure in cleat's VT engine.

**stateright: targeted, not blanket.** Our `reconcile()` purity means the `Model`
impl is genuinely close, and `Sometimes` is worth having. But the state space of a
store full of strings and UUIDs needs `within_boundary()` plus real name
abstraction, and `Eventually` is weak without fairness. Reserve it for one or two
protocols where exhaustive beats random — convoy lifecycle, replication
convergence.

**A full Sieve/Acto port: no — but the oracles are not the port.** Sieve is ~8,600
LOC and Acto ~17,800 across two languages, most of it deployment orchestration
against real clusters. The two differential oracles are a few hundred lines
against a store we already own and can drive deterministically. Take the oracles,
leave the infrastructure.

### S.5 The one-paragraph answer to the original question

The proof machinery should be **generic, and it should be a fold** — that instinct
is right and is a theorem, not an analogy: monitor synthesis produces a minimal
DFA (Bauer/Leucker/Schallhart), safety properties are exactly the prefix-closed
ones (Alpern & Schneider), and `evolve` in the decider model *is* δ (Chassaing).
Every community that has faced this question — runtime verification, model
checking, property-based testing, deterministic simulation, event sourcing —
converged on **one generic engine plus a per-system property specification**, and
that is already our architecture. But two caveats matter more than the
construction. First, **liveness does not fold**; what we can check on a finite
trace is bounded convergence, and the harness should say so. Second, and more
important, **no amount of proof machinery substitutes for the missing declaration
in class 1** — you cannot fold your way to "who owns this field" if nobody ever
said. The literature is unusually clear here: Anvil assumes multi-writer conflict
away, SSA makes it advisory and then tells controllers to force. The single
highest-value change is not a verification technique at all. It is declaring field
ownership and enforcing it at the write path, and then using the generic
transition-system harness to prove the enforcement holds under restart, deletion,
and federation.
