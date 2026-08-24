# DeepSeek Harness (`dsh`) as a Candidate Crew Adapter

**Date:** 2026-08-21
**Context:** Robert flagged "the DeepSeek harness" as interesting without a
link. This document identifies the artefact, records its architecture from
primary sources, and assesses it against Flotilla's `AgentAdapter` contract.
It records findings and constraints; it does not propose an implementation.

## Executive summary

The artefact is firmly identified: **DeepSeek Harness (`dsh`)**, at
[github.com/deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness),
MIT-licensed, a TypeScript/Node monorepo published to npm as
`@deepseek-ai/dsh`. It is not an SWE-agent-style eval scaffold and not a
terminal agent; it is a **plugin-composed agent runtime** whose organising
claim is that every capability — including the agent loop itself — is a
swappable Cordis plugin.

Three findings dominate the adapter question.

**It has no terminal UI.** Only two profiles ship: `web` (a local HTTP server
plus browser UI) and `headless` (a one-shot runner that answers a single task
and exits). There is no REPL, no TUI, no `ink` or equivalent dependency
anywhere in the tree. Flotilla's entire crew model — a resident interactive
CLI hosted in a cleat PTY, re-prompted by typing into it — has nothing to
attach to. This is a structural mismatch, not a missing flag.

**The parts Flotilla finds hardest elsewhere are unusually clean here.**
Unattended operation is one environment variable, `DSH_PERMISSION_MODE=danger-full-access`,
which simultaneously disables filesystem confinement and sets the approval
policy to non-prompting. There is no onboarding gate, no per-project trust
dialog, and no consent state to pre-seed — so none of the `.claude.json` /
`config.toml` seeding machinery that Claude Code and Codex require has an
analogue. Config, credentials, sessions, and profiles all live under a single
relocatable root, `$DSH_HOME`, which directly answers the per-crew config-home
isolation gap recorded in
[2026-07-28-multi-crew-agent-config-seeding.md](2026-07-28-multi-crew-agent-config-seeding.md).
API keys inject as ordinary environment variables at the highest precedence
layer, with no OS keychain involved.

**The machine-driving surfaces are real but not reachable from the `dsh`
binary.** Two stdio JSON-RPC surfaces exist — an SDK protocol and an Agent
Client Protocol (ACP) bridge — and both support multi-turn prompting; the SDK
one also supports session resume. Neither is packaged as a bundle, so neither
can be selected by a `dsh --profile` invocation today. Reaching them means
authoring a custom profile or embedding the SDK.

The net assessment: `dsh` is **not adaptable to Flotilla's current
PTY-resident adapter shape**, and would not become so through configuration.
It is, however, the best-fitting candidate yet seen for the *contract*
Flotilla's ADR 0010 describes but has not implemented — adapter-native
`resume` and `re_prompt` verbs over a durable session log. Separately, several
of its subsystems are worth reading regardless of adoption, above all its
three-tier context ladder and its treatment of compaction as shadowing rather
than deleting.

## Research method and confidence

Findings come from the repository itself, cloned at commit
`b150a551b8d465e31e418e1b2eaf5e79bbb7d28e` (tag `dsh-v0.1.1-rc.2`, published
2026-08-21), read directly rather than summarised from coverage. Repository
and release metadata come from the GitHub API; package metadata from the npm
registry. Citations below are `path:line` within that checkout unless
otherwise marked.

Confidence is high on everything sourced to code and package documentation.
Two areas are weaker and flagged as such in the final section: runtime
behaviour (nothing here was executed) and anything about evaluation quality,
where the project publishes no numbers at all.

Note that the clone was shallow; the earliest commit visible in it
(2026-07-27) is the depth boundary, not the start of the project's history.

## 1. What it is

### Provenance and maturity

The GitHub repository was created **2026-08-13T11:56:32Z** and carries commit
history predating that date, so the public release followed a period of
internal development. The npm package `@deepseek-ai/dsh` was first published
2026-08-10. Adoption has been extreme: 179,234 stars and 19,515 forks as of
2026-08-21, eight days after publication.

