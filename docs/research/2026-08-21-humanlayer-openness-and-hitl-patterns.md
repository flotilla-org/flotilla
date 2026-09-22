# HumanLayer: what is actually open, and what its human-in-the-loop model offers Flotilla

**Date:** 2026-08-21

**Question:** "Not sure how much of what they are making is open to look at."
Answer that precisely, then extract whatever their human-in-the-loop design
offers Flotilla's crew/brief/convoy model.

**Method:** Primary sources only. The `humanlayer` GitHub org read through the
authenticated GitHub API (repo metadata, licence files, source trees at
specific refs); their live sites and docs fetched directly; DNS and TLS state
of their old endpoints checked directly. Flotilla-side claims are cited to
this repo at `docs/adr/` and `crates/`.

## Executive answer

**Most of what HumanLayer ships today is closed. Everything genuinely open is
either abandoned, a stub, or prose.**

The company has run through three products in two years, and the open-source
artefact of each generation was abandoned as the next arrived:

| Generation | Artefact | Openness today |
|---|---|---|
| 2024–2025: hosted approval API + SDKs | `humanlayer/humanlayer` (Apache-2.0, 11.3k stars) | Source still on GitHub, **declared deprecated**; the API it called (`api.humanlayer.dev`) **no longer resolves** |
| 2025: k8s agent scheduler | `humanlayer/agentcontrolplane` (Apache-2.0, 460 stars) | Source intact, **dormant ~14 months**; all HITL paths hard-require the now-dead hosted API |
| 2025–2026: CodeLayer, then "HumanLayer"/Riptide desktop app | — | **Closed.** Source repos `humanlayer/{riptide,synclayer,codelayer}` all 404 |

