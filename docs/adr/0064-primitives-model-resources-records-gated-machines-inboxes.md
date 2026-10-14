# 64. The primitives model: resources, records, gated machines and inboxes

**Status:** Accepted, not yet implemented (owner grilling, 2026-10-13 and 2026-10-14). The #3108 PR stacks implement it. Until the cut-over roll, the running system follows the ADRs this one amends.
**Date:** 2026-10-14
**Amended:** 2026-10-14, with the owner's rulings on #3199 in comments [6166478785](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6166478785) (rulings 1–7) and [6166860880](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6166860880) (rulings 8–10).
**Amends:**
- [ADR 0024](0024-declared-state-machines-and-field-ownership.md): machines are declared and gated; per-field ownership is retired.
- [ADR 0047](0047-stored-data-stays-decodable-across-one-generation.md): after the cut-over, "decodable for one generation" means opening the previous generation's database.
- [ADR 0048](0048-manifest-state-lives-on-a-manifest-root-resource.md): declared definitions are homed at their ManifestRoot.
- [ADR 0055](0055-agent-messages-are-durable-receiver-homed-records.md): role inboxes, the four-state Message, Demand as a Message, and typed addresses.
- [ADR 0058](0058-dispatch-four-queues-one-global-decider.md) (the #1394 ruling): the convoy admission queue is an inbox, and whole-convoy admission becomes admission with a capacity budget.
- [ADR 0059](0059-agent-sessions-processes-and-channels.md): a Role Binding is the consumer relation on an Inbox.

**Relates to:**
- map #3108 and its tickets #3207–#3215;
- #3199, where every ruling is recorded as a comment;
- #3205 (pm connect);
- #3206 (dummy fleet);
- #3220 (Quint traces replayed against the engine);
- the explainer at https://claude.ai/artifact/WCKM5uJGrztnf1y39dfTHU.

## Context

Flotilla's control plane grew around user-visible operations and copied Kubernetes shapes. The audit behind #3199 found the following:
- **Status patches:** about 140 variants across 19 kinds. Convoy alone has 49, and they carry crew phases, turn deliveries, nudges, stall ladders and landing.
- **Conditions:** five unrelated condition structs.
- **Retry counters:** ten stored counters.
- **Copied facts:** facts mirrored from Environment to Vessel to Convoy.
- **Heartbeats:** writes that carry no new information.
- **Storage:** every object stored as JSON, compared as strings, and filtered through 192 per-field value tables.

Convoys assumed that their vessels were co-located and fixed. Many writers could set a phase, and nothing checked why.

## Decision

### Primitives

- **A resource** has its own identity that others refer to. Someone wants it to exist or watches whether it does. It has exactly one **home** host, which alone writes it.
- **A record** belongs to exactly one resource, identified by owner and position. It has no identity, watch or lifecycle of its own, and it is stored, replicated and archived with its owner. Records include:
  - deliverables (promises, submissions, retractions);
  - relations (written by the side that asserts them);
  - observations (written by their observer);
  - attempts;
  - receipts;
  - last-checked values for polled external data.
- **A judgement** is computed and never stored. Examples are ready, stalled, landing, kept, retry count, next attempt, stale and drift. Each judgement is a declared function with declared inputs, and it returns a value and the moment it could next change. The same function gates edges and renders listings, so the two cannot disagree.
- **No host writes another home's resource.** It sends a message to that home, or it publishes a record of its own that the home observes.

### Machines

- **Declaration.** States are nouns, edges are verbs, and the two never share a word. Edges carry typed arguments: one structured edge with arguments, not near-identical variants.
- **Legibility.** Every diagram must be legible in a GUI to a competent person.
- **Gates.** Each edge declares a gate, enforced on the home's write path. The edge event records what the gate read, as references and versions. Gates read only local, versioned inputs and never liveness.
- **Ends are ends.** A late edge on an ended resource is refused, with a reason returned to the caller and not recorded.
- **Attempts are a history.** Retry counts, the schedule and giving up are judgements over attempt records and a policy. Giving up records a final failure and sends a message. Each `resume` starts a new attempt window: the judgements count only attempts since the last `resume`, and older attempts stay in the history (#3214).
- **No mirrors.** Each fact has one home. Facts replicate, and judgements are computed where they are needed.
- **Replacement, not reincarnation.** When a realised thing is lost and still wanted, a new resource replaces it, and the owning relation records the swap.
- **API.** Machines are declared with a builder (`MachineDef` / `EdgeDef`) that validates the rules and renders the diagram. Edge dispatch is typed: a fieldless kind enum is derived from the edge enum, and each edge has its own argument struct. Gates and judgements read through one recording handle (#3209).

The decided machines:

| Machine | States | Edges |
|---|---|---|
| Shared lifecycle: Vessel, Environment, Checkout, Clone, ImageBuild, TerminalSession | Requested, Realised, Gone, GaveUp | `realise(identity)` Requested→Realised; `release` Requested or Realised→Gone; `vanish` Realised→Gone; `give up(cause)` Requested→GaveUp |
| Convoy | Open, Delivered, Abandoned | `deliver` (gate: every promise kept, or retracted with acceptance); `abandon(by, reason)`, always a decision by an authorised party. Nothing fails a convoy automatically. |
| Message | Pending, Awaiting, Settled, Dropped | `deliver(consumer, receipt)` to Settled when nothing is expected, otherwise to Awaiting; `settle(answered)` from Awaiting; `settle(outcome observed)` from Pending or Awaiting; `drop(undeliverable \| expired \| superseded \| withdrawn)` |
| Ensure | Unfulfilled, Fulfilled, Held, Retired | `bind(fulfiller)` when the fulfiller is Realised and can receive; `unbind(gone \| replaced)`; `give up` from Unfulfilled; `resume(by)`; `retire` |
| AgentSession | conversation: Live, Ended (completed \| lost); channel: Detached ⇄ Attached | "running" is a judgement |

An Ensure records each fulfiller before it asks for it, and `retire` releases every fulfiller it recorded, bound or not. Replacing a fulfiller means releasing the old one, observing it Gone, then creating the new one (#3214).

How the Convoy and Message machines behave:
- **A convoy ends when its work is delivered.** A crew's "done" is a submission of evidence against a deliverable, not a convoy edge.
- **Every convoy promises something.** Admission refuses a convoy with no promises, because the `deliver` gate holds trivially over an empty set (#3214). A free-form convoy, such as a navigator session, promises a decision: kept by observing its map ticket, or a similar record, change or resolve. A human sign-off verdict is the fallback.
- **A Demand is a Message that expects an outcome,** and the Demand kind retires. Escalation is `drop(expired)` plus a follow-up message.
- **An outcome observed while Pending settles the message directly** (owner, 2026-10-14). Take a demand to fix a failing build: if the build goes green before delivery, the message did its job, and the consumer never sees the turn. A receipt that arrives afterwards cannot record `deliver` on a Settled message.
- **Dropped is the state; `drop(reason)` is the edge into it.** The reason is recorded on the edge event and shown with the state.
- **A sender may withdraw.** `drop(withdrawn)` is the sender cancelling a message without a replacement. A sender that replaces a message uses `drop(superseded)`.
- **An expiry never beats a reply written in time** (#3214). `drop(expired)` also requires that the home has applied the bound consumer's host stream past the deadline; its keepalive marks make that a local, versioned input. With no consumer bound, the message expires on time. If the consumer's host stays silent, the message stays Awaiting. The listing shows that silence as a judgement, and the sender's supervisor or a human may drop the message deliberately, with the reason recorded.

How Checkouts and lends behave:
- **One Checkout per writing role.** The shared unit is the branch, not a directory. Each writing role gets its own Checkout, a worktree made from a per-host, per-repository Clone that convoys on that host share. Roles share by pushing branches, and hand over by committing and pushing.
- **A Checkout is owned by the role's owner:** the convoy for convoy roles, the Ensure for standing roles. Its home is the host it lives on. A replacement session on the same host reuses it. A replacement on another host means `release`, observing Gone, then creating a new Checkout from the last pushed commit.
- **Inside a vessel is the crew's business.** Extra branches and worktrees inside a vessel, such as a stack lead's subagents, live inside the crew's Checkout. Flotilla knows the deliverables (promised branches and change requests, observed on the forge) and the Checkout, whose release removes everything inside it.
- **A lend** lets another role, or a person, borrow a resource the owner holds: a Checkout, a process with an open port, or a jackstay stream. It is a relation asserted by the lender, with terms: who, mode (read-only or shared-write), and until a time or an event. The environment enforces the mode where it can. A co-placed coder and reviewer sharing local files is a lend, not a shared placement owner.
- **A lend gates release.** The lender's `release` is refused while an unexpired lend exists, unless the borrower has returned it or an authorised override records a reason. Return is the borrower's edge. Loss is `vanish`, with a message to the borrower.

### Roles, inboxes and delivery

- **Inboxes.** Each role instance owns an **Inbox** resource, homed with the role's owner: the convoy for convoy roles, and the project's declarations for standing roles. Messages queue there and outlive any fulfiller.
- **Standing roles** are declared in the charter and own their inboxes, whether or not anything automatic fills them. An Ensure is optional, layered on a role: it keeps an agent bound. There is no separate person kind: a person's own inbox is a declared role with one holder.
- **Addresses** are typed lists of segments over a closed kind enum: project, convoy and role to start. They are stored typed and shown as `project:flotilla/role:gui-approver` and `project:flotilla/convoy:stack-3152/role:coder`. There are two shapes to start, project/role and project/convoy/role, with no vessel segment. New kinds extend the enum without changing existing addresses.
- **Every role is declared** with a one-line purpose. A message to an undeclared role is refused at send time, with the reason.
- **Dynamic roles.** The convoy manages all its roles and inboxes at its home: declaring a role, its inbox and its brief. The workflow template says which roles may declare more (a stack lead, for example), and the supervisor may too. Role names may be dynamic (`coder-pr45`), with the role type as an attribute.
- **The brief** is the inbox's first message. A new conversation reads the inbox from the top; a resumed one continues from its own position.
- **Consumers.** A consumer of an inbox may be an **AgentSession**, a person (through a surface), or a deterministic controller. The consumer relation is the Role Binding, and convoy membership is "X consumes `project/C/role`", asserted by the convoy.
- **Several consumers claim.** Any mix of people and agents may consume one inbox. The first to act on a message claims it and settles it.
- **Attention is a message.** There is no separate human attention concept. An observer that sees something needing a hand (a permission prompt, an expiring credential) sends a message that expects the outcome to the role that must act. If it resolves unseen, the message settles from Pending. Escalation is a chain of roles. Salience becomes a judgement over pending messages. Regard stays a separate presentation concept: what someone looks at, not what waits for them. Inbox activity and workflow interaction diagrams are drawn from message records. Who may read which inboxes is still open.
- **Pull delivery.**
  1. The consumer's host replicates the inboxes it consumes.
  2. It delivers each turn through its AgentChannel, using the adapter's transport.
  3. It appends a **receipt** row on its AgentSession.
  4. The inbox's home records `deliver`, gated on observing that receipt.
- **Turn first, then receipt.** The consumer's host hands over the turn before it writes the receipt. A crash in between can repeat a turn to the same session, so adapters ignore a message ID they have already turned.
- **`drop(undeliverable)`** is gated on what the home can observe: no consumer bound past a threshold, or a delivery-failure record published by the consumer, and no receipt observed. It means "no receipt was confirmed within the policy", not "no agent saw it", because a consumer acting on a lagging replica can still turn the message.
- **At least once across a replacement** (#3214). The home cannot tell a consumer that received a message and died from one that never did, so two sessions may both turn a message across a replacement. The window is kept to that:
  - `bind` waits until every observed receipt has its `deliver` edge;
  - `deliver` accepts a receipt from a session that is no longer bound;
  - `deliver` records exactly one receiver, and later receipts are refused.
- **Unbind reactions.** The inbox's owner unbinds a consumer when it observes the AgentSession's end row, and when the consumer's host has been silent past the role's policy threshold. The silence trigger is a reaction on a judgement, never a gate, and the unbind records its reason. Without these reactions, a dead bound consumer blocks the inbox for ever.
- **The plug is the whole inbox.** An `InboxBackend` contract covers enqueue, pending-for-consumer, receipts, the edges and watch. Every implementation passes one shared contract-test suite. Resource-backed replication is the first implementation. Queue brokers such as RabbitMQ or Cloudflare Queues fit behind the same contract.
- **Filtering** chooses how a message is delivered, never whether: turn, notify or mute. Mute counts as delivered.
- **Dead letters** go to the sender's supervisor, unless the sender is that supervisor.

### Admission and capacity

- **A convoy request is a message** to `project:fleet/role:admission` that expects an outcome. Its consumer is the admission controller. Footprints and crew details are its typed body.
  - Receipt is `deliver`.
  - Position is a judgement over pending messages: class of service and holds.
  - Admitted is `settle(outcome observed)`, when the Convoy exists.
  - Refused is `settle(answered)`, with the reason as the reply.
  - A revised batch supersedes the request; a request cancelled without a replacement is withdrawn.
- **Other requests** can use the same inbox shape. There is one queue mechanism, not two.
- **Admission grants a capacity budget.** The workflow template proposes a budget, and admission may grant less.
- **Growth within the budget goes straight to placement.** The convoy asks placement for vessels directly. Placement is decided by the host that owns the capacity, because the ledger is its own. Filling a role from inside a vessel, such as a subagent, uses no budget.
- **Growth beyond the budget is a new request** to admission, in effect a readmission.
- **Warm vessels** are a host-owned placement policy, at low priority: a host keeps N warm vessels per image. Placement may hand one to a convoy as an ownership change. Its lifecycle stays Realised, and it counts against the host while idle and against the convoy's budget from handover. Pools may later prepare more, such as a build directory seeded from a recent cache.

### Storage, replication, views

**Storage.**
- **Tables from Rust types.** Data is stored as relational tables generated from the Rust types by our own derive on rusqlite (#3208). Collections are child tables, records are owner-plus-position rows, edge events are rows, frozen documents are stored by hash, and strings are interned. JSON is not the stored form.
- **Serde views.** YAML and JSON stay valid documents for disk, manifests and CLI output. Their content follows the model.
- **Shared tables.** Each kind has one table, with a home column. Only the replication applier writes rows homed elsewhere.

**Archive.**
- **Ended resources archive as a bundle.** Seven days after a resource reaches an end state, its home ships the resource with all its records (edge events, attempts, receipts, observations, deliverables) as one bundle to the archive host, then deletes them locally.
- **Standing inboxes never end.** Their messages archive one by one, seven days after they settle or drop.
- **The archive is read-only and typed,** with the same derived relational schema. Retention is indefinite by default. Per-kind deletion comes later, terminal recordings first.
- **Identity.** Archived-identity stubs live only on the archive host and the former home. Other hosts resolve an unknown name by routing to the archive. Live identity everywhere is unchanged.

**Replication.**
- **One sequence per home.** Its value is a resource's `version`. Each write appends row changes in home order. Replicas apply rows and do not re-run machines.
- **Encoding and liveness.** The stream is CBOR (ADR 0063). Its keepalive is the link-liveness signal.
- **Subscriptions.** Every host holds the identity row (kind, name, home) of every live resource. Full rows travel by subscription to coarse scopes: a project, a time window, or "related to host H". The old per-kind rules become default subscriptions.
- **Misses** are routed to the home or the archive. Gates make a home subscribe to any input they need, and never query remotely. Subscription maintenance must never dominate: subscriptions are tagged once at write time and fanned out by a bitmap lookup.

**Liveness.** Liveness is judged from the observer host's link. Observations are written only when they change. Heartbeat-only writes go away.

**Views.**
- **Resident state.** State is loaded at startup into typed resident structures and kept current from writes and the stream. Delivery to surfaces never reads the store, and SQL serves only history and archive.
- **Incremental judgements.** Judgements are maintained incrementally through a compressed-bitmap reverse index from inputs to rows and one timer queue, with Bloom pre-filters on arriving changes and per view client.
- **Surface protocol.** The named-query surface protocol stays.

**Definitions.** A definition is homed at the ManifestRoot that declares it, and edited through git. Undeclared definitions are homed where they were created. Multi-writer merge metadata and field ownership are removed.

### Not Kubernetes

Flotilla resources will not be Kubernetes CRDs. Kubernetes, if used, is an environment provider. The following are dropped:
- namespace;
- API group and version;
- the CRD files and the Kubernetes write client;
- the generic `patch-status`.

The following are replaced:
- finalizers, by a typed teardown step;
- owner references, by a parent key or a gated relation;
- about 80 `flotilla.work/*` labels and annotations, by columns and records;
- JSON-path selectors, by named queries;
- the Event kind, by edge events.

The following stay:
- the spec/status split;
- `version` and `created_at`;
- plural names and kubectl-style verbs.

### Switch-over

There is no double-write path and no long-lived branch.
1. **Build alongside.** The new code is built on main in new crates, behind one switch in the composition root.
2. **Prove it.** The new path is proven with the dummy fleet (#3206) at 100, 1,000 and 10,000 convoys, and with a shadow cluster.
3. **Rehearse the import.** Pre-roll validation (#2167) rehearses the one-shot importer against every host's live store.
4. **Cut over in one roll.** The old database file is left untouched, and rollback means returning to the previous generation.

## Consequences

- **Smaller machines.** Convoy goes from 49 patch variants to 2 edges plus records. Message goes from 11 phases to 4 states. Six kinds share one lifecycle. Every stored retry counter, condition struct, mirror and heartbeat goes.
- **One queue mechanism.** Admission is an inbox, so convoy requests, demands and crew messages share one machine, one backend and one listing.
- **Principal retires into roles.** A person acts by consuming a declared role. Attention, escalation and person-specific inboxes are messages and roles, and Regard stays as the presentation concept.
- **ADR 0055 and #1394 are amended.** ADR 0055's address forms become typed segment lists, and its `principal:` form becomes a declared role. The #1394 ruling (ADR 0058) moves from whole-convoy admission to admission with a capacity budget: growth within the budget goes to placement, and growth beyond it is a new request.
- **Crews on any host.** Crew members can be on any host and can be replaced without stranding messages. Handoffs, resumes and nudges are messages, and no convoy transition is involved.
- **The importer is the largest hand-written piece.** After the cut-over, ADR 0047 is enforced by a checked-in schema snapshot and explicit migration steps, and the golden corpus becomes a SQLite file.
- **Peer merge goes away.** The Plane-A peer-merge layer (`crates/flotilla-daemon/src/peer/`) stays bugfix-only and is deleted by this work, because subscription-based replication supersedes it.
- **CLAUDE.md changes at the cut-over roll.** Its "Development Phase" rules (serde aliases and defaults, the JSON golden corpus) change to the schema-snapshot form in the same roll.
- **Interactions are model-checked in Quint.** The exploratory specs (#3214) found the races ruled above. Minimal per-machine specs, replayed as traces against the engine (#3220), gate the implementation, which is split into PR stacks (#3215).
