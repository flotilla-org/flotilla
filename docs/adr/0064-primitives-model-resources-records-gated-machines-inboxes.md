# 64. The primitives model: resources, records, gated machines and inboxes

**Status:** Accepted, not yet implemented (owner grilling, 2026-10-13 and 2026-10-14). The #3108 PR stacks implement it. Until the cut-over roll, the running system follows the ADRs this one amends.
**Date:** 2026-10-14
**Amended:** 2026-10-14, with the owner's rulings on #3199 in comments [6166478785](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6166478785) (rulings 1–7), [6166860880](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6166860880) (rulings 8–10), [6177162679](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6177162679) (rulings 11, 13 and 14; item 12 is a leaning, recorded under the addresses) and [6177712045](https://github.com/flotilla-org/flotilla/issues/3199#issuecomment-6177712045) (ruling 15). Amended 2026-10-18 with the owner's rulings on #3424 ([6246412148](https://github.com/flotilla-org/flotilla/issues/3424#issuecomment-6246412148), rulings 1–10): consumer identity, receipts from every consumer kind, binding sets, message content and declared-role senders, under "Roles, inboxes and delivery"; and with the owner's schema look-over of the same day on [#3453](https://github.com/flotilla-org/flotilla/pull/3453), amending rulings 1 and 2: a person's receipt is their acknowledgement or response, and the person consumer kind waits for the principals model. Amended 2026-10-20 by the owner's environment and placement grill: placement leases capacity per capacity pool, from that pool's authoritative daemon.
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
  - last-checked values for polled external data.

  Attempts and receipts are not separate records: each is an edge on the resource that made it, so every write to history goes through the one edge path (#3392 R4).

  A convoy's seats, promises, submissions and duties are records of the Convoy, not their own kinds. A generated parent key is named after its root table (#3627 look-over ruling).

  ADR 0064 seats: fan-out adds a seat under an existing one (`AddSeat`, names never reused within a generation) and `Resupervise` is the only edge that changes `supervised_by`; admission refuses unknown owners, unknown parents and cycles.

  ADR 0064 retired seats: a retired seat's name is reserved for the rest of its generation; Rollover clears retired seats, so a later generation may reuse the name, and the live record never accumulates seat history.

  [Owner ruling, 2026-10-26 (#3825)](https://github.com/flotilla-org/flotilla/issues/3825#issuecomment-6416928549):

  ADR carry: ADR 0064 (or the convoy ADR #3794 updated) records that an ended standing generation is retired and its stable name reused by the next generation, with history in the per-home log; superseded if #3911 lands.

  ADR 0064 Convoy records: a change-request observation finds its submission through a full index on the submission address (one lookup per event); there is no partial index on open convoys.

  ADR 0064 promise labels: a promise's label is stored per convoy and is the name crews and views use; kinds stay declarations.

  ADR 0064 promise labels: a promise label is the permanent name of what is delivered, unique in its convoy and never reused or changed by reassignment; the owner is separate data. Seat-qualified labels exist only for imported records.

  ADR 0064 awaiting replies: the AwaitingReply record is written by the sender's home once the receiver has admitted the message, and a send is not finished until that record is written; this replaces "written in the send's transaction".

  ADR 0064 operator-level stalls: until the operator is a principal with an inbox, stalls escalated to the operator are read from the old stall status; no stall drops out of the views.
- **A judgement** is computed and never stored. Examples are ready, stalled, landing, kept, retry count, next attempt, stale and drift. Each judgement is a declared function with declared inputs, and it returns a value and the moment it could next change. The same function gates edges and renders listings, so the two cannot disagree.
- **No host writes another home's resource.** It sends a message to that home, or it publishes a record of its own that the home observes.

### Machines

- **Declaration.** States are nouns, edges are verbs, and the two never share a word. Edges carry typed arguments: one structured edge with arguments, not near-identical variants.
- **Legibility.** Every diagram must be legible in a GUI to a competent person.
- **State is a value.** A machine's state is its label plus the label's data, as in the Quint specs: a convoy is `Open { promises, quota }`. Gates and edges act on the current state, and an edge produces the next state; its declaration names the data it changes. The label is derived for listing, indexing and drawing. The edge events are an immutable history of each edge's arguments and gate reads, used only to show and audit what happened, never by a decision path. Judgements cover only genuinely derived values, such as retry count, stale, ready and running; a convoy's promise resolution and its quota in force are state (owner, #3319, 2026-10-15).
- **Gates.** Each edge declares a gate, enforced on the home's write path, or is declared ungated with a stated reason. The edge event records what the gate read, as references and versions. Gates read only local, versioned inputs and never liveness.
- **Ends are ends.** A late edge on an ended resource is refused, with a reason returned to the caller and not recorded.
- **Attempts are edges.** An attempt is an edge on the resource that made it, recording its outcome and, for a failure, whether it can be retried. Its effect keeps the current window's tally in the state (consecutive failures since the last success, and the latest failure), so retry counts, the schedule and exhaustion are judgements over that tally and a policy, and no gate replays the resource's own attempts. An Ensure's `hold` is allowed when the window's consecutive failures reach the policy's `max_failures`, or when its last failure cannot be retried; backoff timing is outside the machine. Held lasts until an authorised `resume`, which opens a new window, or a `retire`: nothing resumes on its own. Older attempts stay in the log (#3214; #3392 G10).
- **Edges about a master.** An edge may declare one argument as its master, the resource it is about; the log keeps the master's name as the entry's `about`. A master's gates and judgements read the entries about it as versioned reads: a Message's `deliver` and `drop(undeliverable)` read the `receive` entries about the message. An edge with a master may leave its state unchanged, since its entry is what it records. Where a master already names the other resource (an Ensure's fulfiller, an inbox's binding), it keeps reading that resource by reference (#3392 G11).
- **No mirrors.** Each fact has one home. Facts replicate, and judgements are computed where they are needed.
- **Replacement, not reincarnation.** When a realised thing is lost and still wanted, a new resource replaces it, and the owning relation records the swap.
- **No role names.** The engine knows only generic concepts: Role, Inbox, Message, Promise, Lend and the five machines below. Specific roles (coder, reviewer, governor, navigator, admission, gui-approver) and specific promise kinds (`pr`, `decision`, `demo-video`, `decision-ledger`) live in workflow templates and kind declarations. Their names never appear in a machine, a gate or the storage schema. They are affordances for one workflow, not substrate. Where this ADR names a role or a promise kind, it is a workflow example.
- **API.** Machines are declared with a builder (`MachineDef` / `EdgeDef`) that validates the rules and renders the diagram. Edge dispatch is typed: a fieldless kind enum is derived from the edge enum, and each edge has its own argument struct. Gates and judgements read through one recording handle (#3209). The engine lives in the backend-free `flotilla-state-machines` crate (#3319): it declares each machine's stored kind, with its kind code, indexes and storage attributes, but touches no backend (#3373 ruling 6). The only workspace crate it reaches is `flotilla-schema` (#3320), and through it `flotilla-address`, whose types it uses directly: state labels, derived from the state enum, and edge kinds are schema code enums with explicit stable codes; records are the schema's append-only `Records`; an edge event is the schema's `EdgeEvent`, whose inline document holds the edge's arguments and reads; and time is the schema's `Timestamp`, microseconds supplied by the caller: the engine never reads a clock. Admission reads the world through a recording admission handle too, so a create carries the reads it relied on (#3392 G1). Two checks hold the crate to `specs/primitives/`, both in the Quint quick tier. Full-graph conformance (#3109) checks every reachable (state, event) pair of each machine's TLC graph: the engine accepts exactly the spec's moves, reaches the same state value, and refuses with the same `Refusal` variant. Generated ITF trace replay drives the engine through each scenario.

The decided machines:

| Machine | States | Edges |
|---|---|---|
| Shared lifecycle: Vessel, Environment, Checkout, Clone, ImageBuild, TerminalSession | Requested, Realised, Gone, GaveUp | `realise(identity)` Requested→Realised; `release` Requested or Realised→Gone; `vanish` Realised→Gone; `attempted(outcome)` Requested→Requested; `give up(cause)` Requested→GaveUp |
| Convoy | Open, Delivered, Abandoned | `deliver` (gate: every promise kept, or retracted with acceptance); `abandon(by, reason)`, always a decision by an authorised party. Nothing fails a convoy automatically. |
| Message | Pending, Awaiting, Settled, Dropped | `deliver(consumer, receipt)` to Settled when nothing is expected, otherwise to Awaiting; `settle(answered)` from Awaiting; `settle(outcome observed)` from Pending or Awaiting; `drop(undeliverable \| expired \| superseded \| withdrawn)`. Settled keeps the receiver, if any; Dropped keeps the receiver and the watermark an expiry relied on, and its time is the state's since (#3392 G4, G5) |
| Ensure | Unfulfilled, Fulfilled, Held, Retired | `bind(fulfiller)` when the fulfiller is Realised and can receive; `unbind(gone \| replaced)`; `attempted(fulfiller, outcome)` Unfulfilled→Unfulfilled; `hold` from Unfulfilled once the window is exhausted; `resume(by)`; `retire` |
| AgentSession | conversation: Live, Ended (completed \| lost); channel: Detached ⇄ Attached | `receive(message, transport)` Live→Live, about the message; "running" is a judgement |

An Ensure records each fulfiller before it asks for it, and `retire` releases every fulfiller it recorded, bound or not. Replacing a fulfiller means releasing the old one, observing it Gone, then creating the new one (#3214).

How the Convoy and Message machines behave:
- **A convoy ends when its work is delivered.** A crew's "done" is a submission of evidence against a deliverable, not a convoy edge.
- **Every convoy promises something.** Admission refuses a convoy with no promises, because the `deliver` gate holds trivially over an empty set (#3214). A free-form convoy, such as a workflow's navigator session, can promise a decision (see "Promises" below).
- **Promise resolution is final.** Each convoy retains a map from promise id to Pending, Kept or Retracted. A further keep or accepted retraction on a settled promise is refused with `PromiseSettled`; Kept is final. A continuing commitment is a lend with terms, and a breach is a message to the lender's inbox. A demo promise's stable deliverable is a running demo plus a recipe to reactivate it (owner, #3220, 2026-10-14).
- **Admission grants a cap, never a reservation.** `ConvoyRequest` carries project, a nonempty map of promises with opaque kind and target, per-vessel-class quota counts and priority. A quota increase is readmission of the same convoy. Placement fits each vessel's whole request on one host; requests are never pooled across shapes or hosts. Its lease lasts until Gone. *(2026-10-20 grill: "one host" becomes the capacity pools the vessel's environment names, a host's compute and disk or a provider account's quota, each leased by that pool's authoritative daemon; see [ADR 0058, dispatch](0058-dispatch-four-queues-one-global-decider.md), 2026-10-20 amendment.)* Requests that do not fit wait in priority order, and a waiting higher-priority request that fits a pool cannot be passed there by a lower-priority request. There is no borrowing. Spend accounting at nominal unit rates remains unspecified on map #3108 (owner, #3220, 2026-10-14).
- **A Demand is a Message that expects an outcome,** and the Demand kind retires. Escalation is `drop(expired)` plus a follow-up message to the next role in the chain (see "Attention is a message" below).
- **An outcome observed while Pending settles the message directly** (owner, 2026-10-14). Take a demand to fix a failing build: if the build goes green before delivery, the message did its job, and the consumer never sees the turn. A receipt that arrives afterwards cannot record `deliver` on a Settled message.
- **Dropped is the state; `drop(reason)` is the edge into it.** The reason is recorded on the edge event and shown with the state.
- **A sender may withdraw.** `drop(withdrawn)` is the sender cancelling a message without a replacement. A sender that replaces a message uses `drop(superseded)`.
- **An expiry never beats a reply written in time** (#3214). `drop(expired)` also requires that the home has applied the bound consumer's host stream past the deadline; its keepalive marks make that a local, versioned input. With no consumer bound, the message expires on time. If the consumer's host stays silent, the message stays Awaiting. The listing shows that silence as a judgement, and the sender's supervisor or a human may drop the message deliberately, with the reason recorded.

How Checkouts and lends behave:
- **One Checkout per writing role.** The shared unit is the branch, not a directory. Each writing role gets its own Checkout, a worktree made from a per-host, per-repository Clone that convoys on that host share. Roles share by pushing branches, and hand over by committing and pushing.
- **A Checkout is held for its role's owner** (owner's schema look-over, 2026-10-22, #3719 ruling 4)**:** a Checkout is held for its role's owner (the convoy for convoy roles, the Ensure for standing roles), recorded as an indexed holder address with no ownership semantics. An Environment records its Vessel the same way. Its home is the host it lives on. A replacement session on the same host reuses it. A replacement on another host means `release`, observing Gone, then creating a new Checkout from the last pushed commit.
- **A TerminalSession is held for its vessel** (owner's schema look-over of #3720, 2026-10-22)**:** a TerminalSession records the vessel it is held for as an indexed holder address, like Checkout and Environment; its convoy is the vessel's. Its realised pool session id is not indexed.
- **A lifecycle resource is never revived** (owner, 2026-10-22, #3629 Q1-Q5). A lifecycle resource is never revived. Once Gone it stays Gone, and its holder or owner requests a new resource; this covers sessions per launch, replacement vessels and replacement checkouts. Membership in a convoy is the member's own indexed reference, with no edge on the Convoy. A vessel that breaks after its lease ends Gone, and the requester limits replacements in its own state.
- **Inside a vessel is the crew's business.** Extra branches and worktrees inside a vessel, such as a stack lead's subagents, live inside the crew's Checkout. Flotilla knows the deliverables (promised branches and change requests, observed on the forge) and the Checkout, whose release removes everything inside it.
- **A lend** lets another role, or a person, borrow a resource the owner holds: a Checkout, a process with an open port, or a jackstay stream. It is a relation asserted by the lender, with terms: who, mode (read-only or shared-write), and until a time or an event. The environment enforces the mode where it can. In a workflow with a co-placed coder and reviewer, their sharing of local files is a lend, not a shared placement owner.
- **A lend gates release.** The lender's `release` is refused while an unexpired lend exists, unless the borrower has returned it or an authorised override records a reason. A lend until an event that never comes stays unexpired until it is returned or overridden. Return is the borrower's edge. Loss is `vanish`, with a message to the borrower.

ADR 0064 records that a machine's log may stamp persistence-only metadata (such as Message's `admitted_seq`) inside the create transaction through `Log::create_resource`, that `Home::evict` drops rebuildable values without touching the log, and that observations are read through a borrowed consistent snapshot.

### Promises

- **Promise kinds are declared, not built in.** The engine knows a Promise, its submissions and the `deliver` gate over them. Each kind is a declaration that says:
  - its submission shape: what a submission names, such as a change request, a ref or an issue;
  - what it observes and where, such as the forge;
  - what keeps it: a judgement over those observations, with a human verdict on top where the declaration requires one.
- **Example: the `decision` kind** has two forms.
  1. Resolve an existing issue. The promise is kept when the issue is observed closed with a resolution.
  2. Produce a new issue of a declared shape, such as a `wayfinder:map` issue with at least one child. The promise is kept when the submitted issue is observed with that shape and was created during the convoy.

  Either form may also require a human verdict.

ADR 0064 declared kinds: promise kinds are federated data definitions composing a code-owned condition vocabulary into durable, ready and kept rules. Charters may add kinds or tighten stock kinds by extension, and admission freezes the definitions used.

### Roles, inboxes and delivery

ADR 0064 §"Roles, inboxes and delivery" records that resident pending order is a budgeted cache over the store's `pending_by_inbox` index (which also serves restart), and that an unbound inbox's timeout clock starts when it gains its first open message and is released when it has none.

ADR 0064 §"Roles, inboxes and delivery" records the five code-declared fleet roles as overridable defaults, each with an inbox and bindable to any handler, and the minimal AgentSession as the receipt holder.

ADR 0064 §"Roles, inboxes and delivery" records that addresses never carry a host (ADR 0066), that a role's handler is a binding and routing rule (rebinding touches no stored message), that replies to a role are routed by subject, and that `enqueue` is the one place a message's home is worked out.

- **Inboxes.** Each role instance owns an **Inbox** resource, homed with the role's owner: the convoy for convoy roles, and the project's declarations for standing roles. Messages queue there and outlive any fulfiller.
- **Standing roles** are declared in the charter and own their inboxes, whether or not anything automatic fills them. An Ensure is optional, layered on a role: it keeps an agent bound. Retiring an Ensure leaves the role and its inbox in place. There is no separate person kind: a person's own inbox is a declared role with one holder.
- **Addresses** are typed lists of segments over a closed kind enum: project, convoy and role to start. They are stored typed and shown as `project:flotilla/role:gui-approver` and `project:flotilla/convoy:stack-3152/role:coder`. There are two shapes to start, project/role and project/convoy/role, with no vessel segment. New kinds extend the enum without changing existing addresses.
- **Session addresses (planned, not built).** A `session` kind, a leaf under a role, is the planned extension; which session ID it uses is still open.
- **Every role is declared** with a one-line purpose. A message to an undeclared role is refused at send time, with the reason.
- **Dynamic roles.** The convoy manages all its roles and inboxes at its home: declaring a role, its inbox and its brief. The workflow template says which roles may declare more (a stack lead, for example), and the supervisor may too. Role names may be dynamic (`coder-pr45`), with the role type as an attribute.
- **The brief** is the inbox's first message. A new conversation reads the inbox from the top; a resumed one continues from its own position.
- **Consumers.** A consumer of an inbox may be an **AgentSession**, a person, or a deterministic controller. The consumer relation is the Role Binding, and convoy membership is "X consumes `project:P/convoy:C/role:R`", asserted by the convoy.
- **Consumer identity** (#3424 ruling 1). A message's receiver is a typed consumer identity: a consumer kind (`agent_session` or `controller` for now), the id of the consumer's resource and the consumer's host, that resource's home. It is compared only for equality. It is not the Role Binding: a binding changes, while a receipt keeps naming who took the message. The host stays because expiry waits on the consumer host's stream. The person consumer kind comes with the principals and teams model; adding a kind needs no migration.
- **Several consumers claim** (#3424 ruling 3). Any mix of people and agents may consume one inbox, so an inbox's binding is a set of consumers. A message is pending for a consumer while it has no receipt from that consumer. The claim is the first `deliver` the home accepts, and it records that one receiver. The first to act on a message claims it and settles it.
- **Attention is a message.** There is no separate human attention concept. An observer that sees something needing a hand (a permission prompt, an expiring credential) sends a message that expects the outcome to the role that must act. If it resolves unseen, the message settles from Pending. Escalation is a chain of roles. Salience becomes a judgement over pending messages. Regard stays a separate presentation concept: what someone looks at, not what waits for them. Inbox activity and workflow interaction diagrams are drawn from message records.
- **Read access follows the project.** Anyone holding a role in a project reads all its inboxes and its convoys' history. A convoy's crew reads its own convoy's inboxes. Anyone holding a role in the fleet project reads everything. Message bodies never carry secrets. This is a starting policy, not a settled one: a policy hook is expected later, and no policy language is fixed now (#3230).
- **Pull delivery.**
  1. The consumer's host replicates the inboxes it consumes.
  2. It delivers each turn through its AgentChannel, using the adapter's transport.
  3. It applies a **receipt**: a `receive(message, transport)` edge on the consumer's own resource, from a live state to the same state, whose master is the message.
  4. The inbox's home records `deliver`, gated on observing that receipt among the entries about the message.

  Every consumer kind records receipts the same way (#3424 ruling 2), and the receipt reader accepts a `Receive` edge from any kind that declares one. An agent's resource is its AgentSession, and a controller's is its own resource.

  A person's receipt is their **acknowledgement or response**, made through any surface, by the principal or by an agent acting for them (owner's schema look-over, 2026-10-18). A surface is how a person reaches a message, not where it is delivered: the same message may be loaded on a phone, in Wheelhouse and on the web, and **a surface loading a message is not a receipt**. On a shared inbox (two people qualified to approve, say) the first acknowledgement claims the message. A person's verdict is `settle(answered)`.
- **Turn first, then receipt.** The consumer's host hands over the turn before it writes the receipt. A crash in between can repeat a turn to the same session, so adapters ignore a message ID they have already turned.
- **`drop(undeliverable)`** is gated on what the home can observe: nobody bound past a threshold, or a delivery-failure record published by the consumer, and no receipt observed. It means "no receipt was confirmed within the policy", not "no agent saw it", because a consumer acting on a lagging replica can still turn the message.
- **At least once across a replacement** (#3214). The home cannot tell a consumer that received a message and died from one that never did, so two sessions may both turn a message across a replacement. The window is kept to that:
  - `bind` waits until every observed receipt has its `deliver` edge;
  - `deliver` accepts a receipt from a session that is no longer bound;
  - `deliver` records exactly one receiver, and later receipts are refused.
- **Unbind reactions.** The inbox's owner unbinds a consumer when it observes the AgentSession's end row, and when the consumer's host has been silent past the role's policy threshold. The silence trigger is a reaction on a judgement, never a gate, and the unbind records its reason. Without these reactions, a dead bound consumer blocks the inbox for ever.
- **The plug is the whole inbox.** The async `InboxBackend` in `flotilla-inbox` covers `enqueue`, `bind`, `unbind`, `pending(inbox, consumer, rules)`, `receive`, the Message machine’s `deliver`, `settle` and `drop` edges, and resumable cursor `watch`. Rules match sender, relation or expectation and are passed by the consumer, with the chosen Turn, Notify or Mute transport recorded on Receive and Deliver; their storage is deferred to #3317. The backend reacts to `drop(undeliverable)` by enqueueing a notify-mode system message to the sender’s supervisor, with the dropped message as subject. An injected supervision port resolves the supervisor; a sender supervising itself produces no dead letter. Every implementation passes one shared contract-test suite. Engine and inbox tests share the dev-only `flotilla-quint-testkit`; the inbox backends replay `message_race`, while full-graph conformance stays with the engine. Resource-backed replication is the first implementation. Queue brokers such as RabbitMQ or Cloudflare Queues fit behind the same contract.
- **Filtering** chooses how a message is delivered, never whether: turn, notify or mute. Mute counts as delivered.
- **Message content** (#3424 rulings 4, 7, 8, 9 and 10). A message's spec holds, beside its inbox, expectation and deadline:
  - its **sender**, an address naming a declared role;
  - its **relation** to the consumer, a short code (supervisor, peer, dependency, dependee, system), which delivery filters and the turn's frame read and which the sender's role does not determine;
  - its **body**, a frozen document typed as an enum: prose text, or a typed request body (admission's convoy request is the first). It is stored in the database by content hash, never in an external object store, and the create entry does not copy the whole spec, so a body is stored at most twice: in `objects` and in the create's effect;
  - its **references**, a child table of (address, revision, is-subject) with at most one subject (ADR 0066). Nothing indexes messages by reference until "messages about X" is a real query;
  - **`in_reply_to`** and **`supersedes`**, typed references to another Message (kind, name and home) that admission reads, so replies and supersessions cross homes;
  - an optional **delivery condition**, one condition column. When it is observed no longer to hold, the turn-rules role drops the message as `withdrawn`.

  An `interrupting` flag and a body digest are not carried over.
- **The body is kept until expunge** (#3424 ruling 5). A message record is immutable, so its body is never compacted. Retention is the archive's job; trimming, if disk ever forces it, is per-kind deletion in the archive, never an edit of a record.
- **Senders are declared roles** (#3424 ruling 6). A person sends as their declared role, and each `system:` producer (turn rules, nudges, the stall judge, capabilities, the fleet store, holds) becomes a declared fleet role. There is no `system` address kind.
- **Dead letters** go to the sender's supervisor, unless the sender is that supervisor.

### Admission and capacity

- **A convoy request is a message** that expects an outcome, sent to the role the fleet's workflow declares for admission (for example `project:fleet/role:admission`). Its consumer is a deterministic controller. Footprints and crew details are its typed body.
  - Receipt is `deliver`.
  - Position is a judgement over pending messages: class of service and holds.
  - Admitted is `settle(outcome observed)`, when the Convoy exists: from Awaiting after the receipt, or from Pending if the Convoy appears first.
  - Refused is `settle(answered)`, with the reason as the reply.
  - A revised batch supersedes the request; a request cancelled without a replacement is withdrawn.
- **Other requests** can use the same inbox shape. There is one queue mechanism, not two.
- **Admission grants a capacity budget.** The workflow template proposes a budget, and admission may grant less.
- **Growth within the budget goes straight to placement.** The convoy asks placement for vessels directly. Placement is decided by the capacity pool's authoritative daemon, because the ledger is its own (2026-10-20 grill). For an agentless SSH host this is its owning daemon; for a provider account it is the daemon that holds the account's credential. Filling a role from inside a vessel, such as a subagent, uses no budget.
- **Growth beyond the budget is a new request** to admission, in effect a readmission.
- **A vessel's lease is its pinned host for now** (owner, 2026-10-23, #3629 Q6)**:** until a capacity model is ruled, a vessel's lease is the host pinned at admission, recorded as granted; the engine judges no capacity.
- **Warm vessels** are a host-owned placement policy, at low priority: a host keeps N warm vessels per image. Placement may hand one to a convoy as an ownership change. Its lifecycle stays Realised, and it counts against the host while idle and against the convoy's budget from handover. Pools may later prepare more, such as a build directory seeded from a recent cache. *(This bullet predates the 2026-10-20 capacity pools and is not yet pool-aware; which pool a warm vessel counts against is for the capacity grill under #3303.)*

### Storage, replication, views

**Storage.**
- **Tables from Rust types.** Data is stored as relational tables generated from the Rust types by our own derive on rusqlite (#3208). Collections are child tables, records are owner-plus-position rows, edge events are rows, frozen documents are stored by hash, and strings are interned. JSON is not the stored form.
- **One per-home event log** (#3392 G8, owner's schema review of 2026-10-17). Every write is one event, appended to the store's single `log` table, keyed by (home, home sequence), in the same transaction as the kind tables it changes. The kind tables are a view of the log: reads never replay it, and replicas apply events without re-running machines. An entry's header is columns: the resource, its previous sequence (for gaps), the cause (`create`, `edit`, `edge`, `delete` or `expunge`), for an edge its edge code, from-label, to-label and master (`about`, G11), and for every write class its writer and time. Its `payload` is one inline CBOR document explaining it: an edge's arguments and gate reads (so argument shapes change without a schema change, under ADR 0047's rule for documents), or the reads a create's admission relied on, never the spec it creates, which the effect already holds (#3424 ruling 4). Its `effect` is the authoritative change, canonical CBOR, as named field-level changes in the kind's own terms, never positional row operations: `Set` (one column-shaped field), `State` (a state as one unit: its label, `since` and every column datum), `Put` and `Remove` (a collection row by its key) and `Append` (a `records` or `history` row at its position), each addressed by a row path of field names and keys, with every object the event sets. A local write builds its effect, encodes it, decodes it and lowers the decoded effect to rows through the same code as `apply`, so a home and its replicas write the same rows. A whole-value write is used only to create a resource and for spec edits, which never touch state, `records` or `history`. The log is indexed by resource (a resource's history, in sequence order) and, partially, by master (`about`, time order), so the entries about a resource are a range read, and by home over its expunge entries alone, the tombstones `apply` checks an event for an unknown identity against. Edge codes are checked against the kind in the writer and in `apply`, since one table holds no per-kind CHECK. Kinds have no edge-log tables. The readable `log_view` (#3392 D4) names each entry's edge and labels through `codes` by the entry's kind: a kind declaring edges has `codes` rows `log:edge` for its edges and `log:label` for its state's labels. A machine edge that names a master (`EdgeDef::about`, such as a session's `Receive` of a message) is stored with the master as its entry's `about`, and the master's gates read the entries about it: Message's Deliver and Drop(Undeliverable) gates read the receipts about the message from them (#3392 D1).
- **Records.** Attempts and receipts are edges (R4), so they are log entries like every edge, and no kind has a `history` table of its own. `records` are rows a resource owns that change only through a declared edge, such as a deliverable whose status moves on; a records row is never removed. They use the append-only record type in `flotilla-schema` (`Records`, 32-bit positions, each row stamped with the owner's version at the write that appended it). An append past the last 32-bit position is refused with an error (`RecordsFull`), never a panic (#3373 ruling 6).
- **History is never trimmed** (#3392 G9). A delete is an event: the resource's rows go, and its entries and identity stay. Only an expunge removes them: it deletes the resource's entries, then its identity, in one transaction, and logs one `expunge` entry naming the identity by kind, name and home, the one entry about the resource that stays. An entry about an expunged master keeps its row, its `about` set to null.
- **The log holds desired and decided state only** (owner, 2026-10-17, #3392 lane 3 ruling 2). Its entries are creates, spec edits, edges, deletes and expunges. Observations live in the ephemeral observed store, latest value only, and never reach the log: provider refreshes, checkout facts, keepalives and stream watermarks. An observation enters history only through a home's decision on it, as a gate read recorded in an edge's payload or a record the edge appends. The 10 GB disk budget is checked against the measured per-host durable write rate in `WORKLOAD.md`.
- **Reference indexes** (owner, 2026-10-17, #3392 lane 3 ruling 1). The derive generates an index on every reference column of a child table, `<table>_<column>_ref`, partial on the column being present when it is nullable, unless a declared index already leads with it. A child table is keyed by its owner first, so without it the foreign-key check of removing an identity would scan the table, and expunges come about as often as creations. A root reference column is indexed only by a declared index; the query-plan tests check that an expunge's foreign-key checks scan no table.
- **Machine states: a label plus shared data** (owner, 2026-10-16, #3373 ruling 1). A machine's state is stored under `#[store(state)]` as one label column `<field>` and `<field>_since`, then the state's data: one column group or table per datum name, shared by every label that carries it, and one CHECK per label requiring that label's data and nulling the data it lacks. The state enum takes the type attribute `labels = L`, naming its code enum of labels; its variants keep their codes, which must be `L`'s. Each datum takes one storage form: plain (its columns), `child` (a map or set: a table keyed by owner plus map key or set value), `intern`, or `flatten` (a struct without table fields as columns, or an `Option` of one, present when its first required column is non-null, with no presence flag (#3392 rulings G5 and A5), null under a label that lacks the datum). A datum's table holds rows only while the label carries it, and decoding refuses rows otherwise. The closed attribute list gains `labels` (types).
- **Row-level collection edits** (#3373 ruling 2, #3392 A3). An edge built from the previous and the next state value (`Edge::transition`) writes the state as one unit, its label, `since` and every column datum (nulled when the next label lacks it), and each collection datum row by row, putting new and changed keys and removing removed ones; it never rewrites or re-emits a whole collection. The label and its data are set by one statement, so the per-label CHECK holds after it, and the log entry names the state as one `State` change and each collection row on its own. State data changes only through edges, never a spec edit.
- **Label-only `since`** (#3373 ruling 3). `<state>_since` is the time the label was entered: an edge sets it to its event's time only when the label changes, and a data-only edge leaves it. The time of the last change of any kind is the latest edge in the resource's log.
- **Store-side `from` and version checks** (#3373 ruling 5). Inside the writer's transaction, before writing anything, `apply_edge` refuses a stale version (`Conflict`), then an event whose `from` is not the stored label (`WrongFrom`), then anything the edge addresses that does not exist. A refused edge writes nothing and leaves the transaction usable. The engine's `Home` decides and is `apply_edge`'s only caller.
- **The resource is the stored kind** (owner, 2026-10-17, #3392 ruling D5). Every machine's stored kind is its `Resource<M>` (name, spec, state and version); there is no separate row type. The derive targets the kind's spec, which carries the kind's attributes (`kind`, `code`, and `state = S` naming its state), its indexes (whose `state in [...]` names the holder's state) and its interning, and the state. The holder lays out the spec's columns, then the state as the field `state`, and declares its machine's edge codes; its edge events, attempts and receipts among them, are its log entries (#3392 G8). The store supplies the name (the identity row) and the version. A lifecycle kind's request is its spec.
- **History is no part of the decision path** (#3373 ruling R1). Loading a resource's current state never loads its history, and a home holds no edge log: a `Resource<M>` is its name, spec, state and version. Its history is its entries in the home's log, read in pages as a range over (resource, home sequence), by `log_by_resource` (the store's `changes`), and the engine decodes an edge's entry back into the edge event its home returned (`edge_event`).
- **The home sequence is the only version** (#3373 ruling R2). A home decides each write, and its `Log` persists it and gives its sequence, which the resource adopts as its version: over the store the log is `insert` and `apply_edge`, and the sequence is the store's `home_seq`. The home mints no version and stamps no event. A write the log refuses, such as a version conflict, leaves the home's resource as it was. The in-memory `MemoryLog` keeps the same contract for tests, replay and conformance: one sequence per home, and each resource's edges paged by (resource, sequence).
- **Short codes, readable views** (#3392 rulings G7 and D4). Every stored code is short text written by hand. The schema output also holds a shared `codes` table, one row per (kind, field, code, name), filled when the schema is created, and one view per kind table, `<table>_view`, which shows each code as its variant's name (through `codes`) and each interned column as its text (through `strings`).
- **Serde views.** YAML and JSON stay valid documents for disk, manifests and CLI output. Their content follows the model.
- **Shared tables.** Each kind's tables (its root table and the tables under it) hold the rows of every home. A resource's home is part of its identity row in `resource_identity`, not a column of the kind's tables. Only the replication applier writes rows homed elsewhere.
- **References** (#3392 G1 and G3, owner grill 2026-10-16). A field that refers to a definitions-class kind, or to anything outside the new store, holds an Address (ADR 0066): interned canonical text, which decoding refuses in any other spelling. A request that names one that must exist reads it at admission through the recording admission handle, so its existence is a recorded gate read, never a foreign key; a convoy's project is the first. A field that refers to a single-home record inside the new store holds a `Ref`, a foreign key to the target's identity row, which is (kind, name, home): records are single-home and refer across homes, as a vessel, homed where its capacity is, refers to its convoy, homed where it was admitted.
- **Identity rows are tombstones** (#3392 G2, as revised by G9). Deleting a resource keeps its identity row and name string, so a `Ref` never dangles: a reference to a deleted or archived target resolves to its identity row, and a reader that finds no current row routes to the archive. Only expunging the resource removes its identity row and name string.
- **Kind codes** (#3373 ruling 6). The production kind registry (`flotilla_state_machines::kinds`) lists every production kind under the code it holds for good. A code is allocated there once and never reused. Test kinds register beside the production kinds, so the registry refuses a test kind that takes a production code.
- **Vessel is a stored kind; the host ledger is a view** (#3373 ruling 6, #3392 ruling D5). The stored Vessel is `Resource<Vessel>`, declared by its spec, the lease: the placement request and the host, with the lifecycle state; its edges, attempts among them, are its log entries. Placement is not a table of its own: the host ledger is a view over the leases of live vessels, indexed by host, and a ledger is rebuilt from them after a restart rather than kept, so a restart loses no lease. Each other lifecycle kind (Environment, Checkout, Clone, ImageBuild, TerminalSession) declares its spec beside its request, in the crate that owns that request, when the kind moves to this store; the engine supplies the state, edge-event and attempt types it stores.
- **A provider's recipe is its own document** (owner's schema look-over, 2026-10-22, #3719). A stored lifecycle kind's request holds the provider's own recipe as a provider-owned document, not as variant columns in the shared table, so a new provider never changes the shared schema. Columns are for what is queried.
- **Nothing derived at launch is stored** (owner's schema look-over, 2026-10-22, #3719 rulings 5-6). Stored resources never hold launch-time derived data such as credential grants or skills; those are composed at launch. A Docker environment's image is either pinned or built, never both.
- **Edge payloads name edges by code** (#3373 ruling 6). An edge's document holds `[code, arguments]`, never the edge's variant name, so renaming an edge changes no stored payload. The header's edge code and the document's agree.
- **Crates and attributes.** The derive is `flotilla-schema-derive`, re-exported by the backend-neutral `flotilla-schema` (descriptors, DDL, the kind-code registry); the rusqlite runtime is `flotilla-schema-sqlite`. As ruled on #3215, the attribute list is closed: field attributes `child`, `records`, `history`, `frozen`, `cbor` (an inline canonical-CBOR document, added by the owner's ruling of 2026-10-15), `intern`, `flatten` and `state`, the variant attribute `code`, and the type attributes `kind`, `code`, `state` (a spec's state, #3392 ruling D5), `index` and `labels` (added by the owner's ruling of 2026-10-16 on #3373). The schema review of 2026-10-17 (#3392 A7) adds the field attribute `alias = "old"`, which a renamed field takes beside its shape attribute so its columns and table answer to the old name and earlier log entries still apply, and the kind attribute `edges = E`, naming the code enum of the kind's edges, the only edge codes its entries may carry (a machine's `Resource<M>` declares its machine's edges itself, so a spec takes none). Any other shape fails to compile; adding to the list is a reviewed decision.

**Archive.**
- **Ended resources archive as a bundle.** Seven days after a resource reaches an end state, its home ships the resource with its records (deliverables and the like) and its events, its entries in the log (attempts and receipts among them), as one bundle to the archive host. The home keeps them until the archive host has the bundle; only then does an expunge remove them locally (#3392 G9), the one way records and events are removed.
- **Standing inboxes never end.** Their messages archive one by one, seven days after they settle or drop.
- **The archive is read-only and typed,** with the same derived relational schema. Retention is indefinite by default. Per-kind deletion comes later, terminal recordings first.
- **Identity** (#3392 G2, as revised by G9). Every host keeps the identity row of each resource it has seen, live, deleted or archived, as a tombstone until the resource is expunged. A host that finds an identity with no current row routes the read to the archive. Expunge alone removes an identity row and its name string.

**Replication.**
- **One sequence per home.** Its value is a resource's `version`. Each write appends one event to the home's log in home order (#3392 G8), and the stream carries events. Replicas apply each event's effect through the same lowering as the home's own write, append the event to their copy of the home's log, and do not re-run machines. A replica catches up on a resource from its snapshot plus the resource's events after it.
- **Encoding and liveness.** The stream is CBOR (ADR 0063). Its keepalive is the link-liveness signal.
- **Whole-home replication** ([#3463 owner ruling, 2026-10-18](https://github.com/flotilla-org/flotilla/issues/3463#issuecomment-6252080381), superseding #3213 ruling 2). Every host follows every other home's whole log and therefore holds every resource and its identity. Each home's log is the partition. Per-item scope subscriptions are withdrawn.
- **Coarse topic.** Each event reserves one open coarse topic code, chosen by the home's write transaction, for later partitioning by whole topics on whole homes. Nothing filters on it yet.
- **Misses** that the local copy cannot serve are routed to the home, marked remote with their age. Gates read only local inputs and never query remotely.
- **Observation consumers.** Fine-grained filtered views belong to read-only observation subscriptions, such as `pm connect`, rather than replication. They are out of scope, as are partial replication by topic and archives beyond the history window.

**#3463 implementation** (schema [approved 2026-10-18](https://github.com/flotilla-org/flotilla/pull/3465#issuecomment-6252741558)).
The [stream design/workload note](../implementation-choices/change-stream.md) records the
reserved log topic, per-home applied watermark (`home_seq`, applied time), and resume floor.
Only expunge raises that floor, atomically with the log deletion it covers. Store startup
installs these tables and synchronizes their readable views. Snapshot/tail/resume and
keepalives use framed CBOR through a listener-independent service on the resource HTTP API;
follower progress and rows commit together. Missing progress reads as zero/epoch. Gates use
local applied time, never remote reads or link health. Surface misses route once to the home
and are marked remote with age. The old replicator remains for old kinds. Kind migrations
install their new-store registry; #3317 is first. Fine-grained observation consumers, topic
filtering, archives and retiring old replication are deferred. Quint specs are unchanged.
**First shape and expiry:** replicate-everything is sized for a home lab of a few hosts,
including laptops that disconnect, not WORKLOAD's target or ceiling. The workload note sizes
bandwidth, offline catch-up, snapshot and temporary disk for that shape; measurements remain
open in #3425. It expires when hubs such as a desktop and cloud host should hold everything
while spokes such as laptops hold enough to work offline. At larger scales, partition writes
with replicated read layers. Those later shapes are out of scope. No lazy store open or
followers run before the first registered kind is installed; until then both routes return 503.

**Liveness.** Liveness is judged from the observer host's link. Observations are written only when they change. Heartbeat-only writes go away.

**Views.**
- **Resident state is a filtered replica.** State is loaded into typed resident structures and kept current from writes and the stream. Resident state is not a ban on database reads (owner, 2026-10-14):
  - every query has a working, correct in-memory implementation;
  - almost all live queries are served from it, and each access method's default is examined deliberately;
  - queries that need the database go to it, and are sane too, with pushdown and indexes.
- **A memory budget bounds it.** Every resident structure has a size model, and eviction keeps the resident set within the budget. The time period the resident set covers is then a reported fact, not a fixed seven days. Target environments include memory-constrained hosts, such as a Durable Object, with a much larger database. Later, the size model can drive sharding decisions.
- **Incremental judgements.** Judgements are maintained incrementally through a compressed-bitmap reverse index from inputs to rows and one timer queue, with Bloom pre-filters on arriving changes and per view client.
- **Surface protocol.** The named-query surface protocol stays.

**Definitions.** A definition is homed at the ManifestRoot that declares it, and edited through git. Undeclared definitions are homed where they were created. Multi-writer merge metadata and field ownership are removed.

ADR carry: a standing ConvoyEnsure is acted on by exactly one host — its declared driver when it names one, otherwise its definition home (the declaring ManifestRoot's host, or the creating host for an undeclared ensure).

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

ADR 0064 §"Switch-over" records the `[primitives]` switch and its dummy-fleet guard, the phase-to-state import table with named refusals, and old-store reads behind named ports during the transition.

ADR 0064 §"Switch-over" records that Message moves over only after the change stream (#3463), so cross-host reads come from local replicas and misses are marked remote.

ADR 0064 §"Switch-over": with `[primitives]` on, the engine is the single writer for each kind it holds. Any old-shape copy is a one-way, derived scaffold, deleted by the read-port layer.

There is no double-write path and no long-lived branch.
1. **Build alongside.** The new code is built on main in new crates, behind one switch in the composition root.
2. **Prove it.** The new path is proven with the dummy fleet (#3206) at 100, 1,000 and 10,000 convoys, and with a shadow cluster.
3. **Rehearse the import.** Pre-roll validation (#2167) rehearses the one-shot importer against every host's live store.
4. **Cut over in one roll.** The old database file is left untouched, and rollback means returning to the previous generation.

## Consequences

- **Smaller machines.** Convoy goes from 49 patch variants to 2 edges plus records. Message goes from 11 phases to 4 states. Six kinds share one lifecycle. Every stored retry counter, condition struct, mirror and heartbeat goes.
- **One queue mechanism.** Admission is an inbox, so convoy requests, demands and crew messages share one machine, one backend and one listing.
- **Principal retires into roles.** A person acts by consuming a declared role. Attention, escalation and person-specific inboxes are messages and roles, and Regard stays as the presentation concept.
- **ADR 0055 and #1394 are amended.** ADR 0055's address forms become typed segment lists, and its `principal:` form becomes a declared role. The #1394 ruling ([ADR 0058, dispatch](0058-dispatch-four-queues-one-global-decider.md)) moves from whole-convoy admission to admission with a capacity budget: growth within the budget goes to placement, and growth beyond it is a new request.
- **Crews on any host.** Crew members can be on any host and can be replaced without stranding messages. Handoffs, resumes and nudges are messages, and no convoy transition is involved.
- **The importer is the largest hand-written piece.** After the cut-over, ADR 0047 is enforced by a checked-in schema snapshot and explicit migration steps, and the golden corpus becomes a SQLite file.
- **Peer merge goes away.** The Plane-A peer-merge layer (`crates/flotilla-daemon/src/peer/`) stays bugfix-only and is deleted by this work, because whole-home log replication supersedes it.
- **CLAUDE.md changes at the cut-over roll.** Its "Development Phase" rules (serde aliases and defaults, the JSON golden corpus) change to the schema-snapshot form in the same roll.
- **Interactions are model-checked in Quint.** The exploratory specs (#3214) found the races ruled above. Minimal per-machine specs, replayed as traces against the engine (#3220), gate the implementation, which is split into PR stacks (#3215).

ADR 0064 AgentSession records current activity (Working, Idle, Interrupted, with since and cause) through a self edge in Live that changes only on a real change. Its history is the home log. Call-level agent detail is session-log data, never replicated store data.
