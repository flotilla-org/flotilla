# Control-plane eventing and subscription prior art

**Date:** 2026-07-29

**Issue:** [#1264](https://github.com/flotilla-org/flotilla/issues/1264)

**Related:** [#1233](https://github.com/flotilla-org/flotilla/issues/1233),
[#1262](https://github.com/flotilla-org/flotilla/issues/1262), and
[ADR 0024](../../adr/0024-declared-state-machines-and-field-ownership.md)

**Status:** Research recommendation, not an ADR

## Recommendation

Flotilla should build subscriptions as a **level-triggered wake mechanism over
the existing resource event log**, not as a second message bus and not as an
exactly-once command-delivery system.

The word “event” currently hides three different things. They should be named
separately:

1. A **resource mutation** is the raw, durable store fact: an object was added,
   modified, or deleted at an origin cursor. These records already fall out of
   the resource store. They are replication evidence and the complete input for
   replay and ADR 0024 monitors; they are too low-level to be the workflow
   vocabulary.
2. A **semantic transition** is a typed interpretation of one or more resource
   mutations: `ChangeRequestBecameConflicting`, `ReviewArrived`,
   `ChecksBecameFailing`, or `ConvoyEnteredLanding`. These should normally be
   derived by versioned, deterministic interpreters from authoritative before
   and after state. Producers should not separately emit a second assertion of
   a fact already present in resource state.
3. An **explicit occurrence** is a fact that cannot be reconstructed from
   resource state: a delivery was attempted, an undeclared transition was
   refused, an external webhook delivery was received, or a human explicitly
   requested a wake. These must be admitted explicitly to the same durable
   event substrate, for example as typed append-only resource records, with
   identity, attribution, and causation. They must not live only in a tracing
   log or transient queue.

A subscription selects typed semantic transitions or explicit occurrences,
scopes them to subjects, and maps them to a **wake key**. The wake queue
coalesces by that key. A wake carries a high-water mark and a bounded digest of
why the target is dirty; it is not a promise to deliver every source record as
one prompt. The worker re-reads authoritative current state and reconciles.
While the worker is active, another matching event marks the same key dirty and
causes at most one further wake after the worker finishes. This is the
Kubernetes keyed-workqueue pattern.

Delivery should be **at least once with idempotent admission**. Persist a stable
`wake_id`, retry until the target has durably admitted it, and make repeat
admission a no-op. Do not claim exactly-once execution across a session resume,
agent process, forge, or other external side effect.

Repeated observations of the same condition should belong to a durable,
domain-keyed **episode**. An episode opens when its normalized predicate becomes
true, remains open through repeated matching mutations and retries, and closes
when that same subject-specific predicate is no longer true. A later false →
true transition opens a new episode. Delivery state and episode state are
separate: an episode can resolve before delivery, and that fact must be recorded
rather than manufacturing a stale prompt.

CloudEvents is worth adopting as the **interchange envelope at external
boundaries** and as naming guidance for the internal envelope. It is not worth
turning Flotilla’s typed Rust event vocabulary into arbitrary CloudEvents JSON.
CloudEvents standardizes an event’s identity and context; it deliberately does
not supply ordering, persistence, subscription, delivery, batching, or episode
semantics.

## The proposed vocabulary

The following is a conceptual shape, not a wire-format ruling. Names are chosen
to keep the three layers difficult to conflate.

```rust
enum EventRecord {
    ResourceMutation(ResourceMutation),
    ExplicitOccurrence(EventEnvelope<ExplicitOccurrence>),
}

struct ResourceMutation {
    origin: OriginRoot,
    cursor: OriginCursor,
    operation: ResourceOperation, // Added | Modified | Deleted
    subject: ResourceRef,
    object: ResourceImage,
    admitted_at: Timestamp,
    writer: WriterIdentity,
}

struct EventEnvelope<T> {
    id: EventId,
    source: EventSource,
    event_type: EventType,       // versioned, reverse-DNS-style name
    subject: SubjectRef,         // source-qualified canonical identity
    occurred_at: Option<Timestamp>,
    recorded_at: Timestamp,
    cause: EventCause,
    data: T,                     // closed, typed Rust enum/struct
}

enum EventCause {
    StoreDelta {
        origin: OriginRoot,
        from: Option<OriginCursor>,
        through: OriginCursor,
        interpreter: InterpreterVersion,
    },
    ExternalDelivery {
        provider: ProviderIdentity,
        delivery_id: String,
    },
    Command {
        command_id: CommandId,
    },
}
```

`ResourceMutation` is the stored alphabet. A pure interpreter produces the
subscription alphabet:

```rust
enum SemanticTransition {
    ResourceEnteredPhase {
        subject: ResourceRef,
        machine: MachineId,
        from: Phase,
        to: Phase,
    },
    ConditionChanged {
        subject: SubjectRef,
        condition: ConditionType,
        from: ConditionState,
        to: ConditionState,
    },
    IntegrationFactChanged {
        subject: SubjectRef,
        fact: IntegrationFactType,
        from: IntegrationFactValue,
        to: IntegrationFactValue,
    },
}

enum ExplicitOccurrence {
    ExternalDeliveryReceived { provider: ProviderIdentity, delivery_id: String },
    WakeRequested { requested_by: PrincipalRef, reason: String },
    WakeAdmitted { wake_id: WakeId, target: WakeTarget },
    WakeDeliveryFailed { wake_id: WakeId, attempt: u32, error: ErrorSummary },
    RuleViolated { rule: RuleId, writer: WriterIdentity, attempted: Change },
}
```

The initial closed vocabulary should include the acceptance cases, not a generic
“field path changed” escape hatch:

- convoy and vessel phase transitions declared by their state-machine tables;
- named condition transitions;
- change-request state, mergeability, target, checks, and review facts;
- issue state/label/assignment facts where a workflow actually subscribes;
- terminal-session availability and message-admission facts;
- explicit rule violations and wake-delivery outcomes.

The generic raw `Modified` record remains available to infrastructure,
diagnostics, replication, and monitors. It should not be a normal workflow
trigger because timestamp-only refreshes and unrelated status changes would
otherwise become behavior.

### Stable identity for derived transitions

A semantic transition needs a stable identity even when it is not a second
source-of-truth record. Derive it from:

```text
(origin, through_cursor, interpreter_version, transition_ordinal)
```

The interpreter version is part of workflow-definition compatibility.
Historical definitions continue to use the version they were admitted with.
A materialized semantic-event index is allowed for query and delivery
performance, but it is not an independently-authored stream. It is rebuildable
within the supported replay horizon from a checkpoint or snapshot plus the
retained suffix. A transition that must remain addressable beyond that horizon
needs a durable semantic checkpoint, occurrence, or episode record.

An explicit occurrence instead receives its identity at admission. For an
external event, preserve the provider’s delivery identity as causation and
derive a Flotilla event ID from `(provider source, provider delivery id)`.
GitHub explicitly keeps `X-GitHub-Delivery` constant across redelivery, making
that pair the correct exact-duplicate key
([GitHub webhook best practices](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks#use-the-x-github-delivery-header)).

### Subscription, episode, digest, and wake

```rust
struct EventSubscription {
    id: SubscriptionId,
    vocabulary: InterpreterVersion,
    selector: EventSelector,
    episode: EpisodePolicy,
    coalescing: CoalescingPolicy,
    target: WakeTarget,
}

struct EventSelector {
    event_types: NonEmptySet<EventType>,
    subject_scope: SubjectSelector,
    predicate: TypedPredicate,
}

struct EpisodePolicy {
    key: EpisodeKeyExpr,
    opens_when: TypedPredicate,
    closes_when: TypedPredicate,
}

struct EventEpisode {
    id: EpisodeId,
    subscription: SubscriptionId,
    key: EpisodeKey,
    opened_through: CursorSet,
    last_matched_through: CursorSet,
    state: EpisodeState, // Open | Resolved | Cancelled
    resolution: Option<EpisodeResolution>,
}

struct PendingWake {
    wake_id: WakeId,
    wake_key: WakeKey, // (subscription, target, episode-or-digest-key)
    target: WakeTarget,
    cursor_range: CursorRangeSet,
    episode: Option<EpisodeId>,
    digest: EventDigest,
    state: WakeState, // Pending | Admitted | Superseded | Failed
    attempts: Vec<WakeAttempt>,
}
```

`CursorSet` is deliberately plural. Ordering is defined within an origin log;
federation does not invent a total order between independent roots. A
cross-origin subscription advances a vector of origin cursors and deduplicates
derived transitions by their origin-qualified IDs. The interpreter runs against
the authoritative stream for the resource’s replication class. Running the
same interpreter over replicas must not create new logical event identities.

`TypedPredicate` means a declared, reviewable predicate over a closed event and
resource schema. Argo Events demonstrates the reach of expression, data,
context, time, and script filters, but Flotilla’s control-plane substrate should
begin with typed predicates rather than embedding Lua/JQ/string field paths in
workflow definitions. An extensible expression language can be added later
without weakening the core vocabulary.

## Derived transitions versus explicitly emitted events

“The resource store is event-sourced” answers only the first third of the
question. It gives Flotilla durable object mutations with cursors. It does not
make a semantic vocabulary, episode boundaries, or delivery outcomes free.

The correct split is:

| Fact | Representation | Why |
|---|---|---|
| `Checkout` status changed | raw `ResourceMutation` | already a store fact |
| mergeability changed `Mergeable → Conflicting` | derived semantic transition | deterministic comparison of authoritative adjacent state |
| `Convoy` entered `Landing` | derived through ADR 0024’s declared table | one declared machine owns the meaning |
| review comment arrived | derived if immutable review observations are stored; explicit external occurrence until they are | must not disappear because a projection only retains “latest review” |
| GitHub webhook was received | explicit occurrence | receipt is not GitHub domain state |
| crew wake was admitted | explicit occurrence | externally consequential delivery fact |
| undeclared phase edge was attempted | explicit occurrence | the refused write will not appear as resource state |

This avoids two failure modes:

- **Double assertion:** a producer updates `mergeability=Conflicting` and also
  emits `ChangeRequestBecameConflicting`, but a crash or later code change lets
  the two disagree.
- **State-only amnesia:** a producer stores only the latest review/check state,
  so an occurrence that matters to a workflow cannot be reconstructed after
  compaction or overwrite.

If an operation changes state and records a non-derived occurrence, admission
must append them atomically. Otherwise a crash can expose the state change
without its non-reconstructable audit fact, or the occurrence without the
state it claims caused it.

Derivation is versioned because replaying old mutations with new comparison
logic can produce different semantic transitions. Compaction means a transition
interpreter, like an ADR 0024 monitor, must be reconstructible from a snapshot
plus retained suffix. If the required “before” state is not reconstructible,
the transition is not safely derivable and must be explicitly retained.

## Prior art

### Kubernetes: watch is an invalidation stream; reconciliation is level-triggered

Kubernetes controllers watch current cluster state and try to move it toward
desired state
([Kubernetes controller model](https://kubernetes.io/docs/concepts/architecture/controller/)).
Its API conventions make the stronger point: reconciliation is level-based,
not edge-based, and controllers should act on the latest state rather than
requiring every intermediate value
([Kubernetes API conventions](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-architecture/api-conventions.md#spec-and-status)).

The watch protocol is a cursor over a finite history. The normal client pattern
establishes state and its collection `resourceVersion`, then watches changes
after that version. If the requested history has expired, the server can return
`410 Gone`; the client must clear its cache, perform a fresh list, and restart
the watch from the list’s `resourceVersion`
([Kubernetes API concepts](https://kubernetes.io/docs/reference/using-api/api-concepts/#efficient-detection-of-changes)).
The re-list exists to restore correct current state after the history gap. It
does not reconstruct every edge that happened during the gap.

Two details prevent common misreadings:

- Initial watch state may be represented as synthetic `ADDED` records, followed
  by a `BOOKMARK`. Those `ADDED` records are cache bootstrap, not claims that
  each object was created after subscription; bookmarks mark cursor progress
  and are not guaranteed on a schedule
  ([streaming lists](https://kubernetes.io/docs/reference/using-api/api-concepts/#streaming-lists),
  [watch bookmarks](https://kubernetes.io/docs/reference/using-api/api-concepts/#watch-bookmarks)).
- A client-go informer **resync** sends an update notification for every object
  already in its local cache and performs no authoritative-store interaction.
  `HasSynced`, by contrast, means the cache has received at least one full list
  and is explicitly unrelated to resync
  ([client-go `SharedInformer`](https://github.com/kubernetes/client-go/blob/v0.34.1/tools/cache/shared_informer.go#L150-L195)).

The informer cache is eventually consistent. For one object it can observe an
ordered subsequence of authoritative states—intermediate states may never be
seen—and it makes no cross-object ordering promise
([`SharedInformer` contract](https://github.com/kubernetes/client-go/blob/v0.34.1/tools/cache/shared_informer.go#L40-L74)).
This rules out edge-dependent correctness for a Flotilla workflow wake. A wake
can say “this subject may now need work”; the worker must inspect the level.

Kubernetes’ keyed workqueue is the closest precedent for batching. Repeated
`Add(key)` calls coalesce while the key is dirty. If the key becomes dirty
again while it is processing, `Done(key)` queues it once more
([client-go workqueue `Add` and `Done`](https://github.com/kubernetes/client-go/blob/v0.34.1/util/workqueue/queue.go#L215-L293)).
That is precisely the desired invariant:

```text
many changes before work starts → one wake
changes while work is running   → one more wake afterward
no changes                      → no periodic prompt required
```

**Take:** copy level-triggered reconciliation, list/watch gap recovery, and
keyed dirty-bit coalescing. Do not confuse informer resync with a server re-list,
and do not make correctness depend on seeing every raw watch edge.

### Argo Events: useful declarative routing, weak domain episodes

Argo Events cleanly separates:

1. an `EventSource` that consumes an external source and publishes a CloudEvent;
2. an `EventBus` that transports it;
3. a `Sensor` that resolves named event dependencies; and
4. `Trigger`s that perform outputs
   ([EventSource](https://argoproj.github.io/argo-events/concepts/event_source/),
   [EventBus](https://argoproj.github.io/argo-events/concepts/eventbus/),
   [Sensor](https://argoproj.github.io/argo-events/concepts/sensor/),
   [Trigger](https://argoproj.github.io/argo-events/concepts/trigger/)).

Filtering can happen before publication at the EventSource or at a Sensor
dependency. Sensor filters include expression, data, script, context, and time
filters; errors count as false, and filter groups can be ANDed or ORed
([EventSource filtering](https://argoproj.github.io/argo-events/eventsources/filtering/),
[Sensor filters](https://argoproj.github.io/argo-events/sensors/filters/intro/)).
Trigger conditions are Boolean expressions over named dependencies and may be
reset on a schedule to avoid combining facts from different logical windows
([trigger conditions](https://argoproj.github.io/argo-events/sensors/trigger-conditions/)).

Argo also has an explicit coalescing rule. For dependency condition `A && B`, if
`a1` through `a10` arrive before `b1`, the trigger uses `a10` with `b1` and
drops `a1` through `a9`
([multiple dependencies](https://argoproj.github.io/argo-events/sensors/more-about-sensors-and-triggers/#multiple-dependencies)).
This is a latest-per-dependency digest, not a promise to process every event.

Its delivery documentation is a useful warning. With NATS Streaming, order is
not guaranteed and delivery is at least once. The Sensor keeps only a
five-minute in-memory cache of event IDs to suppress duplicates; the docs call
the result “exactly once” in almost all cases while explicitly retaining the
pod-death failure window
([delivery order and guarantee](https://argoproj.github.io/argo-events/sensors/more-about-sensors-and-triggers/#events-delivery-order)).
Trigger retries are off by default because a Sensor cannot know whether
repeating an arbitrary external side effect is safe
([trigger retries](https://argoproj.github.io/argo-events/sensors/more-about-sensors-and-triggers/#trigger-retries)).
Kafka EventBus mode adds durable trigger/action coordination topics and stronger
duplicate suppression, but that is bus-specific machinery
([Kafka EventBus topics](https://argoproj.github.io/argo-events/eventbus/kafka/#how-each-topic-is-used)).

**Take:** copy declarative dependencies, filtering, Boolean conditions, and
latest-per-key coalescing. Do not copy transport-specific “almost exactly once,”
an in-memory dedup window, or time reset as the identity of a domain episode.

### CloudEvents: adopt the context model, not a generic internal payload

CloudEvents defines an event as a record of an occurrence plus context and
separates that event from the transport message. A single occurrence may
produce more than one event
([CloudEvents 1.0 specification](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md#terminology)).

Every CloudEvent carries `id`, `source`, `specversion`, and `type`.
`source + id` must be unique for a distinct event; a resend may keep the same ID
and consumers may treat identical pairs as duplicates. Optional `subject`
supports routing without parsing the payload; `time`, `dataschema`, and
`datacontenttype` describe occurrence time and data
([required attributes](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md#required-attributes),
[optional attributes](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md#optional-attributes)).
The spec supports structured, binary, and batch message modes, but events in a
batch remain independent
([message model](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md#message)).

This maps well:

| CloudEvents | Flotilla |
|---|---|
| `source` | authoritative origin/provider context |
| `id` | origin-qualified mutation, derived-transition, or explicit-occurrence ID |
| `type` | versioned closed event variant |
| `subject` | source-qualified `ResourceRef` or external canonical identity |
| `time` | occurrence time, distinct from admission time |
| `data` | typed Rust payload |

CloudEvents does not define log cursors, ordering, replay retention, delivery
acknowledgement, subscription expressions, episodes, or wake admission. Those
remain Flotilla contracts. Full CloudEvents JSON internally would also cut
against ADR 0001’s typed-struct spine.

**Take:** support lossless CloudEvents import/export and align the envelope.
Keep internal payloads closed and typed, and carry Flotilla cursor, causation,
writer, and authority metadata alongside the CloudEvents-compatible core.

### GitHub webhooks and Actions: notifications are not authoritative history

GitHub asks webhook receivers to return success within ten seconds and recommends
queuing work asynchronously. It does **not** automatically redeliver failed
deliveries
([handling failed deliveries](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries)).
An operator or scheduled recovery job can redeliver deliveries retained from
the past three days
([redelivering webhooks](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks)).
The redelivery keeps the original `X-GitHub-Delivery` identifier
([webhook best practices](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks#use-the-x-github-delivery-header)).

These semantics mean a webhook is:

- a useful low-latency invalidation and an auditable delivery occurrence;
- deduplicable across replay by its delivery GUID;
- not a completeness guarantee, because failed delivery requires separate
  recovery and the replay window is finite;
- not the authoritative statement that a pull request is currently
  conflicting, approved, or merged.

GitHub Actions reinforces the distinction between replay and re-evaluation.
Re-running a workflow preserves the original event’s `GITHUB_SHA` and
`GITHUB_REF`; it does not manufacture a new current-state event
([re-running workflows](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/re-run-workflows-and-jobs)).
Actions concurrency can retain only one pending run per key and replace an
older pending run with a newer one
([Actions concurrency](https://docs.github.com/en/actions/concepts/workflows-and-actions/concurrency)).
That is useful supersession, but not a digest: the cancelled run’s payload is
not automatically merged into the survivor.

**Take:** persist GitHub receipt with its GUID, then refresh and store
authoritative GitHub state. Derive workflow transitions from those stored
observations. Polling/re-list remains the gap-recovery path. Never model webhook
receipt itself as “the PR is conflicting,” and never use Actions-style pending
run cancellation as event aggregation.

### Temporal Signals: durable admission, separate from processing

Temporal distinguishes asynchronous Signals, read-only Queries, and synchronous
tracked Updates. Signals change a running workflow but return no result;
Updates can be validated and awaited
([Temporal message passing](https://docs.temporal.io/encyclopedia/workflow-message-passing)).

For a Signal sent by a client, the call returns when the server accepts it, not
when workflow code processes it, and a `WorkflowExecutionSignaled` event is
written to the workflow’s Event History
([Temporal Go SDK message passing](https://docs.temporal.io/develop/go/workflows/message-passing#send-a-signal)).
That history is a durably persisted append-only log used to recover execution
after crashes
([Temporal Event History](https://docs.temporal.io/workflow-execution/event#what-is-an-event-history)).
The service request includes a `request_id` specifically to deduplicate sent
Signals
([Temporal API](https://github.com/temporalio/api/blob/master/temporal/api/workflowservice/v1/request_response.proto#L3634-L3653)).
Signals target open executions; Signal-With-Start is the separate primitive for
“signal the running workflow, otherwise start one.”

This is the right separation for Flotilla:

- append/admit the wake durably;
- make admission idempotent by request ID;
- do not equate admission with handling;
- choose separately whether an absent/cold target should be started, resumed,
  replaced, or held.

Temporal’s Signal payload is a command to workflow code. Flotilla’s normal wake
should be weaker: “subscription S is dirty through cursor C; reconcile current
state.” A human-authored prompt or imperative request belongs in an explicit
occurrence, not a derived state transition.

### Exactly once stops at the external side effect

Kafka’s own design documentation is unusually direct: at-least-once consumption
can repeat processing after a crash; a primary key often makes the result
idempotent. Kafka can make input offsets and Kafka output atomic, but exactly-once
delivery to another destination generally requires that destination’s
cooperation
([Kafka message-delivery semantics](https://kafka.apache.org/42/design/design/#message-delivery-semantics)).

A Flotilla wake crosses exactly those uncoordinated boundaries: resource store,
daemon, terminal-session adapter, agent harness, forge, and sometimes a human.
No shared transaction covers them. “Exactly once” would therefore either be
false or require a distributed protocol disproportionate to a wake whose
handler is already a reconciler.

**Take:** persist before delivery, retry on ambiguous outcome, deduplicate at
admission, and require idempotent reconciliation and actuation keys. Expose
attempts and outcomes so ambiguity is visible.

## Batching and digest semantics

Coalescing is a correctness-preserving optimization only because the consumer
is level-triggered. The durable source remains the log.

Recommended behavior:

1. Each matching transition advances the subscription’s **seen cursor set**.
2. It opens or updates the matching episode and marks its `WakeKey` dirty.
3. If no wake is pending or processing, create one after an optional short
   debounce, bounded by a maximum delay.
4. If a wake is pending, extend its `through` cursor and rebuild its digest.
5. If it is processing, set `dirty_after_claim=true`.
6. On admitted completion, advance the **handled cursor set**. If dirty, create
   exactly one successor wake.

The digest is a deterministic projection of `(handled, through]`, not mutable
prompt text accumulated by append. It should contain:

- first and last matching cursor/time;
- counts by event type;
- latest transition per semantic subject/fact key;
- opened, still-active, and resolved episodes;
- a bounded list of source event IDs for drill-down;
- a truncation flag and query handle when the bounded summary omits detail.

The prompt renderer reads this typed digest and current state. The eventing
substrate should not store agent prose as its canonical payload.

A debounce window may let “review + failed checks + conflict” arrive in one
wake, but correctness must not depend on their arriving inside that window. The
dirty-bit successor rule closes the race.

## Deduplication and episodes

Three identities solve three different duplicate classes:

| Identity | Suppresses |
|---|---|
| `EventId = source + source event identity` | exact transport/replay duplicate |
| `WakeKey = subscription + target + episode/digest key` | several relevant events before one reconciliation |
| `EpisodeKey = subscription + normalized domain subject/condition` | repeated observations of one continuing situation |

For the [#1150](https://github.com/flotilla-org/flotilla/issues/1150) conflict
case:

```text
EpisodeKey =
  (subscription=rebase-on-conflict,
   convoy,
   repository,
   change_request_id,
   condition=conflicting)
```

`Open → Conflicting` opens the episode. Repeated polls, webhook redelivery,
timestamp-only rewrites, and other changes while the same PR remains
conflicting update the existing episode and wake digest. A stored observation
that the same PR is mergeable/merged/closed resolves it. A later transition
back to conflicting opens a new episode with a new ID.

Resolution is scoped to the episode’s recorded subject identities. An unknown
or unrelated checkout must not strand it. This generalizes the useful part of
the episode sketch reviewed and then removed from
[#1228](https://github.com/flotilla-org/flotilla/pull/1228): one active
undelivered/unresolved episode at a time and idempotency by stable message ID,
without hard-coding “rebase,” convoy-wide resolution, or a particular terminal
target into a reconciler.

Delivery is orthogonal:

- If the episode resolves before the wake is admitted, mark the wake
  `Superseded` and record `ResolvedBeforeDelivery`; do not send stale work.
- If delivery was admitted and the episode remains open, repeated delivery with
  the same `wake_id` is an idempotent no-op.
- If the target disappears, target policy decides whether to hold, resume a
  cold session, provision a new actor, or cancel. Episode identity does not
  change merely because routing changed.
- A delivery acknowledgement proves admission to the target’s durable turn
  state, not that the requested work succeeded. Success is observed as later
  resource state and resolves the episode.

Avoid arbitrary time-window dedup as the primary rule. Time is appropriate for
debounce, rate limiting, retention, and escalation. It cannot tell whether two
conflict observations concern one continuing conflict or two conflicts
separated by a successful rebase.

## The ADR 0024 connection: same event substrate, different consumer

[ADR 0024](../../adr/0024-declared-state-machines-and-field-ownership.md)
declares state-machine tables and a generic safety fold over
`resource_events`. Its
[runtime-verification research](2026-07-29-state-transition-verification-prior-art.md#c1-a-monitor-is-a-fold--this-is-a-theorem-not-a-metaphor)
shows why a monitor is a fold over a complete, correctly sliced trace.

The eventing substrate and the monitor substrate should share:

- raw origin-authored resource mutations and explicit occurrences;
- origin-qualified identities and cursor sets;
- snapshot-plus-retained-suffix reconstruction;
- typed, versioned transition interpreters;
- subject slicing and authority/replication-class rules;
- a single query path for replay and live tailing.

They must **not** share loss semantics:

| Consumer | Required input | May coalesce? | Output |
|---|---|---|---|
| replication/materialization | raw mutation log in origin order | no | current replica/view |
| ADR 0024 safety monitor | complete relevant per-subject trace | no | verdict/violation occurrence |
| workflow subscription | typed semantic transitions/occurrences | yes, by durable wake key | dirty target + digest |
| agent/human wake delivery | persisted `PendingWake` | retry same ID | admission outcome |

A safety monitor must not consume only `PendingWake`s or prompt digests: a
coalesced stream can hide the forbidden intermediate edge that the monitor
exists to catch. Conversely, a workflow does not need one prompt for every
monitor input record.

Monitor violations are explicit occurrences because enforcement may refuse the
attempted mutation, leaving no resource delta from which to derive them. Once
recorded, they can themselves feed subscriptions—for example, waking a governor
when an enrolled controller attempts an undeclared transition. This is the same
substrate serving a different consumer, not a second telemetry pipeline.

## Failure and recovery contract

The design should state these invariants:

1. **No missed durable cause within retention.** After restart, replay from the
   handled cursor reconstructs open episodes and pending dirty keys.
2. **Gap recovery is level-safe.** If a cursor has compacted, rebuild from the
   authoritative snapshot plus suffix. Emit a visible `HistoryGapRecovered`
   occurrence. A workflow re-evaluates current predicates; it does not pretend
   to reconstruct unknowable intermediate semantic edges.
3. **Safety monitors are stricter.** A monitor whose accumulator cannot be
   reconstructed from the snapshot must report `InconclusiveHistoryGap`, never
   silently pass.
4. **Wake admission is idempotent.** The target durably records `wake_id`
   before acknowledging. Duplicate delivery returns the existing result.
5. **Handling is level-triggered.** The worker reads current authoritative
   state and makes idempotent changes; it does not trust the prompt as current.
6. **Acknowledgement advances cursors only after durable admission.** An
   ambiguous adapter result is retried with the same ID.
7. **No cross-origin total-order fiction.** Cross-root subscriptions retain
   per-origin cursors; predicates that require causal order must name an
   authoritative owner or an explicit join rule.
8. **External notification gaps heal by observation.** Webhooks accelerate
   refresh. Periodic/list recovery of provider state remains necessary.

## What not to build

- A universal `Event { type: String, data: serde_json::Value }` internal API.
- A second broker whose log competes with the resource store’s event log.
- One prompt, workflow run, or terminal resume per raw `Modified` record.
- Correctness that depends on informer resync, webhook completeness, delivery
  order, or seeing every intermediate cache state.
- A five-minute in-memory dedup cache as the definition of exactly once.
- Episode identity based only on time windows.
- A global sequence number synthesized across independent origin roots.
- Acknowledgement that means “the work succeeded” when it only means “the wake
  was admitted.”
- A force/replay button that allocates a new event ID for the same external
  delivery; replay keeps causation identity and retries admission.

## Suggested design sequence

1. Name the three layers in the resource/event APIs:
   `ResourceMutation`, `SemanticTransition`, `ExplicitOccurrence`.
2. Define the initial closed transition vocabulary from #1150, #1216, and PR
   adoption acceptance cases, backed by stored integration facts.
3. Define versioned pure interpreters and deterministic derived event IDs.
4. Add durable subscription cursors, episodes, `PendingWake`, and target-side
   idempotent admission.
5. Implement keyed dirty-bit coalescing and typed digest rendering.
6. Add webhook receipt as an explicit occurrence plus provider-state refresh;
   prove redelivery dedup by provider delivery ID.
7. Reuse ADR 0024’s replay/slicing infrastructure and prove that monitors see
   the complete trace while workflow wakes coalesce.
8. Prove the contract with restart, duplicate, compacted-cursor, rapid
   open/resolve, change-while-processing, target-disappears, and cross-origin
   tests.

## Bottom line

Flotilla’s store log supplies durable evidence, not the whole event model.
Derive typed semantic transitions from that evidence; explicitly append only
occurrences that state cannot reconstruct. Let subscriptions turn those facts
into durable, keyed, episode-aware dirty signals. Coalesce freely because the
worker reconciles current state, deliver at least once under a stable wake ID,
and make every boundary idempotent.

This gives #1150, #1216, and PR-adoption workflows a shared mechanism without
hard-coding their behavior into reconcilers. It also preserves ADR 0024’s
stronger requirement: runtime monitors consume the same log and transition
vocabulary, but never the lossy/coalesced wake stream.
