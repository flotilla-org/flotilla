# 40. Object-scoped Events are a resource

Date: 2026-09-26

## Status

Accepted

## Context

Control-plane surfaces need durable, queryable diagnostic history attached to
the object it concerns. `convoy explain` must show why a convoy is stuck after
the fact; `resource get` should carry recent trouble for the object it returns;
a fleet view should let an operator scan recent activity across hosts. Transient
in-memory logs and one-shot signals cannot answer any of these: they are not
addressable, not federated, and gone by the time someone looks. Recording every
occurrence as its own row instead floods storage when a reconciler retries the
same failure on a loop.

Kubernetes already solved the same problem the same way: an Event is a
first-class, namespaced resource, deduplicated by object and reason with an
occurrence count, pruned by TTL. This ADR adopts that shape rather than
inventing a bespoke log, and does so as an ordinary resource under ADR 0001 so
it inherits get/list/watch, replication, and store authority for free.

## Decision

`Event` is a first-class, namespace-scoped resource kind
(`define_resource!(Event, "events", EventSpec, (), NoStatusPatch,
replication = ReplicationClass::HomeBoundRuntime)`). It is a recorded fact, not
a declared intent: it carries no status subresource and no state machine
(`NoStatusPatch`), consistent with ADR 0023 — an Event is evidence, never truth.

`EventSpec` is `{ regarding, reason, message, count, first_seen, last_seen,
expires_at }`, where `regarding` identifies the subject object. Recording an
`ObjectEvent { regarding, reason, message, related_labels }` **deduplicates by a
hash of `(regarding, reason, message)`**: an identical cause increments `count`
and refreshes `last_seen`/`expires_at` on the one record; a materially different
message is a distinct Event. Deduplicating on the message — not on
`(object, reason)` alone — keeps genuinely different diagnostics separate while
still folding a retrying reconciler's repeated identical failure into a single
counted row. TTL defaults to 24h (`DEFAULT_EVENT_TTL_SECONDS`), is refreshed on
each occurrence, and expired Events are pruned.

**Association labels are the related-event query seam.** An `ObjectEvent`
copies the subject object's labels into the Event's `related_labels`, so
"events about things related to X" is a label query rather than a bespoke
join. `recent_for(regarding)` answers the direct "events about this object"
lookup.

Events fold into existing surfaces rather than a new API: generic `resource
get` augments the returned object with a top-level `recentEvents` field while
the **stored** object is unchanged; `convoy explain` reads the same records;
and `flotilla events` is a fleet-wide view. Augmenting the read object was
chosen over a separate related-records envelope so a script that already reads
an object gets its recent trouble without a second call. The raw stored shape
and the read-time DTO are deliberately two types: the stored Event is the
authoritative fact, the DTO is the projection consumers see.

Replication is `HomeBoundRuntime` (ADR 0016): the origin host is authoritative
for its Events and replicas are read-only, fleet-visible projections — an Event
is host-local runtime evidence, not editable-anywhere Definition state.

## Consequences

Object-scoped diagnostic history is now durable, deduplicated, fleet-visible,
and queryable through the same get/list/watch and replication every resource
already has, with no new transport. A retrying reconciler produces one counted
row, not a flood. TTL bounds growth without an operator sweep. Because Events
are `HomeBoundRuntime`, a partitioned host still records and serves its own
Events, and other hosts read the last replicated view; there is no cross-host
authority to contend.

This follows the Kubernetes Events precedent, so it ages well toward the option
of running these as real CRDs — unlike the manifest-refusal representation under
grill in #2023, an Event is a typed resource, not controller state smuggled into
annotations.

It is distinct from ADR 0034 (decision-ledger demand shape): that governs the
shape and admission of the decision ledger, not this general-purpose object
event record. A convoy's ledger and its Events are different evidence.

Deferred, deliberately not decided here: whether ephemeral diagnostic resources
eventually warrant a **distinct relay/retention class** rather than plain
`HomeBoundRuntime` (e.g. shorter replica retention, lossy relay) — recorded as a
future fork in #1836's ledger. The 24h TTL window and the
`(object, reason, message)` dedup granularity are tunables, not contract; a
richer public reason taxonomy, if wanted, is a later addition.