Their own answer, from the FAQ at
[humanlayer.com/#faq-oss](https://www.humanlayer.com/#faq-oss):

> **"Is HumanLayer open source?"** — "Not yet, but we'll be open-sourcing some
> of the building blocks in the coming weeks and months. Our Research, Plan,
> Implement (RPI) framework is already open source."

So: read them for ideas, do not depend on them for anything. There is no
service worth integrating with — the approval API that would have been the
integration target is dead — and no library worth vendoring. What is worth
having is the *data shapes*, which are good, and one design mistake of theirs
that Flotilla should avoid repeating.

The comparison with Flotilla is more favourable than expected. Flotilla's
`Demand`/`Regard` model (ADR 0018) is already a **more considered** design
than anything HumanLayer shipped: HumanLayer never made a pending human
request a first-class resource, and never had a principal-side model at all.
The three concrete things worth lifting are a **typed verdict** on a Demand,
**enumerated response options**, and an **expiry/deadline** — all of which
they specified and Flotilla has not (§5).

---

## 1. Openness inventory

### 1.1 The org, repo by repo

All 21 public repos in `github.com/humanlayer`, sorted by last push
(GitHub API, `/orgs/humanlayer/repos`, fetched 2026-08-21). Licence column is
the *verified* licence, i.e. the LICENSE file's actual text, not the API's
inference.

| Repo | Licence | Stars | Last push | What it is |
|---|---|---|---|---|
| `homebrew-humanlayer` | Apache-2.0 (tap only) | 28 | 2026-08-21 | Casks pointing at **closed** signed DMGs |
| `ampere` | **none** | 0 | 2026-08-20 | Early Effect/TS re-imagining of ElectricSQL sync |
| `fold` | MIT | 45 | 2026-08-20 | Real, substantial Effect-native agent core |
| `skills` | MIT | 385 | 2026-08-13 | Claude Code skill/plugin marketplace (prompts) |
| `effect-channels` | MIT | 1 | 2026-08-05 | **Empty stub** — 3 files, no code |
| `advanced-context-engineering-for-coding-agents` | **none** | 2523 | 2026-08-04 | Prose essay (ACE-FCA) |
| `rpi-coordination-template` | **none** | 36 | 2026-08-03 | 4-file multi-repo coordination shell |
| `effect-durable-streams` | **none** | 11 | 2026-07-08 | Real, complete durable-stream server |
| `humanlayer` | Apache-2.0 | 11306 | 2026-06-19 | **Deprecated** monorepo (SDK, daemon, CodeLayer) |
| `pulumi-resend`, `pulumi-stripe` | MIT | 3, 5 | 2026 | Unrelated Pulumi providers |
| `12-factor-agents` | Apache-2.0 (code) / CC BY-SA 4.0 (content) | 25401 | 2025-09-21 | The essay series |
| `agentcontrolplane` | Apache-2.0 | 460 | 2025-07-02 | Dormant k8s CRD control plane |
| `claudelayer` | **none** | 25 | 2026-01-12 | Dormant; hosts the RPI command prompts |
| `riptide-rpi` | **none** (`plugin.json`: "All Rights Reserved") | 8 | 2025-12-29 | One Claude Code skill, closed-tool dependent |
| `mcp-cli` | **none** | 7 | 2025-11-26 | **Empty stub** — 2 files |
| plus 5 forks and archived repos | | | | |

Two patterns matter. First, **licensing is bimodal**: the libraries carry MIT
or Apache-2.0, but the high-value methodology assets do not.
`advanced-context-engineering-for-coding-agents` has 2,523 stars and **no
LICENSE file at all** — meaning all rights reserved. `riptide-rpi` declares
"All Rights Reserved" explicitly in its `plugin.json`. The claim that "our RPI
framework is already open source" is therefore loosely supported at best: the
essay is unlicensed, the plugin is all-rights-reserved, and the actual RPI
subagent prompts are only readable because they were ported into MIT-licensed
`fold` (`packages/fold-agent/src/Mode/Rpi.ts`) and left behind in unlicensed
`claudelayer` (`.claude/commands/*.md`).

Second, **two of the most promising repo names are empty**. `effect-channels`
("Effect-based agent chat channels for linear, slack, github, etc") is a name
reservation created 2026-08-05: `.gitignore`, `LICENSE`, and a two-line
README, verified via
`gh api /repos/humanlayer/effect-channels/git/trees/HEAD?recursive=1`. There
is no channel abstraction to study. `mcp-cli` is the same.

### 1.2 The deprecation, in their own words

The entire current README of `humanlayer/humanlayer` (replaced 2026-06-19,
commit `99abe673`, author "Dex"):

> "## HumanLayer
>
> public issues repo for humanlayer - the code here is pretty much all
> deprecated - you can try the rebuild of humanlayer at https://humanlayer.com
> - thanks for all your support - dex"

The repo is **not archived**, still has issues enabled, still carries its
Apache-2.0 LICENSE ("Copyright (c) 2024, humanlayer Authors"), and the old
source tree is intact — `hld/`, `hlyr/`, `humanlayer-wui/`, `claudecode-go/`
are all still there and readable. It is a graveyard you are allowed to walk
through, which is why §2 below can quote it.

The infrastructure behind the old product is genuinely gone, not merely
unmaintained:

| Endpoint | State (checked 2026-08-21) |
|---|---|
| `api.humanlayer.dev` — the SDK's hard-coded base URL | **NXDOMAIN** |
| `docs.humanlayer.dev`, `app.humanlayer.dev` | **expired TLS certificate**, unreachable |
| `humanlayer.dev/docs/quickstart-python` (the README's own link) | **404** |
| `humanlayer.dev/code` (CodeLayer waitlist) | **308 → `/`**, i.e. deliberately removed |
| PyPI `humanlayer` | frozen at 0.7.9, last upload 2025-06-03 |
| `github.com/humanlayer/{riptide,synclayer,codelayer}` | **404** (private) |

There is no dated sunset notice, no migration guide, and no blog post about
any of it. The blog (10 posts, all technique content, latest 2026-08-12)
contains **zero** posts about the pivot, the API shutdown, or closing the
source. `humanlayer.dev` and `humanlayer.com` now serve byte-identical pages —
the old domain was simply repointed at the new product.

### 1.3 What the current product is

A proprietary macOS desktop app plus a hosted backend, sold at **$100
per user per month** (Pro tier, published inline at
[humanlayer.com/#pricing](https://www.humanlayer.com/#pricing)); Starter is
free up to 3 members and 200 sessions/month; Enterprise adds SSO and
"On-Prem & Private VPC" as a custom line item. Bring-your-own-key for model
access: "plug in your Claude, Codex, and other AI subscriptions or API keys."

Distribution is a signed, closed binary. `Casks/humanlayer.rb` in the tap:

```ruby
cask "humanlayer" do
  version "0.163.0"
  sha256 "ae473e1e61eafb5fd618ec0a77da2162ceef7ef650b441518d58d88c05dfb323"

  url "https://github.com/humanlayer/homebrew-humanlayer/releases/download/riptide-v0.163.0/Riptide-darwin-arm64.dmg",
      verified: "github.com/humanlayer/homebrew-humanlayer/"
  name "HumanLayer"
  desc "AI coding agent powered by Claude"
  ...
  binary "#{appdir}/HumanLayer.app/Contents/Resources/bin/riptided", target: "riptided"
end
```

The internal codename is Riptide; `humanlayer` and `riptide` are the same
artefact (identical sha256) under two names. The bundled daemon is `riptided`
— the closed successor to the open `hld` daemon quoted in §2.3. Release
cadence is heavy and current (v0.163.0 published 2026-08-21T04:13Z, v0.162.1
three hours earlier), so the closed product is where all the work goes. The
npm CLI `@humanlayer/cli` publishes with `license: None` and no repository
field.

Note for Flotilla's own trajectory: their "remote daemons" feature
([docs.humanlayer.com/guide/remote-daemons](https://docs.humanlayer.com/guide/remote-daemons))
is *not* self-hosting. You run the execution host; you still authenticate to
their hosted control plane by minting a launch token at `app.humanlayer.com`.
That is the same multi-host shape Flotilla has, with the control plane
retained as the commercial moat.

---

## 2. The human-in-the-loop model, across three generations

HumanLayer built the same idea three times. The data model barely changed; the
placement of authority changed completely, and the last version is the one
Flotilla should pay attention to because it is the one where they gave up on
the hosted service being in the loop.

### 2.1 Generation 1 — the hosted approval API and SDK

Two object kinds, both `run_id` + `call_id` keyed, both spec/status shaped.
From `humanlayer/core/models.py` at ref `760e769f` (2025-05-29):

```python
class FunctionCallSpec(BaseModel):
    fn: str
    kwargs: dict
    channel: ContactChannel | None = None
    reject_options: list[ResponseOption] | None = None
    state: dict | None = None  # Optional state to be preserved across the request lifecycle


class FunctionCallStatus(BaseModel):
    requested_at: datetime | None = None
    responded_at: datetime | None = None
    approved: bool | None = None
    comment: str | None = None
    reject_option_name: str | None = None
    slack_message_ts: str | None = None
```

```python
class HumanContactSpec(BaseModel):
    msg: str
    subject: str | None = None
    channel: ContactChannel | None = None
    response_options: list[ResponseOption] | None = None
    state: dict | None = None


class HumanContactStatus(BaseModel):
    requested_at: datetime | None = None
    responded_at: datetime | None = None
    response: str | None = None
    response_option_name: str | None = None
```

Four things in these twenty lines are worth stealing, and they are the same
four Flotilla is missing (§5):

1. **The verdict is typed and its evidence is on the status**, not smuggled
   through a message. `approved: bool | None` — three-valued by construction,
   with `None` meaning *not yet answered*. `requested_at`/`responded_at` make
   latency and staleness computable without a separate audit trail.
2. **Rejection is structured, not free text.** `reject_options:
   list[ResponseOption]` lets the *requester* enumerate the ways a human may
   say no, and `reject_option_name` records which one was chosen. The
   `ResponseOption` shape is `{name, title, description, prompt_fill,
   interactive}` — `prompt_fill` is the pre-written text handed back to the
   agent, so a rejection carries a machine-actionable instruction rather than
   prose the agent has to interpret.
3. **`state: dict` is opaque continuation context** round-tripped through the
   human interaction untouched. In their webhook resume path the agent stores
   `state: {thread_id: ...}` and reads it back off the response, so the
   correlation key never has to live in a side table.
4. **Rejection is asymmetric in its evidence requirement.** `as_completed()`
   raises `ValueError("FunctionCallStatus.Rejected with no comment")` — you
   may approve silently, but you may not refuse silently. That is a good rule
   and it is enforced in the type, not in a docstring.

Contact channels are a tagged union over `slack | sms | whatsapp | email`,
each with a `context_about_user` field whose stated purpose is to *rewrite the
tool description the model sees* — "the user you are assisting" turns the tool
into `contact_human_via_email_to_the_user_you_are_assisting`. Slack adds
`allowed_responder_ids` (an authorisation list on who may answer), `thread_ts`
for threading, and an optional per-channel `bot_token`. Email adds
`additional_recipients` with `to|cc|bcc`, RFC-822 `in_reply_to_message_id` /
`references_message_id` for thread continuity, and a Jinja2 `template`.

Escalation exists as a verb, not a policy: `Escalation {escalation_msg,
additional_recipients}` posted to
`/function_calls/{call_id}/escalate_email`. Something outside the model
decides when to call it.

**There is no timeout, deadline, TTL, or expiry anywhere in this model.** A
pending request waits forever. Their `drafts/a2h-spec.md` in the 12-factor
repo — an unfinished "Agent-to-Human protocol", k8s-shaped down to
`apiVersion: proto.a2h.dev/v1alpha1` — lists `prioritizedContactChannels` per
human but specifies no failover rule between them. This gap is consistent
across all three generations and is the single clearest "do not copy this"
finding.

One idea in that draft is genuinely good and has no Flotilla equivalent:

> "This separation allows for agents to query and find humans to contact,
> without exposing the human's contact details to the agent. It is the
> responsibility of the A2H provider to relay agent requests to the
> appropriate human via that human's preferred contact channel(s)."

The agent addresses a *person*, never an address. Delivery route is
administrative data the agent cannot see.

### 2.2 Generation 2 — Agent Control Plane (the k8s attempt)

`agentcontrolplane`, Apache-2.0 (the API's NOASSERTION is an artefact of a
short-form LICENSE notice), API group `acp.humanlayer.dev/v1alpha1`. Six
kinds: `LLM`, `Agent`, `MCPServer`, `Task`, `ToolCall`, `ContactChannel`.
Dormant since 2025-07-02, no deprecation notice, README says only
"ACP is in alpha".

This is the closest structural analogue to Flotilla's control plane, and its
central design decision is the one to learn from — **negatively**.

**There is no `HumanContact` or `Approval` CRD.** A pending human interaction
is a *phase* on `ToolCall` plus an opaque foreign key
(`api/v1alpha1/toolcall_types.go:65-66`):

```go
	// ExternalCallID is the unique identifier for this function call in external services
	ExternalCallID string `json:"externalCallID"`
```

The phase enum carries the state (`toolcall_types.go:89-116`):
`AwaitingHumanApproval`, `ReadyToExecuteApprovedTool`, `ToolCallRejected`,
`AwaitingHumanInput`, `ErrorRequestingHumanApproval`,
`ErrorRequestingHumanInput`.

The consequences of that choice are visible as damage in the code:

- The rich verdict from §2.1 collapses into prose. From
  `internal/controller/toolcall/state_machine.go:149-160`, a rejection sets
  `Status: Succeeded` (the *resource* succeeded at obtaining a verdict) and
  `tc.Status.Result = fmt.Sprintf("Rejected: %s", status.GetComment())`. The
  `reject_option_name` that generation 1 modelled is discarded.
- The human-as-tool path recovers its own identifier by string-parsing its own
  return value (`state_machine.go:187-194`):
  `parts := strings.Split(result, "call ID: ")`.
- The controller re-resolves the `ContactChannel` on every 5-second poll purely
  to re-read an API key, because the pending request holds no state of its own.

**Approval gating is per-MCP-server, all-or-nothing** — a single
`approvalContactChannel *LocalObjectReference` on `MCPServerSpec`, with no
per-tool or per-argument granularity (their own open issue #104 asks for it).
So there is no prior art here for "gate the `merge` verb but not the `read`
verb"; they wanted it and never built it.

**Every HITL path requires their hosted service.** The endpoint is hard-coded
in four places, e.g. `internal/humanlayer/client.go:63-72`:

```go
func NewClient(apiKey string) externalapi.Client {
	return &Client{
		apiKey:  apiKey,
		baseURL: "https://api.humanlayer.dev/humanlayer/v1/function_calls",
```

`HUMANLAYER_API_BASE` is the sole, process-wide override, and the ToolCall
controller always constructs its factory with an empty base
(`toolcall_controller.go:84`). The `ContactChannel` controller cannot even
reach `Ready` without a live call to `/humanlayer/v1/project`, whose response
it stores as `projectSlug`/`orgSlug` **in the CRD status** — vendor tenancy
promoted to first-class API surface. Since `api.humanlayer.dev` is now
NXDOMAIN, every ContactChannel in every ACP cluster is permanently `Error`.
The README admits the missing alternative: "Directly approving tool calls with
`kubectl` or a `acp` CLI is planned but not yet supported."

Two structural details are worth keeping, though:

- **`ToolType` as a spec-level enum** (`MCP | HumanContact | DelegateToAgent`)
  gives one state machine, one terminal fold-back, three executors. Asking a
  human, calling a tool, and delegating to a sub-agent are the same shape of
  suspension. Sub-agent delegation genuinely reuses the machinery: it creates
  a child `Task` and polls it from `AwaitingSubAgent`.
- **Absolutely everything is poll-based** — `RequeueAfter: 5s` with label
  selectors, no watches, no callbacks, no webhook receiver. Flotilla's
  aggregator and watch model is already past this.

### 2.3 Generation 3 — local approvals in the CodeLayer daemon

This is the most relevant generation and the least discussed, because it is
buried in the deprecated monorepo. When they built a *desktop* product they
abandoned the hosted approval service for their own tool calls and built a
local one in `hld`, the Go daemon.

The interface (`hld/approval/types.go`, entire file minus imports):

```go
// Manager defines the interface for managing local approvals
type Manager interface {
	CreateApproval(ctx context.Context, runID, toolName string, toolInput json.RawMessage) (string, error)
	CreateApprovalWithToolUseID(ctx context.Context, sessionID, toolName string, toolInput json.RawMessage, toolUseID string) (*store.Approval, error)
	GetPendingApprovals(ctx context.Context, sessionID string) ([]*store.Approval, error)
	GetApproval(ctx context.Context, id string) (*store.Approval, error)
	ApproveToolCall(ctx context.Context, id string, comment string) error
	DenyToolCall(ctx context.Context, id string, reason string) error
}
```

The record (`hld/store/store.go:237-248`), with statuses
`pending | approved | denied`:

```go
type Approval struct {
	ID          string          `json:"id"`
	RunID       string          `json:"run_id"`
	SessionID   string          `json:"session_id"`
	ToolUseID   *string         `json:"tool_use_id,omitempty"`
	Status      ApprovalStatus  `json:"status"`
	CreatedAt   time.Time       `json:"created_at"`
	RespondedAt *time.Time      `json:"responded_at,omitempty"`
	ToolName    string          `json:"tool_name"`
	ToolInput   json.RawMessage `json:"tool_input"`
	Comment     string          `json:"comment,omitempty"`
}
```

The delivery mechanism is the interesting part, and it maps almost exactly
onto Flotilla's daemon/TUI split. The daemon **hosts an MCP server** exposing
a single tool, and configures the coding agent to call it as its permission
handler (`hld/mcp/server.go`):

```go
	s.mcpServer.AddTool(
		mcp.NewTool("request_approval",
			mcp.WithDescription("Request permission to execute a tool"),
			mcp.WithString("tool_name", mcp.Description("The name of the tool requesting permission"), mcp.Required()),
			mcp.WithObject("input", mcp.Description("The input to the tool"), mcp.Required()),
			mcp.WithString("tool_use_id", mcp.Description("Unique identifier for this tool use"), mcp.Required()),
		),
		s.handleRequestApproval,
	)
```

The handler creates the local approval record, then blocks on a channel from
`pendingApprovals sync.Map[string]chan ApprovalDecision`, woken by an event
bus subscriber. The response is Claude Code's permission-handler protocol:
`{"behavior": "deny", "message": ...}`. There is an `MCP_AUTO_DENY_ALL`
environment escape hatch for testing, which is a small good idea.

The daemon then exposes approvals over JSON-RPC 2.0 on a Unix socket at
`~/.humanlayer/daemon.sock`, mode 0600, line-delimited JSON
(`hld/PROTOCOL.md`) — `fetchApprovals`, `sendDecision` with
`decision: approve|deny` where "deny requires comment", and a subscription
stream carrying `new_approval` and `approval_resolved` events.

That is, structurally: **daemon-owned approval store + MCP tool as the
agent-side capture + socket RPC and an event stream as the surface-side
delivery.** Flotilla already has every one of those pieces — the daemon, the
socket protocol, the event stream, the aggregator, and hook-based attention
capture (ADR 0017). What Flotilla does not have is the *capture* seam for an
agent to raise a request deliberately, as opposed to the harness's permission
prompt being *observed*.

The gap between observation and request is exactly Robert's question, and §5
is about closing it.

---

## 3. 12-factor agents

`humanlayer/12-factor-agents`, Apache-2.0 for code and CC BY-SA 4.0 for
content, 25.4k stars, last pushed 2025-09-21. It is a genuine essay series,
not marketing, and factors 5–8 and 11 are the relevant ones.

The full list, from `README.md`:

1. **Natural Language to Tool Calls** — the atomic pattern is turning a
   natural-language phrase into a structured object that deterministic code
   executes.
2. **Own your prompts** — prompts are first-class code, not framework
   internals.
3. **Own your context window** — you don't need `role`/`content`; build your
   own token-efficient serialisation.
4. **Tools are just structured outputs** — a tool call is JSON your switch
   statement interprets: "Just because an LLM 'called a tool' doesn't mean you
   have to go execute a specific corresponding function in the same way every
   time."
5. **Unify execution state and business state** — infer execution state
   (current step, waiting status, retry counts) from the one append-only
   thread; don't run a second state machine beside it.
6. **Launch/Pause/Resume with simple APIs** — launch, query, resume, stop as
   ordinary APIs; external triggers resume a paused agent "without deep
   integration with the agent orchestrator."
7. **Contact humans with tool calls** — human contact is a structured tool
   intent, and the answer arrives later as another event on the thread.
8. **Own your control flow** — write your own loop so intents can break out of
   it, wait, compact, rate-limit, or durably sleep.
9. **Compact errors into the context window** — bounded, with a counter;
   crossing the threshold is a good escalation point.
10. **Small, focused agents** — "3-10, maybe 20 steps max" inside a mostly
    deterministic system.
11. **Trigger from anywhere, meet users where they are** — Slack, email, SMS
    in and out.
12. **Make your agent a stateless reducer** — `foldl` over the event list.
    (The file's entire body is "This one is mostly just for fun.")

Plus appendix factor 13, pre-fetch context you know you'll need.

### 3.1 The parts that bear on Flotilla

**Factor 7** is the core. Its argument is that a human request should be an
ordinary structured intent, not a mode switch:

```python
class Options:
  urgency: Literal["low", "medium", "high"]
  format: Literal["free_text", "yes_no", "multiple_choice"]
  choices: List[str]

class RequestHumanInput:
  intent: "request_human_input"
  question: str
  context: str
  options: Options
```

`urgency` and `format` are the two fields worth noting. `format` tells the
*surface* how to render the ask — a yes/no gets buttons, a multiple-choice
gets a list, free text gets a box — without the surface parsing prose.
`urgency` is routing input.

The rendered thread shows the answer as a peer event carrying attribution:

```xml
<human_response>
    response: "yes please proceed"
    approved: true
    timestamp: "2024-03-15T10:30:00Z"
    user: "alex@company.com"
</human_response>
```

The stated benefits include the case Flotilla actually has:

> "**Inner vs Outer Loop**: Enables agents workflows **outside** of the
> traditional chatGPT-style interface, where the control flow and context
> initialization may be `Agent->Human` rather than `Human->Agent`"

**Factor 8** contains the sharpest claim in the whole series, and it is a
direct critique of every harness Flotilla drives:

> "the number one feature request I have for every AI framework out there is we
> need to be able to interrupt a working agent and resume later, ESPECIALLY
> between the moment of tool **selection** and the moment of tool
> **invocation**.
>
> Without this level of resumability/granularity, there's no way to
> review/approve the tool call before it runs, which means you're forced to
> either:
> 1. Pause the task in memory while waiting for the long-running thing to
>    complete (think `while...sleep`) and restart it from the beginning if the
>    process is interrupted
> 2. Restrict the agent to only low-stakes, low-risk calls like research and
>    summarization
> 3. Give the agent access to do bigger, more useful things, and just yolo hope
>    it doesn't screw up"

The durability pattern in their loop is uniform: append the intent event →
contact the human → `db.save_thread(thread)` → `break`. Persist, then break;
resume by webhook. Never hold the pause in a call stack.

Their TypeScript workshop makes the pause predicate *derived*, which is factor
5 in practice — nothing stores "we are waiting":

```ts
    awaitingHumanApproval(): boolean {
        const lastEvent = this.events[this.events.length - 1];
        return lastEvent.data.intent === 'divide';
    }
```

and turns a rejection into a tool result the agent re-plans against, rather
than an abort:

```ts
    } else if (thread.awaitingHumanApproval() && body.type === 'approval' && !body.approved) {
        thread.events.push({
            type: "tool_response",
            data: `user denied the operation with feedback: "${body.comment}"`
        });
```

**What the factors do not cover**, stated plainly because it bounds their
usefulness: no timeout or expiry on a pending request; no escalation ladder
between humans; nothing about several agents contending for one person's
attention; and nothing about resuming an agent that is bound to a *workspace*
rather than just a thread. Factor 5 waves at the last one — "You may have
things that can't go in the context window, like session ids" — and says to
minimise them. Flotilla cannot minimise them; a warm crew in a worktree is
precisely the thing that cannot be folded into a thread. That is the boundary
where the 12 factors stop being applicable to Flotilla and ADR 0027's
verbs-over-the-log model takes over.

### 3.2 RPI, and where a human reviews

Their current methodology (`advanced-context-engineering-for-coding-agents`,
unlicensed) is Research → Plan → Implement with "frequent intentional
compaction", each phase ending in a compacted markdown artefact, context kept
at 40–60% utilisation. The load-bearing claim is about *where* review goes:

> "A bad line of code is… a bad line of code. But a bad line of a **plan**
> could lead to hundreds of bad lines of code. And a bad line of **research**,
> a misunderstanding of how the codebase works or where certain functionality
> is located, could land you with thousands of bad lines of code."

So the gates sit on the research document and the plan, deliberately not on
the diff — with the explicit warning "You have to engage with your task when
you're doing this or it WILL NOT WORK."

The mechanism, however, is worth being clear-eyed about: **every RPI gate is a
prompt-level convention, not a runtime primitive.** From `implement_plan.md`:

> "**Pause for human verification**: After completing all automated
> verification for a phase, pause and inform the human that the phase is ready
> for manual testing. […] do not check off items in the manual testing steps
> until confirmed by the user."

Nothing blocks. The model is instructed to stop talking and wait. That is the
entire HITL story across their current public surface — despite the company
name. `fold`, their live MIT agent core, has `steer`/`stop`/`interrupt` and a
`preToolUse` hook whose decision union
(`continue{params}` | `replaceResult{result, isFailure}`) could implement a
deny, but ships **no approval gate at all**; its `bashTool` just runs.

Flotilla's briefs use the same prompt-convention mechanism today. The
difference is that Flotilla has a control plane underneath that could make the
gate real, and they no longer publish one.

---

## 4. Measured against Flotilla

Flotilla is not behind here. Setting the models side by side:

| Concern | HumanLayer, best generation | Flotilla today |
|---|---|---|
| Pending human request as a resource | Only in gen-1 as a hosted object; ACP demoted it to a phase + opaque ID | **`Demand`** — a real resource, `principal_attention.rs` |
| Principal-side model | none | **`Regard`**, searchlight, decay/pin (ADR 0018) |
| Routing | `channel` on the spec; agent picks an address | **Join against the addressee's regards**; out-of-searchlight demands escalate (ADR 0018) |
| Attention observation | none | `TerminalAttention` — `Working`/`NeedsInput`/`Idle`/`Unobservable` with `as_of`, hook-primary, screen-fallback (ADR 0017) |
| Claim vs evidence | conflated | **Three planes, explicitly non-collapsible** (ADR 0017) |
| Typed verdict | three-valued `approved`, plus `comment` and `reject_option_name` | **absent** |
| Enumerated response options | `ResponseOption {name, title, description, prompt_fill, interactive}` | **absent** |
| Question payload | `msg`, `subject`, `question`, `context`, `format`, `urgency` | **absent** — `DemandSpec` has no payload |
| Deadline / expiry | **absent** (all three generations) | **absent** on Demand (Regard decays; Demand does not) |
| Channel adapters | Slack, email, SMS, WhatsApp, with threading | **absent** — attach to the terminal, or the TUI |
| Gate on a specific verb | wanted (issue #104), never built | **absent** |

Flotilla's `DemandSpec` is currently three fields
(`crates/flotilla-resources/src/principal_attention.rs:62-67`):

```rust
pub struct DemandSpec {
    pub originating_work_ref: ResourceRef,
    pub kind: DemandKind,
    pub addressee: DemandAddressee,
}
```

with `DemandKind` = `Permission | HumanGate | Review`, addressee `Principal` or
`Pool`, and `DemandStatus` a three-state lifecycle
`Raised → Satisfied | Acknowledged`, each transition stamped `{as_of,
authority}`.

That is a well-shaped skeleton with **no payload and no verdict**. A Demand
today can say *that* a person is needed, by whom, and about what work — but
not *what is being asked*, not *what answer came back*, and not *by when*.
`ConvoyAttention {source, reason, raised_at}` (`convoy.rs:519-523`) carries a
free-text `reason` and is used by `refuse_turn_delivery` to record a hold, so
the vocabulary for "we stopped and a person is why" exists; it is prose.

The related-work note is that HumanLayer's whole model is exactly the payload
and verdict half, and it is the half Flotilla skipped. Their model is missing
exactly the half Flotilla built. Neither of them has expiry.

---

## 5. Adoptable ideas for Flotilla

Ordered by confidence. All are patterns to copy; **nothing here is a service
to depend on** — the only integration target their design ever had is now
NXDOMAIN, and their current product is a closed desktop app.

### 5.1 Give `Demand` a payload and a typed verdict (high confidence)

The single highest-value lift, and it is a data-shape change — which is
exactly the kind ADR-style guidance says to make now rather than later,
because retrofitting it means changing what is on disk and on the wire.

Adopt the gen-1 spec/status split, in Flotilla vocabulary:

- **Spec gains an ask.** A question or proposition, optional context, and a
  render hint in the shape of factor 7's `format` (`free_text | yes_no |
  choice`). The surface should never parse prose to decide whether to draw
  buttons.
- **Spec gains enumerated response options.** `ResponseOption {name, title,
  description, prompt_fill}` is the right shape. `prompt_fill` matters most:
  it makes a human's answer *machine-actionable text handed back to the crew*
  rather than something the agent has to interpret. This is what turns "the
  human said no" into a re-plannable instruction, which is the same move
  factor 8's reject-as-tool-response makes.
- **Status gains a verdict**, three-valued by construction — unanswered is a
  distinct state, not a missing field — carrying the responding principal, the
  chosen option name, and free-text comment. Keep their asymmetry rule:
  approval may be silent, refusal must carry a reason. Enforce it in the type.

This slots cleanly beside the existing `DemandState` lifecycle: `Raised →
Satisfied` already models the transition; the verdict is what `Satisfied`
should have been carrying all along. It also fixes the ACP failure mode
directly — no verdict should ever have to be reconstructed by string-parsing a
`reason`, which is precisely what `ConvoyAttention.reason` invites today.

### 5.2 Put a deadline on a Demand (high confidence)

**Every one of HumanLayer's three generations lacks this, and each one is
worse for it.** An ACP `ToolCall` in `AwaitingHumanApproval` waits forever,
holding its parent `Task` and renewing a distributed lease indefinitely. The
12-factor essays never mention a deadline. The A2H draft lists "prioritized
contact channels" and specifies no failover between them.

Flotilla has already accepted the shape of the answer on the Regard side —
`RegardExpiryPolicy::Decaying { expires_after_seconds } | Pin`. Demands should
have the mirror: a deadline or expiry policy in the spec, and a corresponding
terminal state (expired/abandoned) distinct from satisfied. Otherwise
"unattended convoy blocked on a person who went to bed" is indistinguishable
from "convoy working", and the fleet slowly fills with legs that will never
resume.

Given ADR 0018 already routes demands by joining against regards and escalates
out-of-searchlight ones, expiry is the missing input to that policy: escalate
*after* a deadline, not merely because the addressee wasn't looking.

### 5.3 An agent-side capture seam: the deliberate ask (high confidence)

Today Flotilla's attention plane is entirely **observational** — hooks and
cleat noticing infer `NeedsInput` from what the harness does (ADR 0017). That
is the right primary source and should stay. But a crew that wants to ask a
question mid-brief has no way to *state* one; the brief's call-for-human phase
is a prompt convention, exactly like RPI's, and it resolves by the human
noticing the terminal.

`hld`'s design is the proven local pattern, and Flotilla already owns every
piece of it: the daemon exposes a tool (MCP or otherwise), the crew calls it,
the daemon raises a Demand, the surface renders it, the answer resolves the
Demand and unblocks the call. Their handler blocks on a channel woken by an
event-bus subscriber; Flotilla's equivalent would resolve against the resource
store and its watch stream, which is strictly better than `hld`'s in-process
`sync.Map` because it survives a daemon restart and works across hosts.

Two details from `hld` worth keeping: the request carries the tool name *and
its input* (so the surface can show what would happen), and there is an
`MCP_AUTO_DENY_ALL`-style escape hatch for testing the blocked path without a
human.

The important framing: this is **not** a second attention mechanism. It is a
second *source* for the same `Demand` resource — deliberate request alongside
observed prompt — the same way ADR 0017 already runs hook-primary with
screen-fallback for `TerminalAttention`.

### 5.4 Verb-scoped gates (medium confidence — do it properly or not at all)

HumanLayer wanted per-tool approval granularity and never got past
per-MCP-server all-or-nothing; their own issue #104 is still open and the repo
is dormant. So there is no prior art to copy here, only a warning about the
shape that fails.

For Flotilla the natural home is the declared state machines of ADR 0024: a
gate is a *predicate on an admitted transition*, not a wrapper around a tool
call. "Landing requires a human verdict" belongs next to the transition that
performs it, evaluated by the same authority that admits it. That keeps gates
federated correctly (ADR 0025) and avoids ACP's mistake of scattering approval
policy across per-server config where it cannot see the verb it is gating.

This should wait until 5.1 exists — a gate is worthless until a Demand can
carry a verdict.

### 5.5 Channel adapters — worth the pattern, not the priority (low confidence, deliberately)

Their contact-channel union (Slack/email/SMS/WhatsApp with threading,
`allowed_responder_ids`, RFC-822 reply headers) is competent, and the A2H
separation principle is genuinely good: **the agent addresses a person, never
an address; the route is administrative data the agent cannot see.** Flotilla
should keep that principle in mind if it ever grows adapters, because
`DemandAddressee::Principal { principal_ref }` already has the right shape and
would be easy to corrupt by hanging a Slack ID off it.

But this is where their model is weakest relative to Flotilla's. Their channel
is on the *request* — the agent picks how to reach you. Flotilla's ADR 0018
routes by joining against the principal's live regards, which is a better
answer, and the near-term surfaces (TUI, wheelhouse, attach) are already the
places a Flotilla principal looks. An out-of-band adapter is worth building
when demands need to reach someone who is not at a surface — i.e. after 5.2
gives expiry something to escalate *on*. Note also that `effect-channels`, the
repo whose name promises exactly this abstraction, is an empty stub; nobody
has solved it in public.

### 5.6 What to explicitly not copy

- **A pending human request as a phase plus an opaque foreign key.** ACP's
  central mistake; the damage is quoted in §2.2. Flotilla already avoids it by
  having `Demand` at all — the point is not to erode that by leaving the
  payload and verdict outside the resource, which is the state it is in today.
- **Vendor tenancy in resource status.** ACP put `projectSlug`/`orgSlug` in
  `ContactChannelStatus`. If a Flotilla resource ever fronts an external
  service, provider identity belongs in a sub-object or annotation.
- **Poll-everything control loops.** ACP polls every 5s with label selectors,
  no watches. Flotilla's aggregator is already ahead.
- **Prompt-convention gates presented as guarantees.** RPI's "pause and inform
  the human" and Flotilla's call-for-human phase are the same mechanism, and
  it is honest only as long as nobody claims it blocks.

---

## Sources

Primary, all fetched or verified 2026-08-21.

**Code (via authenticated GitHub API):**
`humanlayer/humanlayer` — `README.md` (commit `99abe673`), `LICENSE`,
`hld/approval/types.go`, `hld/store/store.go`, `hld/mcp/server.go`,
`hld/PROTOCOL.md`, and `humanlayer/core/{models.py,approval.py}` at ref
`760e769f`.
`humanlayer/agentcontrolplane` at `main` (`eaa2a7ed`) —
`LICENSE`, `api/v1alpha1/{contactchannel,toolcall,task,mcpserver}_types.go`,
`internal/controller/toolcall/state_machine.go`,
`internal/controller/contactchannel/state_machine.go`,
`internal/humanlayer/{client.go,hlclient.go}`,
`internal/humanlayerapi/api/openapi.yaml`, `CHANGELOG.md`.
`humanlayer/12-factor-agents` — `README.md`, `content/factor-0{3,5,6,7,8,9}-*.md`,
`content/factor-1{1,2}-*.md`, `drafts/a2h-spec.md`, `workshops/2025-05-17/`.
`humanlayer/{fold,skills,effect-channels,mcp-cli,ampere,effect-durable-streams,claudelayer,riptide-rpi,rpi-coordination-template,advanced-context-engineering-for-coding-agents}`
— READMEs, LICENSE presence, trees.
`humanlayer/homebrew-humanlayer` — `Casks/humanlayer.rb`, releases.

**Web:** [humanlayer.com](https://www.humanlayer.com/) (incl. `#pricing`,
`#faq-oss`), [humanlayer.dev](https://www.humanlayer.dev/),
[docs.humanlayer.com](https://docs.humanlayer.com/) (release notes,
remote-daemons, beta-to-prod migration), the
[blog](https://www.humanlayer.dev/blog),
[YC company page](https://www.ycombinator.com/companies/humanlayer),
[YC launch post](https://www.ycombinator.com/launches/M8e-humanlayer-human-in-the-loop-for-ai-agents-and-beyond).
DNS/TLS state of `api.humanlayer.dev`, `docs.humanlayer.dev`,
`app.humanlayer.dev` checked directly.

**Flotilla:** `docs/adr/0017-convoy-completion-claims-conditions-attention.md`,
`docs/adr/0018-presentation-attention-demands-regards-projection.md`,
`crates/flotilla-resources/src/principal_attention.rs`,
`crates/flotilla-resources/src/convoy.rs`,
`crates/flotilla-resources/src/terminal_session.rs`.
