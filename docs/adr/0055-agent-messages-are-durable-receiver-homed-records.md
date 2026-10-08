# Agent messages are durable, receiver-homed records with role addresses, expectations, subject supersession and reference-gated delivery

**Status:** Accepted (owner ruling on #2678)
**Date:** 2026-10-06
**Amended by:** ADR 0059 ("holder" is retired: a role address resolves to the AgentSession its current role binding names; delivery receipts record `sent` or `received` as the AgentChannel proves; interrupts are parked until a producer exists). The dispatch ADR that was also filed as 0055 is now [ADR 0058](0058-dispatch-four-queues-one-global-decider.md) (renumbered 2026-10-08).

Agent inputs are durable **Message** resources, homed on the receiver's host and created through ordinary cross-host mutations. Replication carries the records; transport adapters attempt delivery and record evidence. Turn rules, nudges, escalations, resumes, rulings and handoffs are producers of the same records rather than independent queues. This follows the owner ruling on [#2678](https://github.com/flotilla-org/flotilla/issues/2678) and amends ADR 0050: its cross-vessel carries become typed Message references, staged and visible from the receiver's location before delivery.

Addresses are role-address strings: `project/convoy/vessel/role`, `project/role`, `fleet/role`, `principal:<name>` and system senders `system:<subsystem>`. `fleet/role` means `<fleet project>/role`: the fleet is an ordinary Project (ADR 0054), so it has no separate grammar. Convoy-relative addresses are qualified at creation; the AgentSession bound to the role is resolved at delivery. A role with no bound AgentSession leaves the record waiting with a reason and since-timestamp. Delivered records retain the actual crew and session, independent of later role-binding changes.

Messages declare notification (`none`), `reply` or `outcome` expectations, with optional deadlines. Replies link through `in_reply_to`; outcomes name condition leaves. A typed reference may be the subject, including its revision. The same sender and receiver regarding that subject revision replaces an undelivered predecessor and is suppressed while a delivered predecessor has an open expectation. A new revision is independent. A message without a subject supersedes only through an explicit predecessor ID. Content hashes and opaque producer keys do not define supersession.

Delivery progresses through accepted, waiting-on-references, deliverable and delivered, followed by satisfied, answered or outcome-met. Expired, superseded and dead-lettered are exits. Every waiting rung explains why and when it began. Typing is not delivery evidence. All deliverable messages for one AgentSession are delivered as one framed input at the turn boundary, ordered by creation; FIFO holds per sender/receiver pair. Only supervisor senders may request interrupts where supported (parked by ADR 0059 until a producer exists).

Today's adapter uses cleat's guarded send. It preserves #2705's bounded-retry contract: durable counted failures, capped backoff and a visible hold at exhaustion; an ambiguous submission cannot be typed again until evidence establishes that it was not accepted. Reference visibility, dead-letter escalation, relationship-derived reach and additional transports are separate follow-on slices (#2711–#2714), without changing the stored contract. Per ADR 0047, previous pending briefs and turn deliveries remain decodable for one roll and are adopted as Messages.

Project roles use their standing ConvoyEnsure declaration and admitted convoy, never a guessed latest generation. Relative creation uses explicit sender context; ordinary resource mutations may qualify a relative receiver from a fully qualified sender. An unresolved receiver with no declared home produces a routing refusal rather than falling back to the sender's host. An admitted receiver without a terminal still uses its placement or convoy authority and waits there.

Admission stores immutable intent, supersedes definitely pending predecessors, then initializes status. A retry repairs an intent with missing status; replay of completed admission is a no-op. The authoritative home has one writer; namespace inbox locks serialize admission within that process. Suppression returns the canonical delivered predecessor's resource. Before creation, the requested successor ID is not created. Producers must retain the returned reference rather than assume their requested ID exists.

Ordinary resource mutations have no implicit sender identity: a bare sender role is rejected. Context-aware producers qualify their sender and receiver before publishing; `accept_in_context` is the explicit contextual admission API. A fully qualified sender supplies context for a relative receiver on the ordinary path.

Routing reads a replicated view of role bindings, so its home decision may lag a binding change. Delivery resolves the binding again on every pass and requires a locally authored terminal before transport. Stale routing leaves a visible wait rather than typing at an obsolete remote session; movement to another home remains part of the receiver reach and transport follow-ons. Delivered evidence retains the original crew and session.

If a crash left the successor intent stored before suppression could be decided, recovery retains that intent as a Superseded audit record with a typed `canonical_predecessor` reference. Every replay returns that predecessor, even after its expectation closes. Normal suppression still creates no successor record; GET of a recovered partial returns its audit status rather than pretending the creation never happened.

Four-part addresses select both the named vessel and role, including when several roles share a vessel. A project role selects the unique AgentSession bound to it through its declared standing convoy; its workload's internal terminal role may differ from the project role. Multiple current bindings are refused rather than guessed.

### Owner rulings on delivery and addressing

System producers use a distinct `system:<name>` sender namespace, such as
`system:checks`. A correlated reply to a system sender is homed at the original
Message's origin. It is durable reply evidence and does not require a system
terminal. Project-role replies from a qualified crew address match the recorded
receiver incarnation, rather than requiring textual address equality.

A declared `project/role` with no bound AgentSession waits at its `ConvoyEnsure`
origin. An unbound role is not a creation refusal. Fleet and principal routing
remain deferred to #2658.

Explicit `supersedes` replaces its target, including an open delivered
expectation. Same-subject-revision repeats alone are suppressed by an open
expectation. Replacement is not transport receipt evidence.

Legacy terminal inputs and Message batches share the hold/evidence policy:
three definitely-unsent attempts, the deployed 60/120/240-second retry schedule,
five minutes before operator escalation, and fresh acceptance evidence including
changed output accompanied by fresh Working. There is one receiver-scoped
operator gate. An explicit operator batch failure preserves the uncertain input
audit while closing that intent so later inbox work may proceed.

Message reconciliation wakes on resource watches. One inbox deadline schedules
actual retries, hold escalation, and debounce completion; waiting records do not
each requeue every second. Transport calls run outside admission, and batch
intent is revalidated and persisted under admission before submission.

HTTP/Kubernetes resource versions are not assumed to fit an ordered `u64`.
Admission uses the immutable creation timestamp and name fallback on that
backend, retaining a numeric tie-breaker only for local backends.

## First implementation

Project-level roles resolve through their `ConvoyEnsure` declarations; convoy crew roles resolve through federated Convoy and TerminalSession resources. New intent follows ordinary resource mutation routing to the receiver terminal's authority, or its pinned vessel home while startup is pending. Fleet and principal role declarations follow #2658. A known receiver home waits for an unbound role; without a declared home, routing refuses rather than guessing.

Workflow episodes retain only Message admission references. The receiver persists batch membership and the bound session's identity before transport I/O. Definitely unsent attempts have a three-attempt budget with exponential backoff; uncertain input stays held and observed without resubmission. Holds resolve on acceptance evidence and never stop attention refresh. An already-idle session can receive a batch without a later idle transition, as required by #2755.

Upgrade adoption publishes receiver-scoped records before clearing old payloads. It preserves unsent FIFO order, bounded retry state and uncertain submissions. Previous launch briefs awaiting a session retain an exact incarnation and content witness. Payload-free receipt witnesses prevent a delayed replicated authority queue from retyping previously acknowledged input. These witnesses and retired queue decoders can be removed after one fleet roll. Crew list and convoy explain share a durable inbox projection, including waiting reasons, state timestamps and delivery identities.
