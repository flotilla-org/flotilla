# 59. Agent sessions, processes and channels

**Status:** Accepted (owner grill, 2026-10-08; map #2912, recorded by #2913)
**Date:** 2026-10-08
**Amends:** ADR 0055 (delivery wording and "holder"), ADR 0028 (the delivery ladder), ADR 0010 (tool crew and the Crew definition), ADR 0054 ("role holders"), ADR 0031 (AttachableSet is retired, not split)
**Relates to:** ADR 0047 (stored data stays decodable for one generation), ADR 0051 (presentation metadata and `pm connect`), #2872 (host loss and rehydration), #2894 (the held transport draft that prompted this)

## Context

Draft #2894 added Codex app-server delivery and, on the way, invented
delivery semantics: a per-session "holder transport" list, two kinds of
acknowledgement, urgency rules, a new attention source and a per-vessel
daemon. None of the concepts it rested on was defined. A survey of main at
95ff9ae2 found why:

- The only modelled thing running in a vessel is the `TerminalSession`. Tool
  crew members pretend to be terminal sessions, the per-environment cleat
  daemon has no record, and a service such as a Codex app-server has nowhere
  to live.
- The agent's conversation (harness, thread, transcript, agent home) has no
  record of its own. It is spread across a TerminalSession, crew work status
  and adapter state, so it cannot outlive a process in any explicit way.
- "Holder", used throughout ADR 0055 and the role-routing docs, was never
  defined. In practice it meant "the live session currently filling a role
  address".
- "Delivered" meant "tool activity or a hook-reported Working state seen after
  the keystrokes were sent", which proves less than the word suggests.
- Several declared lifecycle phases are never written, and terminal convoy
  phases are re-opened by resume and turn delivery.
- Two terminal models coexist: the per-checkout AttachableSet registry and
  TerminalSession, with separate attach paths and attention sources.

This ADR settles the vocabulary and the decisions. Record shapes and the
transport implementations follow in their own tickets under #2912.

## Decisions

### 1. Process is first-class, and non-agents are never crew

A **Process** is anything executing aboard a vessel: an agent's harness
process, tools, services (a Codex app-server, an MCP server), the cleat daemon
for an environment, and one-shot jobs. A terminal is a property some processes
have, not what makes something a process. Tool "crew members"
(`CrewSource::Tool` today) become vessel-declared processes. Crew is reserved
for agents.

*Rationale:* the substrate already runs non-terminal, non-agent programs and
has nowhere honest to put them. Calling a test runner "crew" makes it look
like something that takes briefs, receives Messages and stalls, none of which
apply. A process kind with its own lifecycle gives the cleat daemon and an
app-server an owner, and lets the agent's process be one process among
several.

### 2. AgentSession is the durable conversation

An **AgentSession** is one agent conversation: harness and model, the
harness's thread or conversation id, its transcript and agent home, its grants,
and its park depth (`live`, `parked`, `archived`). It outlives any single
process. A harness process is how a live AgentSession runs right now; when the
process ends, the session can be parked and later revived on a new process.

*Rationale:* the thing that matters across restarts, resumes and host moves is
the conversation, not the PTY. Making it a record gives revival, archival
(#2889) and rehydration (#2872) a subject, and separates "the agent" from "the
terminal currently showing it".

### 3. Role bindings replace "holder"; crew means agent sessions working for a vessel

An AgentSession fills a role through a **role binding**: a record that this
session fills this role address, from when until when. Bindings keep history.
A role address resolves to the AgentSession its current binding names. A role
with no current binding is **unbound**, and Messages to it wait. More than one
current binding for a role is refused.

**Crew** means the AgentSessions working in or for a vessel, including a
session whose agent loop runs elsewhere but works for that vessel. An
operator-style session that does not work for a vessel is not crew. The word
"holder" is retired.

*Rationale:* "holder" hid two things: which conversation fills a role, and
since when. A binding with history answers both, and makes "the role was
re-bound to a fresh session" an ordinary recorded fact rather than an implicit
swap. Defining crew by what the session works for, rather than where its
process runs, keeps central agent loops (reasoning outside the vessel, tools
inside) inside the model.

### 4. AgentChannel binds a session to a transport

An **AgentChannel** is an AgentSession's current binding to a **transport** at
a concrete endpoint on the session's current Process. The transports named so
far are `keystroke` (typing into the terminal through cleat),
`codex-app-server` (JSON-RPC turns) and `claude-channels` (Claude Code's
Channels). The AgentAdapter declares which transports a harness supports, by
harness version; the AgentChannel records which one is bound.

Binding happens when a session is launched or revived. Any later fallback is
an explicit re-bind with a recorded reason, never a silent fallthrough at
send time. Docs and code say "AgentChannel" or "transport", never a bare
"channel", which collides with Claude Code's Channels feature.

*Rationale:* #2894's per-send preference list made the transport a property of
each delivery attempt, so the same session could receive input through
different paths with different guarantees and nobody could say which applied.
One recorded binding per session makes the guarantee knowable before a send,
keeps adapter capability (what is possible) separate from session state (what
is in use), and fits adapter definitions as data (#2836).

### 5. Delivery receipts say what the channel proves

A Message delivery receipt records what the bound AgentChannel can prove:

- `sent`: the transport accepted the input;
- `received`: the harness echoed the input back (for example a correlated user
  message id from the Codex app-server).

"Activity followed" (a hook or screen showed the agent working after the
send) is a separate derived observation, never a receipt. "Acted on" belongs
only to the Message's expectation: a reply or an outcome. Today's keystroke
transport proves `sent`.

*Rationale:* ADR 0055 called input "delivered" when activity followed it.
Activity can have other causes, and a received message can still be ignored.
Recording only what the transport proves keeps the receipt truthful across
transports of different strength, and leaves judgement of the agent's response
to the expectation layer, which already exists for it.

### 6. Urgency is parked

No producer sends urgent Messages today, so urgency (interrupting or steering a
running turn) is not specified. When a real producer exists, the default is to
interrupt and then deliver at the new turn boundary, per harness, rather than
inject input mid-turn.

*Rationale:* mid-turn semantics differ by harness; Claude frames mid-turn
Channels input as untrusted data (#2660). Designing urgency without a consumer
would fix those differences into the model before anyone needs them.

### 7. Retirements

- The **AttachableSet** registry (`attachables/registry.json`) and the
  personal-checkout workspace commands built on it. Terminals are
  TerminalSessions, and later Processes.
- The **PresentationManager providers** (cmux, zellij) and the presentation
  reconciler that drives them. Future cmux and plain-zellij adapters consume
  the `pm connect` stream (ADR 0051) as clients outside flotilla's core; #2916
  records the research.

*Rationale:* two terminal models with separate attach paths and attention
sources are a standing source of drift. ADR 0031 said AttachableSet should be
"audited and split"; the audit (#2912's survey) found nothing in it that the
TerminalSession model cannot hold. Presentation already has a neutral metadata
stream; per-multiplexer providers inside the daemon duplicate it.

### 8. Lifecycle phases: written, terminal, and `lost`

- Phases that no code writes are deleted. As of 95ff9ae2 these are Convoy
  `Anchored` and `Cancelled`, Vessel `TearingDown`, Environment `Terminating`,
  Checkout `Preparing` and `Terminating`, and the stall rung `Supervisor`.
  (The survey also listed the `Bosun` rung, but the leaf engine writes it for
  a convoy-crew supervision target, so it stays.) Stored records keep decoding
  the deleted values for one roll (ADR 0047).
- A single `lost` state covers environment loss (the host or container is
  gone), distinct from `Failed`. It pairs with AgentSession park depth: the
  sessions aboard drop to `parked` or `archived`, depending on what survived,
  and can be revived elsewhere (#2872). TerminalSession's existing `Lost`
  (liveness unknown, often self-recovering) is a process-level fact; the
  record-shape work decides its name.
- Terminal means terminal. Nothing re-opens a `Landed`, `Failed` or
  `Abandoned` convoy. More work is an explicit continuation: a new generation,
  or a new convoy with `--continue-pr`.

*Rationale:* readers still branch on phases nothing writes, and cleanup keys
on `is_terminal` while resume flips terminal convoys back to `Active`. Both
make the phase field untrustworthy. Host loss collapsing into `Failed` made a
recoverable situation look final and blocked `convoy resume`.

### 9. The brief starts a conversation; revival does not re-brief

A **Brief** belongs to starting a **new** conversation for a role: the first
start, or the fresh-agent rung of the delivery ladder. Revival is ordinary:
resume the same conversation, re-bind its AgentChannel, and deliver pending
Messages normally. Material changes while a session was parked (a charter
revision, a ruling) arrive as ordinary superseding `system:` Messages, never as
a forced re-brief.

*Rationale:* a revived conversation already has its brief in its transcript;
sending it again either duplicates context or contradicts what the agent has
since learned. Messages already carry supersession, so changes made while a
session was parked need no second mechanism.

## Supersedes and amends

| Ruling | What it said | What stands now |
|--------|--------------|-----------------|
| ADR 0055 (delivery) | Delivery means evidence that a resolved holder accepted the input; activity after submission counts | Receipts record `sent` or `received` as the AgentChannel proves; activity is a derived observation; acting on input is judged by the expectation |
| ADR 0055 (addressing) | Holders are resolved at delivery | The role's current role binding names the AgentSession resolved at delivery; "holder" is retired |
| ADR 0055 (interrupts) | Supervisor senders may request interrupts where supported | Parked until a producer exists (decision 6) |
| ADR 0028 (delivery ladder) | Warm session → adapter resume from the session log → fresh agent with a reconstructed brief | Revive the bound AgentSession (live, or parked and resumed on a new process with a re-bound AgentChannel) → start a new AgentSession for the role with a brief. Only the floor starts a new conversation, so only the floor briefs |
| ADR 0028 (park depth) | Park depth is a vessel spectrum: warm process → suspended vessel → no vessel | Unchanged for vessels; AgentSession has its own park depth (`live`, `parked`, `archived`) |
| ADR 0010 (tool crew) | Crew members may be agents or tools (`ProcessSource`/`CrewSource::Tool`) | Tools are vessel-declared Processes; crew is agent sessions only |
| ADR 0010 (Crew) | A crew is the processes aboard a vessel for a Leg | Crew is the AgentSessions working in or for a vessel |
| ADR 0031 (attachables) | Audit and split AttachableSet consumers | Retire the AttachableSet registry |
| ADR 0054 (role holders) | The fleet reuses Project role holders | The fleet reuses Project role bindings |

## Not decided here

- Record shapes for Process, AgentSession, role binding and AgentChannel.
- The transport implementations: Codex app-server, Claude Channels, and
  keystroke through cleat's guarded send (#2649), reworked from #2714 and
  #2894.
- Dead letters and the stall ladder on Message expectations (#2712), and
  reference gating (#2711).
- Rehydration of parked sessions on another host, and object-storage logs
  (#2872, #2889).
- Central agent loops and cloud-agent vessels.
- Where patch-to-tool translation for cmux and zellij lives (#2916).
