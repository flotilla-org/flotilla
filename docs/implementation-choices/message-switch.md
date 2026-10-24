# Message on the engine behind `[primitives]` (#3317)

Owner rulings: [2026-10-18](https://github.com/flotilla-org/flotilla/issues/3317#issuecomment-6250618253),
its [amendment](https://github.com/flotilla-org/flotilla/issues/3317#issuecomment-6251238869), and the
[schema look-over](https://github.com/flotilla-org/flotilla/issues/3317#issuecomment-6276308214) of 2026-10-19.

The `[primitives]` section of `daemon.toml` moves Message onto the engine and its store. The daemon
refuses it without `[dummy_fleet]`. With it off, nothing changes: every path below reads and writes
the old store exactly as before.

## Storage

The switch opens one decided store (`decided.sqlite` in the state directory), homed at the daemon's
node id, with exactly two kinds (`flotilla_engine_store::registry`): `Message` (`ms`) and
`AgentSession` (`as`). Its DDL and readable view are checked in beside the crate's tests
(`tests/snapshots/message.sql`, `schema_view.md`). They are identical to the reviewed fixture tables:
no new table, column or index.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `agent_session` | root | 6 | – | – | – |
| `message` | root | 23 | 5: `expectation` 2, `reason` 3 | `message_references` (child) | `pending_by_inbox` (inbox, admitted_seq), `deadline` (deadline), `by_reply` (in_reply_to), `by_supersedes` (supersedes) |
| `message_references` | child | 5 | – | – | – |

- **`flotilla-engine-store`** is the engine's `Log` over `flotilla-schema-sqlite`:
  - creation is one `insert` and an edge one `apply_edge`, each in its own transaction;
  - `MessageLog` assigns admission inside the create's transaction;
  - receipts are served as a `ReceiptLog` from `Read::about`.
- **The store is installed before peer tasks start**, so every home's log is followed from the first
  connection (#3463).
- **Conditions are ADR 0066 conditions.** `Expectation::Outcome` and `delivery_condition` held an
  `Address`, which cannot express a producer's Leaf. They hold `flotilla_address::Condition` instead,
  interned as its canonical print in the same address-domain column. The DDL snapshots are unchanged.

## Addresses

- **A message is `<inbox address>/message:<id>`**, an ADR 0066 kind under its inbox (carry applied
  to ADR 0066 §4).
- **A keyed send's id is `message_record_name(receiver, sender, producer key)`**, the name the old
  store gives it. So a repeated key names the same message, and so do an import and a post-switch
  send of the same producer key.
- **An imported message keeps its old record name.**
- **An unkeyed send** takes the digest of its sender, its home and its admission sequence.
- **A dead letter** takes the digest of the message it reports.
- **The fleet roles** (`turn-rules`, `nudge`, `stall-judge`, `capabilities`, `fleet-store`) are
  addressed under the fleet identity declared in `FleetDesignation.spec.fleet`:
  `fleet:<fleet name>/role:<name>` (owner carry in ADR 0066, #3822).
  - They are declared in code, each with a one-line purpose, and can be overridden
    (`FleetRoles::declare`).
  - A held turn sends as turn-rules; `system:hold` survives only in its de-duplication key.
  - The designation is read from the record each time. Changing its charter Project does not
    change these addresses. Changing `spec.fleet` changes the fleet identity, changes the inbox
    addresses and strands messages pending under the previous name.
  - Missing or empty fleet names refuse resolution with a named reason; legacy designations
    still decode. Nonempty names preserve their spelling, as other address natural keys do.

## The send path

Producers keep building their intents in the old shape; every one already goes through the
receiver-home router. `enqueue` happens at the receiver's home:
- The router's admission (`apply_intent_document`), or a producer's local admission, maps the intent
  onto the engine spec (`primitives::convert`).
- It then enqueues it with `InboxBackend::enqueue_as(spec, id)`.

The home is worked out in one place, the router's `message_creation_origin`, as `resolve_message_receiver`
does today. A reply to a fleet role is routed by its subject: the convoy it references, else the
convoy whose crew the original was sent to, else the original's home.

## Reads

Convoy and crew code still reads old-shaped Messages: stall judgement, turn evidence, dedupe,
capability cards, charter notices and convoy explain. It reads them through one port, the Message
records port (`in_process::message_port`).
- **Switch off:** the port is exactly the old-store read it replaced.
- **Switch on:** it reads the engine replica, every home's, in the old shape (`primitives::view`).

A replica miss on a message an admission names is read at its home through `HomeReads`, marked
remote with its age. That version is the admission read.

## Transition ports

Every old-store read the engine path makes goes through a named port (`primitives::ports`):
- **`RoleBindings`:** today's role routing, through `MessageReceiverSnapshot`.
- **`ConditionReads`:** condition values over old-store records.
- **`OldStoreRecords`:** a role's vessel, a receipt's session, and the records a reference names.

When Convoy and the session kinds move, each swaps one implementation.

## Delivery

The delivery adapter (`flotilla-daemon` `runtime::primitives`) replaces the old Message controller.
The old audit sweep does not run either, as there are no old Messages to compact. Each pass:
1. Reads who holds each role through `RoleBindings`.
2. Keeps one minimal AgentSession per live crew session (named by its TerminalSession record,
   created Live and detached, ended when the terminal ends), bound to the inboxes it holds.
3. Types each session's pending turns through the terminal transport, framed as the old path frames
   them.
4. Once the turn is confirmed, writes `Receive { message, transport }` on the session's record and
   records the message's `deliver`.
5. Settles and drops the home's open messages on the evidence the gates read:
   - a reply in the replica;
   - an outcome, or a lapsed delivery condition, through `ConditionReads`;
   - a passed deadline, with the bound consumer's host's stream watermark as `AppliedThrough`;
   - an inbox left unbound past `UNBOUND_AFTER` (24 hours).

**Mode rules are not stored.** The adapter passes none, so every message is a turn; the backend sends
dead letters as Notify. No supervisor is declared on this path yet, so no dead letter is sent, as
the old store dead-letters only on an operator's batch failure.

## The import

`primitives::import` maps an old record onto the new spec and state per the ruled table:

| Old phase | New state |
|---|---|
| Accepted, WaitingOnReferences, Deliverable | Pending |
| Delivered | Awaiting |
| Satisfied, Answered, OutcomeMet | Settled |
| Expired | Dropped(Expired) |
| Superseded | Dropped(Superseded) |
| DeadLettered | Dropped(Undeliverable) |

- **How a record is mapped:**
  - Each home's admission order is the old acceptance order.
  - A receiver's host comes from the session its delivery names.
  - `Superseded { by }` comes from the successor that supersedes it, or from its canonical predecessor.
  - Expired records no watermark.
  - A compacted body imports as `[compacted body sha256:<digest>]`.
- **The check against the machine:** the import replays, from a fresh admission, the edges that reach
  the restored state, with exactly the evidence the state records.
- **Refused with the record named:**
  - a `principal:` sender or receiver (the principals model is #3457);
  - a topic receiver;
  - a missing successor, session or referenced message;
  - an unmappable condition or reference.
- **The rehearsal:** `flotilla resource validate --from-daemon` restores every import into a temporary
  store, reads each back and checks it. It prints counts by new state and each refusal; it does not
  fail the roll while the switch is off in the live fleet.

## Known limits

- **Operator rulings on the engine path:** an operator sends as `principal:<name>`, which has no
  declared role until #3457, so on the engine path the ruling is refused with that reason. A held
  turn with no supervision target falls back to `principal:operator` and is refused the same way.
- **No submission batches:** an operator's batch failure and a withdrawal of a message homed elsewhere
  are refused with a reason.
- **Old-shaped reads list the whole replica:** the Message records port reads all messages in the
  replica for each list, which suits the dummy fleet's scale; an index read per receiver comes with
  the next kind to move.
- **No out-of-repo authors:** only the daemon behind `[primitives]` writes the new tables, and the
  switch defaults to off. No resource kind's serialized shape changes.