Development is fast and ongoing — four prereleases in five days
(`dsh-v0.1.0-rc.7` on 08-17, `rc.8` on 08-19, `dsh-v0.1.1-rc.1` and `rc.2`
both on 08-21), with the most recent push the same day as this research.

Maturity is stated bluntly in `README.md:11-13`:

> DeepSeek Harness is currently in _developer preview_ and is iterating
> rapidly. **THERE WILL BE COMPATIBILITY-BREAKING CHANGES.**

License is MIT (`LICENSE:1`, "Copyright (c) 2026 DeepSeek"), confirmed as
`MIT` by the GitHub API. Runtime requirement is Node `^22.19.0 || >=24.0.0`
(`package.json:11`); the toolchain is pnpm workspaces, TypeScript 6, oxlint,
and vitest.

Scale: roughly 45 package groups under `packages/`, two apps, 219 markdown
docs. Every documentation page exists as an English/Chinese/translation-brief
triple, and a large generated-catalogue apparatus (tool catalogue, config
catalogue, persistence catalogue, module graph) is verified in CI.

### What it drives

Default model selection is DeepSeek's own: provider `deepseek-official`, model
`deepseek-v4-flash` (`packages/bundle/base/cordis.patch.yml:63-67`). The
shipped catalogue is `deepseek-v4-flash`, `deepseek-v4-pro`, and a vision
variant, with a 1,000,000-token default context window
(`packages/llm/llm-deepseek/src/index.ts:83-94`).

**It is not DeepSeek-only.** Two adapters sit behind one provider-neutral
seam. `llm-deepseek` is a direct-fetch adapter for DeepSeek's API;
`llm-pi-ai` is a generic multi-provider adapter, mounted *dormant* in the
default composition and activated by user settings
(`packages/bundle/base/cordis.patch.yml:88-96`). It speaks three wire
protocols (`packages/llm/llm-pi-ai/src/provider.ts:47-51`):

```ts
const PROTOCOLS: Readonly<Record<string, () => ProviderStreams>> = {
  'openai-completions': openAICompletionsApi,
  'openai-responses': openAIResponsesApi,
  'anthropic-messages': anthropicMessagesApi,
}
```

Anthropic and OpenAI are catalogue providers reachable by entering a key;
arbitrary gateways are declarable by hand with `baseURL`, `api`, and a model
list (`docs/user/guide/providers.md:23,37-48`). Bedrock, Vertex, Azure, and
Codex-OAuth are modelled as native-auth routes (`providers.md:19`).

The adapter contract is a documented extension point
(`docs/user/develop/practice/llm-adapter.md`), with exactly one required
method — `stream()` returning an async iterable of chunks
(`packages/llm/llm/src/index.ts:259`).

Nothing hard-codes DeepSeek's endpoint as a floor: `PUBLIC_BASE_URL` is a
fallback behind config and `$DEEPSEEK_BASE_URL`
(`packages/llm/llm-deepseek/src/index.ts:182,358-361`). Residual couplings are
soft — the default model pick, DeepSeek-shaped `reasoning_content` passback,
a DeepSeek Files API used for image attachments
(`packages/llm/llm-deepseek/src/files-api.ts`), and attribution headers that
would follow a repointed `baseURL` to a third-party host
(`packages/llm/llm-deepseek/src/adapter.ts:525-530`). The one capability that
genuinely wants a DeepSeek key by default is **web search**, mounted as
`web-search-deepseek` against a separate endpoint, with Exa and Perplexity
alternatives shipped.

## 2. Architecture

### Cordis: the micro-kernel

`dsh` is built on [Cordis](https://github.com/cordiverse/cordis), vendored
into the tree. The README cites a paper, *A Programming Paradigm for
Spatiotemporal Composability*, for the design; the phrase is not defined
anywhere in this repository, so treat it as an external citation rather than a
claim the code explains.

Operationally, Cordis gives four things (`docs/cordis-primer.md:9-13`): a
plugin is an object with `inject` and `apply(ctx)`; a context is a repository
of services addressed by key (`ctx.tools`, `ctx.llm`, `ctx.sessions`) rather
than by import; dependencies are declared as service names so load order is
derived, not sequenced; and **every registration is a reversible effect**, so
a plugin whose dependency disappears unloads and reloads cleanly.

The load-bearing dispatch mode is `waterfall`, described as around-middleware:
a listener receives `(...args, next)` and either delegates or short-circuits
(`docs/cordis-primer.md:30`). Interception, permission policy, compaction,
hook bridges, and sandboxing all hang off this one mechanism.

The composition claim is literal (`docs/architecture.md:11-13`):

> Every part of the product is a plugin, including the model adapter, the tool
> registry, the session log, and the agent loop itself… **There is no
> privileged core to patch**.

Composition is layered: bundles → profile patch → home-level patch → `--patch`
overlays, later layers winning per row, with `--dump-config` printing the
resolved tree.

### The agent loop

The loop lives in its own package (`packages/core/agent-loop`) behind
`ctx.agentLoop`, deliberately separated from the `Agent` interface so it stays
swappable (`docs/subsystems/core.md:20`). The driver is `ReactLoopAgent`
(`packages/core/agent-loop/src/agent.ts:64`).

Vocabulary (`docs/architecture.md:65`): a **step** is one model request plus
the tools it calls; a **turn** is zero or more steps, opening before its first
input is claimed and closing once nothing is owed. The canonical sequence is
`turn/start` → per-step (`agent/pre-step` → `step/start` → derive history →
`agent/request` → stream → `assistant/message` → tool pipeline → `step/end`) →
`agent/turn-stopping` → `turn/end`.

Two design choices stand out.

**There is no explicit re-prompt.** Tool results are appended to the session
log as events, and the next step re-derives the entire model history from that
log (`agent.ts:341`). The message array is a projection, never stored state.

**`Agent` is not a `run(prompt)` function.** It exposes a durable two-list
inbox (`agent.ts:113-132`): `followup()` queues a new turn and wakes the
agent; `steer()` interrupts at the next step boundary; and `inject()` adds
context **without** waking it. That last primitive — queue context that will
be seen whenever the agent next runs, without causing it to run — is uncommon,
and is how skills, background job completions, and subagent settlements land
in-band.

The governing invariant is asserted at runtime (`docs/architecture.md:96`):
"**Model-visible means logged.** Anything that reaches a model request must be
reconstructable from the log."

Tool dispatch is a concurrency scheduler rather than a `Promise.all`
(`packages/core/agent-loop/src/tool-calls.ts`): exclusive calls form barriers,
parallel calls run in a bounded rolling pool, classification is re-read per
call so a mid-batch registry change can create a barrier, and results commit
strictly in model order even though dispatch overlaps.

### Tools

The built-in set is broad and conventional at its core — `read`, `write`,
`edit`, `read_image`, `glob`, `grep` (via packaged ripgrep, no host `rg` and
no shell), `bash`/`pwsh` in one-shot and PTY-persistent forms, six `terminal_*`
tools, `web_search`, `web_fetch`, `lsp`, `todo_write`, job control, and
`exit_plan_mode`.

Less conventional entries: `session_search` / `session_trace` and siblings let
the model full-text search and trace **its own and other sessions' event
logs**; and an opt-in, not-shipped `dsh-tool-cordis` set lets the model
define, run, and inspect live Cordis plugins inside the running harness,
registering additional model-visible tools at runtime.

Schemas are declared through a typed DSL with compile-time inference rather
than hand-written JSON Schema, and a tool's `output` type is **required**
(`docs/subsystems/tools.md:145,151`) — which is what makes the Code Mode typed
SDK possible.

The generated tool catalogue (`docs/tool-catalog.md`, 2221 lines) is unusual
in that its generator **boots each tool plugin on a real context** and reads
back the live schemas, because schemas are not statically knowable; a
completeness guard fails CI if any `packages/*/tool-*` package is missing from
the boot manifest.

### Sandboxing

Isolation vocabulary is **filesystem effects only**, three modes:
`read-only`, `workspace-write`, `danger-full-access`
(`docs/subsystems/sandbox.md:11,20`). Backends are selected per platform
(`packages/sandbox/sandbox-local/src/index.ts:159-166`): `bwrap` then
`landlock` on Linux, `seatbelt` (`sandbox-exec`) on macOS, a restricted-token
ACL runner on Windows. `native/landlock-run` is a genuine ~300-line C11
binary, statically linked against musl, distributed as prebuilt per-platform
npm packages; it self-restricts then `exec`s, and exits without running the
command if the kernel cannot enforce.

The seam is fail-closed: `confine()` either returns a confined argv or throws,
and "silent unconfined passthrough is never legal for a confined policy"
(`docs/subsystems/sandbox.md:154`).

**Network egress cannot be restricted.** This is stated in four separate
package READMEs, and the bwrap profile notably omits `--unshare-net`. For
egress control the documented answers are a custom runner command, a
container, or the experimental E2B remote-sandbox composition
(`packages/e2b`), which swaps `ctx.fs` and `ctx.subprocess` for a remote
execution world.

### Context management — the strongest part

Three cooperating subsystems form a ladder, and they are the most
architecturally interesting thing in the repository.

**Spill** (`docs/subsystems/spill.md`) keeps oversized tool results out of
context in the first place. An over-threshold plain-text result is replaced
with a head/tail preview plus a `SpillRef`. The reference is a **branded
opaque locator, not a path** — the local backend renders it as a filesystem
path, but a remote backend could render a URI or command token, and the model
is told how to retrieve it via a per-backend `retrievalHint` rather than
assuming `read` works. The policy is best-effort: a save failure keeps the
original inline result rather than turning a successful call into an error.

**Model-free pruning** runs before any summarisation. The compactor
deterministically truncates oversized tool results, **remeasures, and skips
summarisation entirely if pressure became safe**
(`packages/compaction/compaction-basic/README.md:13-24`). Most harnesses jump
straight to an LLM call.

**Cache-warm summarisation** is the last rung, and the trick is genuinely
clever (`compaction-basic/README.md:18`):

> The call replays the conversation's own system prompt, tools, and
> shadowed-region messages verbatim, including image references, and appends
> the compaction instruction as the final user message, so it reuses the
> provider's warm prefix cache instead of invalidating it.

Rather than constructing a fresh "summarise this transcript" prompt against a
cold cache, it re-issues the actual conversation with a trailing instruction,
so everything up to that instruction is cache-hit.

Underneath all three: **compaction deletes nothing.** The durable log is
append-only and complete; what the model sees is a derived "surface", and
compaction shadows surface nodes while the originals stay on disk
(`docs/subsystems/compaction.md:11`). Replay, fork, and audit stay intact.
Compaction is itself event-sourced with a durable lock whose release-last
ordering makes a mid-operation crash detectable as an orphaned lock rather
than a false claim of completion (`compaction.md:19`).

Measurement anchors on real provider usage numbers when the request envelope
is unchanged, falling back to heuristics only when it cannot
(`docs/subsystems/token-meter.md:29`).

### Novel subsystems

**Code Mode** (`docs/subsystems/code-runtime.md`) lets the model write a
TypeScript or Python program that calls tools as function bindings, instead of
emitting tool-call blocks, via a reserved `run_code` transport. What makes
this implementation stronger than most attempts: sub-calls **re-enter the full
guarded pipeline**, so permissions, sandbox, approval, and hooks still apply
inside the program; denials surface as catchable typed exceptions the program
can adapt to; and the failure taxonomy keeps `exception`, `timeout`, `abort`,
`worker-exit`, `invalid-output`, and `output-limit` orthogonal rather than
collapsed. It is also honest that its `isolation` label is "a diagnostic
label, **not a security claim**".

**Workflow** applies the same idea to orchestration: the model writes a script
whose globals are `agent()`, `parallel()`, and `pipeline()`. The failure
discipline is notable — combinators re-throw fatal errors rather than mapping
the item to `null`, on the stated grounds that a typo'd option "must kill the
script loudly, never dissolve into something that reads as an ordinary child
failure" (`docs/subsystems/workflow.md:116`).

**`ralph`** ships the "same prompt, fresh agent, repeat" technique as a
first-class tool: each round gets only the immutable objective and the
previous structured handoff, with the workspace as the only long-term memory.

**Goal** is an event-sourced objective with compare-and-set revisions, and —
unusually — enforced authority: creating, editing, pausing, and resuming
require direct-human root authority, so a model cannot silently rewrite its
own objective.

**Subagents** follow the LLM-adapter pattern, with multiple named providers
coexisting: in-process spawn, fork, ACP, **Codex**, and **Claude Code**. So
`dsh` can delegate a subtask to a competitor's CLI behind one interface.

**Hook bridges** run an existing Claude Code `hooks.json` or a Codex hook
config faithfully against the harness's own typed interception points
(`packages/hooks/README.md`).

## 3. Fit as a Flotilla crew adapter

Flotilla's crew contract is `AgentAdapter`
(`crates/flotilla-core/src/agent_adapter.rs:461-486`). Its shape is a resident
interactive CLI: `launch()` returns a command that the terminal pool runs
inside cleat; the brief is a file at `.flotilla/briefs/<role>.md` with argv
carrying only a pointer to it; completion is signalled by the agent running
`flotilla crew complete` as its final act; and re-prompting is
`cleat send --submit` into the live PTY.

### Where `dsh` fits well

**Unattended operation is one variable, with no consent state to seed.**
`packages/bundle/base/cordis.patch.yml:175,191`:

```yaml
mode: !!js process.env.DSH_PERMISSION_MODE ?? 'workspace-write'
policy: !!js "(process.env.DSH_PERMISSION_MODE ?? 'workspace-write') === 'danger-full-access' ? 'never' : 'ask'"
```

Setting `DSH_PERMISSION_MODE=danger-full-access` disables confinement and sets
the approval policy to `never`. Note the precise semantics: `never` means
**auto-reject**, not auto-approve (`user-approval/src/index.ts:312`) — it is
safe because under `danger-full-access` nothing needs to ask. This replaces
the whole apparatus Flotilla currently needs per adapter: no
`hasCompletedOnboarding`, no `hasTrustDialogAccepted`, no
`projects.<cwd>.trust_level`, no settings overlay. The repository's own e2e
tests use exactly this variable for unattended runs.

**Config home is a single relocatable root.** `DSH_HOME` (default `~/.dsh`)
covers settings, credentials, sessions, attachments, profiles, and telemetry
identity (`packages/util/home-paths/src/index.ts:18,87-91`). This is cleaner
than either existing adapter and directly answers the credential-vs-config-home
conflation flagged as design debt in the 2026-07-28 seeding research.

**Credentials inject headlessly.** The precedence chain puts the inherited
process environment first, explicitly so that a container `-e` or CI secret
wins (`packages/credentials/credentials-local/src/index.ts:5-10`). No OS
keychain exists anywhere in the tree — the README says so and calls a keychain
provider "the deferred answer". One caveat: the environment snapshot is frozen
at launch, so a variable exported after startup is invisible.

**The brief-pointer pattern transfers unchanged.** `dsh` has file-read and
bash tools, so "read your brief at X and follow it" works as-is, and the brief
can instruct the agent to run `flotilla crew complete` as its final act.
`AGENTS.md` and `CLAUDE.md` are both discovered per-project, walking from
project root to session cwd, plus a global `$DSH_HOME/AGENTS.md`
(`packages/context/agent-instructions/src/config.ts:11-14`).

**Some Flotilla hook wiring may transfer.** The Claude Code bridge maps
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`
onto native interception points. Flotilla emits SessionStart, SessionEnd,
UserPromptSubmit, Stop, and Notification — so three of five map, but
**`SessionEnd` and `Notification` are not in the supported subset**, and those
are the two most relevant to observing a crew.

### Where it does not fit

**There is no interactive process to host.** Only `web` and `headless`
profiles ship (`packages/boot/app-boot/src/profile.ts:113-117`); the `tui`
profile appearing in help text is an illustrative example of argv passthrough
for a profile that does not exist. No TUI dependency (`ink`, `blessed`,
`enquirer`, and similar) appears in any `package.json`. Flotilla's model of a
resident agent in a PTY, steered by typing, has no counterpart.

**The headless profile is one-shot and cannot be re-prompted or resumed.** It
takes the task as a positional argument, waits for quiescence, prints the last
non-empty assistant message to stdout, and exits
(`packages/bundle/headless/src/index.ts:118-133`). Its README lists "**One
submitted task only** — the runner has no interactive follow-up surface" as a
known limitation, and the `dsh` CLI defines no `--resume` flag anywhere. There
is no JSON output mode; the repository's JSONL event stream is explicitly
labelled "test infrastructure, not a supported CLI output format".

**Completion signalling is coarser than it looks.** Headless exits 0 only when
the final `turn/end` reason is `completed`; `max-tokens`, `blocked`,
`aborted`, and `interrupted` all exit 1 indistinguishably except by parsing
stderr. Flotilla does not use exit codes for completion anyway, so this
matters less than it would elsewhere — but it means the process boundary
carries one bit.

**The good machine surfaces are not reachable from `dsh`.** The SDK JSON-RPC
server (`packages/sdk/server`, stdio NDJSON, with `session.status` idle/running
notifications and full event streaming) and the ACP bridge
(`packages/acp/acp`, stdio JSON-RPC, blocking `session/prompt` returning a
`stopReason`) are both built for machines and both support multi-turn work.
Neither package declares a bundle, so neither can be selected by
`dsh --profile`; today they run only via hand-written demo binaries over
hand-authored `cordis.yml` files. The ACP bridge additionally documents
"**Fresh sessions only** — load, list, resume, delete, and fork are
unsupported" and emits committed messages rather than token deltas.

**Session resume exists but only as an API.** `ctx.agents.resume()` is a real
primitive, sessions persist as append-only JSONL under `$DSH_HOME/sessions`
with crash repair, and the web/host API resumes cold sessions. But the format
is pinned at `SESSION_FORMAT_VERSION = 0` with, in its own words, "no
compatibility implied" and no migration path; only one process may write a
given session log; and nothing ever deletes logs.

**No auth preflight is obvious.** ADR 0022 requires a bounded preflight so a
missing credential fails admission rather than looking like a stuck crew. The
cheapest `dsh` equivalent — a trivial headless run — costs a model call and a
container round-trip. There is no `dsh auth status` analogue to
`codex login status` or `claude -p ok`.

### Assessment

Adopting `dsh` as a crew adapter is not a configuration exercise. Three
shapes are possible, in increasing order of both cost and value:

1. **One-shot headless per turn.** Fits the brief-and-complete pattern
   directly and needs no new adapter verbs. Loses in-session steering
   entirely, and without a resume flag every re-prompt starts a fresh session,
   discarding accumulated context. Cheap to try, weak as a crew.
2. **A resident driver over the SDK JSON-RPC surface.** This is the shape the
   harness actually wants, and it supplies exactly the `resume` and
   `re_prompt` verbs that ADR 0010 froze into the design and that no adapter
   implements. It requires authoring a custom profile that mounts the SDK
   server, and it makes Flotilla speak a protocol rather than drive a PTY.
3. **ACP.** Standards-shaped and would generalise beyond `dsh`, but the
   bridge's fresh-sessions-only limitation removes the main reason to prefer
   shape 2.

Shape 2 is the only one that produces a crew comparable to the existing two.
It is also a larger change than "add an `AdapterFlavor` variant", because it
crosses the seam from process-and-PTY to protocol — which is precisely the
seam ADR 0010 anticipated. That makes `dsh` valuable as a **forcing case for
the resident-driver adapter shape** even if it is never adopted.

Two smaller blockers apply to any shape: the crew image must ship Node 22.19+
and the `dsh` binary, and a host detector must probe it or the adapter
silently never registers — the exact bug that previously hit Codex.

## 4. Adoptable ideas for Flotilla

Independent of whether `dsh` ever becomes a crew adapter, several ideas are
worth lifting.

**Shadow, don't delete, when compacting.** The split between a complete
append-only log and a derived model-visible surface, with compaction shadowing
surface nodes rather than rewriting history, is strictly better than
destructive transcript rewriting. Flotilla's own annotation-layer and
materialised-view work (see
[2026-07-26-annotation-layer-and-materialized-views.md](2026-07-26-annotation-layer-and-materialized-views.md))
is reaching for the same distinction; this is a worked example with the
crash-safety details filled in.

**Release the lock last so crashes are detectable.** Bracketing an operation
with start/end events and releasing the durable lock after the end event turns
a mid-operation crash into a detectable orphaned lock rather than a false
completion claim. This generalises directly to convoy and vessel lifecycle
operations.

**The three-tier context ladder as a design pattern.** Keep the big thing out
of context (spill) → prune deterministically and remeasure before spending a
model call → only then summarise, and do it cache-warm. The middle rung —
remeasure and skip the LLM call if pruning was enough — is the cheap win most
implementations omit.

**Opaque locators with retrieval hints.** `SpillRef` carries a branded opaque
handle plus a per-backend hint telling the model how to retrieve it, rather
than a path the consumer must assume is readable. Flotilla has the same
problem wherever a resource reference crosses a host or container boundary and
the retrieval mechanism differs by placement.

**Boot the plugins to generate the catalogue.** The tool-catalogue generator
boots each plugin on a real context and reads back live schemas, with a
completeness guard that fails CI when a package is missing from the manifest.
This is a stronger pattern than static extraction for anything whose true
shape is only known at runtime — and Flotilla has several such surfaces
(binding tables, provider descriptors, command catalogues).

**Non-waking context injection.** The `inject()` primitive — queue context the
agent will see when it next runs, without causing it to run — is a cleaner
primitive than either "send now" or "store for later" for delivering
out-of-band facts to a crew. Flotilla's re-prompt path today is
`cleat send --submit`, which always wakes.

**Authority on objectives.** Requiring human root authority to create or edit
a goal, while letting the agent report completion or blockage, is a
distinction Flotilla's convoy and settlement-claim vocabulary would benefit
from making explicit.

**Fail loud on orchestration typos.** The workflow engine's rule that a bad
option kills the script rather than degrading into a normal-looking child
failure is the right default for convoy orchestration too.

**Environment-first credential precedence, stated as intent.** The credential
store puts the inherited environment above its own managed file specifically
because a container `-e` or CI secret is operator intent for that run, and it
reports that layer as read-only rather than silently ignoring writes. That
framing is worth copying into Flotilla's credential store documentation.

## 5. Unverified items to preserve as explicit unknowns

- **Nothing was executed.** All findings are from source and package
  documentation. Runtime behaviour, actual sandbox enforcement, and whether
  the headless profile behaves as documented in a container are untested.
- **No published evaluation exists.** `BENCHMARK.md` is three lines pointing
  at the Python SDK and telling the reader to run their own benchmarks. No
  SWE-bench, Terminal-Bench, or any score appears anywhere in the repository.
  For a harness this architecturally ambitious, that is the notable gap, and
  it means agent *quality* here is entirely unassessed.
- **Model quality is out of scope.** `deepseek-v4-flash` and `-pro` were not
  evaluated, and their capability relative to the models Flotilla's crews use
  is unknown.
- **The `dsh`-as-Flotilla-crew path was not prototyped.** Whether a headless
  run can successfully call back to `flotilla crew complete` over
  `FLOTILLA_DAEMON_SOCKET` from inside a vessel is plausible but unverified.
- **Whether a custom profile can mount the SDK server cleanly** is inferred
  from the bundle/profile mechanism, not demonstrated.
- **Stability risk is high and explicit.** Developer preview, `0.1.1-rc.2`,
  session format v0 with no compatibility promise, four prereleases in five
  days, and a repository README promising breaking changes. Any integration
  built now should expect churn.
- **The Cordis paper was not read**, so the "spatiotemporal composability"
  framing is reported as a citation rather than assessed.
