# Agent Infrastructure Landscape Refresh

**Date:** 2026-08-21
**Context:** A general "look around" at the projects adjacent to Flotilla — sandbox
substrates, agent orchestrators, hosted vendor platforms, terminal/session
infrastructure, and fleet-deployment patterns — refreshed against primary
sources. This is the periodic survey, not a question-driven investigation.

**Method and confidence.** Every factual claim below was fetched from a primary
source (project repository, official documentation, official changelog, or the
GitHub API) on 2026-08-21, and carries an inline link. Six parallel research
passes produced the raw material; overlapping claims were cross-checked where
the passes intersected. Where a source could not confirm something, this
document says "could not verify" rather than guessing. Star counts and release
dates are the least reliable numbers here — treat them as order-of-magnitude
signals, and re-check any figure a decision would turn on. The
[non-verifications section](#12-explicit-non-verifications) at the end is not a
formality: several load-bearing claims about the multi-host tier rest on a
single fetch each.

**Relationship to prior surveys.** This does not re-tread
[`2026-03-19-vt-engine-landscape-survey.md`](2026-03-19-vt-engine-landscape-survey.md)
(VT engines and reattach rendering),
[`2026-07-26-software-factories-and-flotilla.md`](2026-07-26-software-factories-and-flotilla.md)
(the harness-engineering argument),
[`2026-07-27-agent-harness-credential-patterns.md`](2026-07-27-agent-harness-credential-patterns.md)
(per-harness credential file formats), or
[`2026-07-29-state-transition-verification-prior-art.md`](2026-07-29-state-transition-verification-prior-art.md)
(k8s conditions, `observedGeneration`, deterministic simulation). Where those
docs already answer a question, this one says so and moves on.

---

## 1. Executive summary

Five findings, in descending order of how much they should change what Flotilla
does.

**1. The "daemon per machine plus aggregating client" architecture is now table
stakes, not a differentiator.** Six months ago it was defensible ground. Today
at least seven independent projects have shipped it —
[Omnara](https://github.com/omnara-ai/omnara),
[Multica](https://github.com/multica-ai/multica),
[AgentsMesh](https://github.com/AgentsMesh/AgentsMesh),
[Paseo](https://github.com/getpaseo/paseo),
[ai-maestro](https://github.com/23blocks-OS/ai-maestro),
[Comet](https://github.com/zeronsh/comet),
[Centaur](https://github.com/paradigmxyz/centaur) — alongside
[OpenHands Agent Canvas](https://docs.openhands.dev/openhands/usage/agent-canvas/backends),
which registers N agent servers by URL and API key. What none of them does is
**capability-aware placement across heterogeneous hardware**. Every one either
makes you name the host or treats runners as an interchangeable pool.
Flotilla's `VesselRequirement` → placement resolution, with pins and
`PlacementPolicy`, is the unoccupied ground; "daemon per machine" is not.

**2. Settlement gates barely exist in the field, and the two that do are API
products rather than mainstream coding-agent paths.** Anthropic's Managed
Agents ships
[`define_outcome`](https://platform.claude.com/docs/en/managed-agents/define-outcomes):
a rubric, an independently-contexted grader ("uses a separate context window to
avoid being influenced by the main agent's implementation choices"), bounded
iterations, and a typed disposition —
`satisfied | needs_revision | max_iterations_reached | failed | interrupted`.
Devin's session API has
[`structured_output_required`](https://docs.devin.ai/api-reference/v3/sessions/post-organizations-sessions.md),
a platform-validated JSON Schema the agent must satisfy. Everything else —
Cursor, Codex cloud, Copilot, Jules, Factory, Amp — ends at "agent stops, human
reviews a PR". Anthropic's own code-review check run
[always completes as `neutral` so it never blocks a merge](https://code.claude.com/docs/en/code-review).
Flotilla's Settlement Claim and Exit Table (ADR 0017, ADR 0028) are aimed at
genuinely thin prior art.

**3. Argo Workflows is the structural prior art for the claim-versus-observation
split, and Flotilla should read it before finalising the Landing reconciler.**
Argo maintains two independent sources of truth per node and refuses to settle
until they agree: the world observation (`pod.Status.Phase`, which the workload
cannot write) and the claim (a `WorkflowTaskResult` CR the executor sidecar
writes, whose type comment says "This is an internal type. Users should never
create this resource directly"). `Fulfilled` is the conjunction, and there is a
force-settle timeout (`TASK_RESULT_TIMEOUT_DURATION`, default 10 minutes) for
the case where the claim never arrives. That timeout branch is the piece a
homegrown implementation forgets. Local checkout at `/Users/robert/dev/argo-workflows`.

**4. The counter-proposal to a settlement claim is Linear's Agent Session API:
don't let the agent name the disposition at all.** Sessions carry
`pending | active | error | awaitingInput | complete | stale`, and
[the state is derived, not asserted](https://linear.app/developers/agent-interaction) —
"Session state is visible to users, and updated automatically based on the
agent's emitted activities. No manual state management is required." The agent
emits `thought | elicitation | action | response | error` and Linear infers. It
cannot claim `complete`. There are also liveness deadlines (10 seconds to
acknowledge a created event), which is what `stale` is for. Flotilla's
Disposition is the explicit-claim design; Linear is the evidence-only design,
and the liveness-derived `stale` state is the thing Flotilla would otherwise
forget.

**5. Several prominent reference points are dead or moved, and any plan citing
2025-vintage knowledge is wrong.** Daytona's open-source repo was abandoned in
June 2026 with the LICENSE file removed from `main`. Terragon shut down
2026-02-09. vibe-kanban is sunsetting. Crystal is now Nimbalyst. The
documentation URLs for Cursor, Codex, and Claude Code have all moved. See
[§3](#3-corrections-to-stale-priors).

Two secondary findings worth carrying: **deploy-rs's magic rollback** is the
cheapest correct design for `fleet-install` and is about twenty lines of logic
([§8.2](#82-generation-and-rollback-mechanisms)); and **tmux control mode** is
still the only push-with-backpressure terminal control protocol in existence,
which makes it the reference design for cleat's control surface
([§7.3](#73-tmux-and-control-mode)).

---

## 2. Compact landscape table

Categories: **SB** sandbox substrate · **OR** orchestrator/manager ·
**HP** hosted platform · **TS** terminal/session · **FL** fleet/deploy.

| Project | Cat | What it is, in a phrase | License | Multi-host? | Status |
|---|---|---|---|---|---|
| [container-use](https://github.com/dagger/container-use) | SB | Container + git branch per agent, over MCP | Apache-2.0 | No | Slow — v0.4.2 (2025-08-19), 8 commits/90d |
| [E2B](https://github.com/e2b-dev/infra) | SB | Firecracker microVMs, FS+memory pause/resume | Apache-2.0 (infra too) | Self-host on GCP/AWS only | Very active, releases 2026-08-21 |
| [Modal Sandboxes](https://modal.com/docs/guide/sandbox) | SB | Managed sandboxes with the best egress policy model | Proprietary | No | SaaS |
| [Morph / MorphCloud](https://cloud.morph.so/) | SB | VM snapshot-and-branch ("Infinibranch"), <250ms | SDKs Apache-2.0; plane closed | No | SaaS |
| [Northflank Sandboxes](https://northflank.com/docs/v1/application/sandboxes/sandboxes-on-northflank) | SB | microVM-backed containers, persist until terminated | Proprietary | BYOC cloud accounts | SaaS |
| [Daytona](https://github.com/daytonaio/daytona) | SB | Agent sandboxes — **OSS repo abandoned** | License removed from `main` | No | **Dead repo**, core went private Jun 2026 |
| [Anthropic `srt`](https://github.com/anthropic-experimental/sandbox-runtime) | SB | Sandbox a process **without a container** (seatbelt/bubblewrap/WFP) | Apache-2.0 | n/a | Pushed 2026-08-21 |
| [kubernetes-sigs/agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox) | SB | `Sandbox`/`SandboxTemplate`/`SandboxClaim`/`SandboxWarmPool` CRDs | Apache-2.0 | Any k8s | v0.5.6 (2026-08-20), weekly |
| [microsandbox](https://github.com/superradcompany/microsandbox) | SB | libkrun microVMs, **daemonless**, local-first | Apache-2.0 | Yes (own machines) | v0.6.12 (2026-08-19) |
| [Vercel Sandbox](https://vercel.com/docs/sandbox/concepts) | SB | Firecracker; sandbox/session split; per-sandbox MITM CA | SDK Apache-2.0; service closed | No | Continuous |
| [Fly Sprites](https://docs.sprites.dev/) | SB | Persistent microVM keeping FS **and memory** between runs | Proprietary | No | SaaS |
| [OpenHands](https://github.com/OpenHands/OpenHands) | OR | Agent Canvas frontend over N Agent Servers | MIT | **Yes** (register backends) | v1.14.0 (2026-08-17) |
| [imbue `mngr`](https://github.com/imbue-ai/mngr) | OR | "git for agents" — SSH + git + tmux, pluggable providers | MIT | **Yes, natively** | Commits 2026-08-21 |
| [Sculptor](https://github.com/imbue-ai/sculptor) | OR | Desktop app, worktree per agent, **CI Babysitter** | MIT (closed process) | No | v0.44.0 (2026-08-14) |
| [Conductor](https://conductor.build) | OR | Mac app, parallel agents in worktrees, + Conductor Cloud | Proprietary | Local↔cloud only | 0.82.0 (2026-08-20) |
| [claude-squad](https://github.com/smtg-ai/claude-squad) | OR | tmux + worktrees + TUI, attach/detach | AGPL-3.0 | No | v1.0.20 (2026-08-20) |
| [GitButler](https://github.com/gitbutlerapp/gitbutler) | OR | **Argues against worktrees** — many virtual branches, one dir | FSL-1.1-MIT | No | 0.22.0 (2026-07-30) |
| [vibe-kanban](https://github.com/BloopAI/vibe-kanban) | OR | Kanban board driving agents — **sunsetting** | Apache-2.0 | No | Frozen 2026-04-24 |
| [Nimbalyst](https://github.com/nimbalyst/nimbalyst) (ex-Crystal) | OR | Desktop + **mobile** agent workspace, WYSIWYG diff approval | MIT | Phone-as-remote | v0.74.1 (2026-08-21) |
| [Backlog.md](https://github.com/MrLesk/Backlog.md) | OR | Markdown tasks with acceptance criteria + Definition of Done | MIT | n/a | v1.50.1 (2026-08-10) |
| [Cursor Cloud Agents](https://cursor.com/docs/cloud-agent) | HP | Firecracker VMs + **self-hosted worker pool** (Helm/CRD) | Proprietary | BYO compute | Active |
| [Devin](https://docs.devin.ai) | HP | Snapshot VMs, playbook-by-label, Managed Devins, Outposts | Proprietary | BYO compute | Active |
| [OpenAI Codex](https://learn.chatgpt.com/docs/cloud) | HP | Cloud chats + local CLI; **secrets stripped before agent phase** | CLI/SDK Apache-2.0; cloud closed | No | Active |
| [Claude Code web / Managed Agents](https://code.claude.com/docs/en/cloud-environments) | HP | Cloud VMs, teleport, self-hosted runners, `define_outcome` | Mixed (CLI proprietary) | Self-hosted envs | Active |
| [Copilot cloud agent](https://docs.github.com/en/copilot/concepts/agents/cloud-agent/about-cloud-agent) | HP | Assign an issue → draft PR, on GitHub Actions | Proprietary | ARC self-hosted runners | Active |
| [Amp](https://ampcode.com/manual) | HP | Orbs (remote machines, on e2b) + runners + Multiplayer + Puck | Proprietary | BYO runner | Active |
| [tmux](https://github.com/tmux/tmux) | TS | Control mode: the only push protocol with backpressure | ISC | Via ssh | 3.7c (2026-08-17) |
| [zellij](https://github.com/zellij-dev/zellij) | TS | WASI plugins, resurrection, web client with read-only tokens | MIT | **Yes** (own HTTPS) | v0.45.0 (2026-08-20) |
| [shpool](https://github.com/shell-pool/shpool) | TS | Session persistence, no multiplexing; single client per session | Apache-2.0 | No | v0.11.2 (2026-08-14) |
| [wezterm](https://github.com/wezterm/wezterm) | TS | Native cross-host mux — but exact codec-version equality | MIT | **Yes** | No tag since 2024-02-03 |
| [cmux](https://github.com/manaflow-ai/cmux) | TS | macOS agent terminal; resumable event stream, remote daemon | GPL-3.0-or-later | **Yes** | v0.64.22 (2026-08-03) |
| [Coder](https://github.com/coder/coder) | TS | Best-engineered reconnecting PTY, multi-viewer without a mux | AGPL-3.0 | Yes | v2.35.4 (2026-08-10) |
| [deploy-rs](https://github.com/serokell/deploy-rs) | FL | Nix deploy with **magic rollback** (canary + fresh connection) | MPL-2.0 | Yes | Active, zero releases ever |
| [Colmena](https://github.com/nix-community/colmena) | FL | Multi-host nix deploy; goals, tag globs, `keys` as a goal | MIT | Yes | Active; last tag v0.4.0 (2023) |
| [clan.lol](https://git.clan.lol/clan/clan-core) | FL | Peer-to-peer NixOS fleet: inventory + roles + vars + fallback | MIT | **Yes, and heterogeneous** | Active, no tagged releases |
| [greenboot-rs](https://github.com/fedora-iot/greenboot-rs) | FL | Health-check rollback via bootloader `boot_counter` | BSD-3-Clause | n/a | v0.16.4 (2026-08-18) |
| [balenaOS](https://github.com/balena-os/meta-balena) | FL | Two-level pin (fleet + device) and two independent rollback layers | Apache-2.0 / AGPL-3.0 | Yes | Continuous |
| [Talos](https://github.com/siderolabs/talos) | FL | Push upgrades; boot-once, verify, then make permanent | MPL-2.0 | Yes | v1.13.9 (2026-08-19) |

---

## 3. Corrections to stale priors

These invalidate 2025-vintage knowledge and are worth reading before anything else.

| Was | Is now |
|---|---|
| Daytona = the open agent-sandbox platform | **OSS repo abandoned.** README: "As of June 2026, Daytona's core development has moved to a private codebase. This repository will receive no further updates, fixes, or releases." Last release v0.190.0 (2026-06-23); the GitHub API now reports `license: null`. ([repo](https://github.com/daytonaio/daytona)) |
| Terragon = cloud Claude Code task runner | **Shut down 2026-02-09**; code open-sourced as [terragon-oss](https://github.com/terragon-labs/terragon-oss) (Apache-2.0). ([shutdown notice](https://docs.terragonlabs.com/docs/resources/shutdown)) |
| vibe-kanban = the kanban-drives-agents tool | **Sunsetting.** bloop shut down; per the [announcement](https://www.vibekanban.com/blog/shutdown) (2026-04-10) remote services ran 30 more days and the product transitions to fully local. Repo frozen 2026-04-24. |
| `All-Hands-AI/OpenHands` | Moved to **`OpenHands/OpenHands`**; the repo now ships **Agent Canvas**, a TS frontend. The agent engine moved to [software-agent-sdk](https://github.com/OpenHands/software-agent-sdk). |
| Crystal (`stravu/crystal`) | Renamed **Nimbalyst** (`nimbalyst/nimbalyst`); old repo frozen 2026-02-26. |
| `sst/opencode` | Moved to **`anomalyco/opencode`**. |
| sketch.dev, humanlayer's public repo, uzi | sketch **archived** 2026-01-15; `humanlayer/humanlayer` self-declares "pretty much all deprecated" (product rebuilt at humanlayer.com); uzi dormant since 2025-06-04. |
| Cursor "Background Agents", `docs.cursor.com` | **"Cloud Agents"**, `cursor.com/docs` (308 redirect). |
| `developers.openai.com/codex` | **`learn.chatgpt.com/docs`**; "tasks" are now "cloud chats". |
| `docs.claude.com/en/docs/claude-code/*` | **`code.claude.com/docs/en/*`** (301). |
| GitHub "Copilot coding agent" | **"Copilot cloud agent"** ([docs](https://docs.github.com/en/copilot/concepts/agents/cloud-agent/about-cloud-agent)). |
| Windsurf; `cognition.ai` | **Devin Desktop**; `cognition.com`. |
| Devin priced per ACU at a public rate | Self-serve moved to dollar pricing 2026-04-14; **no public per-ACU figure exists** today. ACUs survive only in enterprise order forms. |
| `zhaofengli/colmena` | Moved to **`nix-community/colmena`**; docs at [colmena.cli.rs](https://colmena.cli.rs/). |
| greenboot | **Deprecated 2026-02-10** in favour of [greenboot-rs](https://github.com/fedora-iot/greenboot-rs). |
| wezterm unmaintained after the maintainer stepped back | **Recovered.** Benoit de Chezelles (@bew) is now the primary day-to-day maintainer upstream; wez remains active. But still no tagged release since 2024-02-03. |

Also: licences are drifting away from OSI at the funded end. GitButler and
Charm's Crush both ship **FSL-1.1-MIT**; AgentsMesh ships **BSL-1.1**; Consul
and Nomad are **BUSL-1.1** (Licensor now IBM); Omni is BUSL with a home-lab
exemption. The durable OSI options in this space are OpenHands (MIT), opencode
(MIT), Nimbalyst (MIT), container-use (Apache-2.0), E2B (Apache-2.0 including
the control plane), and claude-squad (AGPL-3.0).

---

## 4. Sandbox and execution substrates

### 4.1 container-use — still the closest analogue to a Vessel

[container-use](https://github.com/dagger/container-use) gives each agent a
fresh Dagger container whose work lands on its own git branch, materialised
through worktrees; prerequisites are just Docker and Git. The
[git mechanics](https://raw.githubusercontent.com/dagger/container-use/main/repository/repository.go)
are the interesting part and three of them are directly liftable:

- It creates a **bare git repo as a fork** of yours, with `include.path` pointing
  at your real `.git/config` so it inherits your identity, and
  `commit.gpgsign` explicitly disabled for automated commits.
- It adds a remote literally named `container-use` to your working repo, so
  agent branches appear as remote-tracking refs **without polluting your branch
  namespace**. Environment IDs are petnames; branches are `container-use/<id>`.
- Two **git notes refs** carry out-of-band metadata: `container-use` for the
  environment's command history, `container-use-state` for its state. The audit
  log lives on the commits, not in a sidecar database.

Its settlement verbs are the cleanest in the survey:
[`merge`](https://container-use.com/cli-reference) "preserving commit history"
versus `apply` as "staged modifications without commits" — two genuinely
different acts, named differently. `checkout` explores locally.
Environments are "disposable by design".

Secrets are **references, not values**:
`container-use config secret set <KEY> <ref>` with four URI schemes —
`op://` (1Password), `env://`, `vault://`, `file://` — resolved inside the
container, with secrets stripped from logs and command outputs
([docs](https://container-use.com/secrets)). This is the only design surveyed
that never puts a secret value in config.

Human attach has two forms.
[`terminal`](https://raw.githubusercontent.com/dagger/container-use/main/cmd/container-use/terminal.go)
works around the Dagger Go SDK having no TTY forwarding by checking for
`DAGGER_SESSION_TOKEN` and, if absent, **re-execing itself under `dagger run`**
to borrow the CLI's interactive session. And
[`watch`](https://raw.githubusercontent.com/dagger/container-use/main/cmd/container-use/watch_unix.go)
is literally `git log --remotes=container-use --oneline --graph --decorate`
polled once a second — watching N agents becomes watching one git graph.

**Caveat:** last tagged release is v0.4.2 (2025-08-19), with 8 commits in the
trailing 90 days. Still self-described "experimental… in early development".
Single-box; no remote-engine or multi-host story documented.

### 4.2 Anthropic `srt` — sandboxing without a container

[`anthropic-experimental/sandbox-runtime`](https://raw.githubusercontent.com/anthropic-experimental/sandbox-runtime/main/README.md)
(Apache-2.0, pushed 2026-08-21) enforces filesystem and network restrictions on
an arbitrary process **at the OS level, without requiring a container**: macOS
Seatbelt profiles generated dynamically, Linux bubblewrap namespaces, Windows a
dedicated user plus Windows Filtering Platform egress fencing. Network is
deny-by-default — "all network access is denied by default. You must explicitly
allow domains" — enforced by an HTTP/HTTPS proxy plus a SOCKS5 proxy bridged
over Unix domain sockets. Config is `~/.srt-settings.json` with
`filesystem.allowWrite`, `filesystem.denyWrite`, `network.allowedDomains`; it
also wraps MCP servers by substituting `{"command": "srt", "args": [...]}`.

This is the most directly relevant sandbox finding for Flotilla, because it
fills the gap Flotilla actually has: **a `contained` Stance on a git-worktree
vessel gets no isolation today**, and `srt` is a walls-first realisation that
needs no container. It matches CONTEXT.md's stated preference for
"environment sandbox; wrapper sandbox on host-direct" over harness flags.

### 4.3 kubernetes-sigs/agent-sandbox — the resource cut to diff against

[agent-sandbox](https://raw.githubusercontent.com/kubernetes-sigs/agent-sandbox/main/README.md)
is a SIG Apps subproject providing `Sandbox` at `agents.x-k8s.io/v1beta1`, plus
`SandboxTemplate`, `SandboxClaim`, and `SandboxWarmPool`. The execution
primitive is a Pod on a "Sandbox Runtime" (gVisor or Kata), and the controller
owns creation, scheduled deletion, pausing and resuming; deep hibernation and
automatic resume are in development. Release v0.5.6 (2026-08-20), weekly cadence.

Three mechanisms are worth reading against Flotilla's own kinds
([api.md](https://github.com/kubernetes-sigs/agent-sandbox/blob/main/docs/api.md)):

- **`lifecycle.shutdownTime` + `shutdownPolicy: Retain|Delete`**, where `Retain`
  tears down the Pod but keeps the record and surfaces expiry as
  `Ready=False, reason=SandboxExpired`. That is exactly "the vessel expired but
  the record stays inspectable" — Flotilla's Park Depth spectrum (ADR 0028) with
  a k8s spelling.
- **The claim is the interesting resource**, not the template. `SandboxTemplate`
  is config; `SandboxWarmPool` is an optimisation that leaks into neither.
- **Orchestrator ≠ runtime**: it "delegates low-level container isolation to
  secure Sandbox Runtimes" rather than owning isolation. kagent draws the same
  seam between `Agent` and `ModelConfig`; ARK between `Agent` and
  `ExecutionEngine`. Three independent projects, one cut — the same one Flotilla
  draws between VesselRequirement and Environment.

Standing up k8s on a desk Mac plus two lab boxes to get these semantics is a bad
trade. **The CRD designs are the export, not the software.**

### 4.4 The cloud substrates, briefly

**E2B** is the one genuinely open control plane: `e2b-dev/E2B` and
`e2b-dev/infra` are both Apache-2.0, and the LICENSE file has not changed since
2023. Self-hosting is real but
[GCP/AWS only](https://raw.githubusercontent.com/e2b-dev/infra/main/self-host.md)
(Nomad + Consul + Terraform + Firecracker + Postgres, requiring nested
virtualisation) — not a personal-fleet story. Two ideas transfer:
[pause saves filesystem **and memory**](https://docs.e2b.dev/sandbox/persistence)
with indefinite retention (~4s per GiB to pause, ~1s to resume); and the
[PTY API](https://docs.e2b.dev/sandbox/pty) supports
**`pty.connect(pid)` to reattach an existing session**, with disconnect and
reconnect under a new data handler — the closest thing in the survey to cleat's
problem domain.

**Modal** has the most complete
[egress policy model](https://modal.com/docs/guide/sandbox-networking):
`block_network=True`, `outbound_cidr_allowlist`, and a beta
`outbound_domain_allowlist` (TLS/443 only), **combinable additively**, plus
`create_connect_token()` carrying "user metadata passed as an unspoofable
header". Its
[memory snapshots](https://modal.com/docs/guide/sandbox-snapshots) duplicate a
sandbox with processes still running, at the cost of terminating the original
and closing open TCP connections.

**Vercel Sandbox** contributes three ideas.
[Multi-agent isolation via Linux users](https://vercel.com/docs/sandbox/concepts):
"Give each AI agent its own Linux user with a private home directory, and share
files between agents with groups." A **per-sandbox MITM proxy CA** mounted into
the trust bundle with ~11 tool-specific env vars pre-set (`NODE_EXTRA_CA_CERTS`,
`PIP_CERT`, `GIT_SSL_CAINFO`, `CARGO_HTTP_CAINFO`, `REQUESTS_CA_BUNDLE`, …) so
the firewall is transparent to ordinary tooling — with the documented gotcha
that containers inside the sandbox do not inherit it. And a
**sandbox/session split**: "an agent workspace resumed once a day for a week is
one sandbox and seven sessions."

**Cloudflare's Sandbox SDK** has a
[session model](https://developers.cloudflare.com/sandbox/api/sessions/) worth
noting: "each session maintains its own shell state, environment variables, and
working directory, while sharing the sandbox filesystem and process space", and
setting an env key to `undefined` unsets it **for that session only**. That is
"several crew members, one checkout, different credentials" as a primitive.

**microsandbox** ([repo moved](https://github.com/superradcompany/microsandbox)
from `microsandbox/microsandbox`) is the best fit for "runs on my own machines"
alongside container-use: libkrun microVMs running standard OCI images, sub-100ms
boots, cross-platform, and explicitly **daemonless** — "Spawn VMs right within
your code. No setup server. No long-running daemon." v0.6.12 (2026-08-19).

**Arrakis** (AGPL-3.0 + commercial) has the right idea — cloud-hypervisor
microVMs with snapshot-and-restore so agents can backtrack multi-step workflows
— but is **dormant since 2025-05-29**.

**"agentcontainers" is not a thing.** A GitHub search returns six unrelated
repos, the largest with 10 stars. `devcontainers/spec` (CC-BY-4.0) has not been
pushed since 2026-03-20 and shows no agent-sandbox-specific evolution.

---

## 5. Agent orchestrators and managers

### 5.1 `mngr` — the closest thing to Flotilla anyone has built

[`imbue-ai/mngr`](https://github.com/imbue-ai/mngr) (MIT, commits 2026-08-21) is
the find of this survey. It bills itself as "a Unix-style tool for managing
coding agents… Seamlessly scale from a single local Claude to **100s of agents
across remote hosts, containers, and sandboxes**", and — the part that matters —
"Built on SSH, git, and tmux. Extensible via plugins. **No managed service
required**."

The design overlaps Flotilla's almost point for point:

- **Hosts are sandboxes created by pluggable providers** (local, Docker, Modal,
  SSH); remote hosts are "*always* accessed via SSH"; multiple agents can share
  one host; each agent gets **its own tmux session** named `mngr-*` for
  detach/reattach
  ([hosts.md](https://raw.githubusercontent.com/imbue-ai/mngr/main/libs/mngr/docs/concepts/hosts.md)).
- It models the **"outer host"** — the VPS or docker-daemon machine behind a
  container — as a first-class accessor. Flotilla's Host/Environment nesting
  question has the same shape.
- Addressing is `name@host.provider`, e.g. `mngr create my-task@.modal`.
- Verbs are `create`/`destroy`/`list`/`clone`/`message`/`push`/`pull`/`snapshot`/`migrate`,
  framed as "git for agents". Attach is `mngr connect <agent>` over SSH into the
  tmux session; `mngr message <agent>` injects a prompt programmatically;
  `mngr transcript` prints chat history; `mngr exec` runs on the agent's host.
- Its
  [idle-detection model](https://raw.githubusercontent.com/imbue-ai/mngr/main/libs/mngr/docs/concepts/idle_detection.md)
  is a configurable activity taxonomy (user input, agent output, SSH
  connections, agent process alive, creation, boot) bundled into named modes
  (`io`, `user`, `agent`, `ssh`, `create`, `boot`, `start`, `run`, `disabled`),
  plus host shutdown when all `mngr-`-prefixed tmux sessions have exited after a
  grace period.

It has no tracker integration and no settlement gate. It is a CLI with no
dashboard, no view model, no aggregator. But it is direct prior art for the
placement and residency half of Flotilla, and its idle taxonomy is more
carefully specified than anything Flotilla has written down.

### 5.2 Sculptor's CI Babysitter — a real system-verifies gate

[Sculptor](https://github.com/imbue-ai/sculptor) (MIT, "experimental research
preview") is otherwise unremarkable — worktree per agent by default, desktop GUI
— but its
[CI Babysitter](https://raw.githubusercontent.com/imbue-ai/sculptor/main/docs/help/ci_babysitter.md)
is one of only two genuine system-verifies mechanisms found in the OSS tier. It
watches open PRs, and when CI fails or a merge conflict appears it asks an agent
to fix it, polling GitHub every ~30s up to a configurable retry cap. Two design
details matter: it **never interrupts a busy agent** ("it waits for a quiet
moment"), and it **never merges or closes a PR itself** — unrescuable PRs are
left red for a human. That is a gate that deliberately stops short of
auto-settlement.

Worth flagging a contradiction: imbue.com/sculptor still says "Every agent runs
in its own container" and describes a "Pairing Mode", but the shipped docs say
worktrees are the default and containers are an experimental whole-backend
option, and "pairing" appears nowhere in `docs/help/`. Best reading is that the
product moved and the marketing page is stale — **could not verify** from a
changelog.

### 5.3 GitButler argues against per-agent worktrees

This is the live design debate Flotilla's Vessel model sits inside, and it is
worth knowing that the consensus is contested.
[GitButler](https://docs.gitbutler.com/ai-agents/parallel-agents) keeps **one
working directory** and organises changes into separate virtual branches and
commits, with each agent's commits organised onto its session's branch. Stated
rationale: "reuse one install and dev server when tasks can share runtime
state." Sculptor independently allows multiple agents in one shared workspace,
and Cloudflare's per-session env overlay over a shared filesystem is the same
instinct.

GitButler also explicitly refuses to own the settlement gate: "You decide how far
the agent goes. You can tell it to stop after local commits, or you can allow it
to push and open pull requests… your instructions decide when it stops."

It ships FSL-1.1-MIT (source-available, converts to MIT after a delay), and
`but agent setup` installs an agent skill plus version-control instructions;
agents commit selected hunks by ID to `but commit`, can stack dependent
branches, and do history edits without an interactive rebase. `but oplog`
restores an earlier local state.

### 5.4 Backlog.md — the strongest *contract* vocabulary

[Backlog.md](https://github.com/MrLesk/Backlog.md) (MIT, v1.50.1) executes
nothing; it is a markdown-native task layer where every task is a `.md` file in
the repo. It is here for its settlement vocabulary: tasks carry **acceptance
criteria** and a reusable **Definition of Done** checklist, plus milestones and
dependencies "to make execution order reviewable", and it structures agent work
around **three review checkpoints — review the spec, review the plan, review the
code** — with the constraint "one task = one context window = one PR." That is
Flotilla's Brief-as-contract instinct, written down by someone else.

### 5.5 The rest, briefly

**OpenHands Agent Canvas** has the right control-plane shape:
[register N Agent Servers](https://docs.openhands.dev/openhands/usage/agent-canvas/backends)
by display name, host URL and API key, and switch between them; a backend "can
be a separate process on the same machine, a self-hosted deployment on a VM or
container platform, or a managed Cloud or Enterprise service". The worked
example is exactly Flotilla's shape: "share an Agent Server with your team for
agents doing code review and dependency updates, then have your personal agents
running on your laptop." **Whether Canvas aggregates across backends or only
switches between them could not be verified** — the docs say settings, LLM, MCP
and automations "are all scoped to the active backend", which hints at
switching. Issue decomposition lives in a separate
[Automation Server](https://github.com/OpenHands/automation) driven by schedule
or webhook.

**claude-squad** (AGPL-3.0, v1.0.20) is the best pure-terminal attach UX:
a TUI session list, `↵/o` attaches, `ctrl-q` detaches, `tab` switches between a
preview tab and a diff tab. Mechanism is stated plainly — "tmux to create
isolated terminal sessions for each agent" plus "git worktrees to isolate
codebases". Single box; no tracker; the human is the gate.

**Nimbalyst** (MIT, ex-Crystal) is worth watching for one reason: a **mobile
companion** that shows "which agents need you and which are still working", lets
you reply by text or voice, swipe through diffs to approve, and sends push
notifications when an agent needs you. Also notable for WYSIWYG diff approval
across non-code artefacts (markdown, Mermaid, Excalidraw, CSV) — the diff-review
surface is not assumed to be code.

**Conductor** shipped Conductor Cloud 2026-07-30 (each agent in an isolated
cloud sandbox, workspaces org-shared with presence, versus local workspaces
private to the device). Its **Checks** concept aggregates git status, CI,
deployments, comments and todos — but both `/docs/checks` and
`/docs/concepts/checks` 404, so **whether Checks are an enforced gate or a
display could not be verified**.

**opencode** (`anomalyco/opencode`, MIT) matters architecturally rather than as a
competitor: it is
[client-server by design](https://opencode.ai/docs/server/), with
`opencode serve` running headless, an OpenAPI 3.1 spec at `/doc`, and — notably
— "the `/tui` endpoint can be used to drive the TUI through the server". That
makes remote-host operation with a local client feasible, which is the same
Surface-over-headless-core cut in Flotilla's roadmap.

---

## 6. Hosted vendor platforms

The full vendor sweep is long; this section keeps only what bears on Flotilla's
open design questions. Note that **split-plane BYO-compute has become the
standard enterprise answer**, with near-identical wording across four vendors:
the agent loop stays in the vendor cloud, tool execution comes to you, and
workers connect outbound-only. Cursor
[Self-Hosted Pool](https://cursor.com/docs/cloud-agent/self-hosted-pool) (Helm
chart + a `WorkerDeployment` CRD), Devin
[Outposts](https://docs.devin.ai/cloud/outposts/overview.md), Anthropic
[self-hosted environments](https://code.claude.com/docs/en/self-hosted-environments)
("Anthropic never connects into your network"), Amp runners, and Copilot ARC
runners are all the same shape. **Nobody ships a control plane you can own** —
except Kiro Crew (Apache-2.0, "does not require a Kiro Crew-hosted control
plane") and Factory, which alone offers a
[fully airgapped deployment](https://docs.factory.ai/docs/enterprise/network-and-deployment).

### 6.1 Issue-tracker binding shapes

Every vendor now does issue-driven dispatch. The interesting variable is the
*binding shape*:

| Binding | Who |
|---|---|
| **Assignee field** | Cursor (Linear/Jira), Devin (Linear/Jira), Codex (Linear), Copilot (GitHub), Claude Action (`assignee_trigger`) |
| **Label** | Jules (the `jules` label), Claude Action (`label_trigger`), Devin |
| **Mention** | everyone |
| **Platform automation config** | Cursor Automations, Devin Automations, Factory Automations, Claude Routines |
| **Agent-registered webhook** | Amp alone — the agent calls `amp.createWebhook` for a durable endpoint bound to its own thread |

Two findings worth carrying. **Devin's labels select the *playbook*, not merely
go/no-go**: `!plan`, `!implement`, `!triage`, `!review` each choose a procedure,
with word-boundary case-insensitive matching
([linear](https://docs.devin.ai/integrations/linear.md)). Nobody else has a
"which procedure runs" binding, and it is the natural spelling for
"which WorkflowTemplate does this issue dispatch under".

And **Copilot's issue payload is one-shot**: "Copilot will not be aware of, and
therefore won't react to, any further comments that are added to the issue"
([docs](https://docs.github.com/en/copilot/how-tos/use-copilot-agents/cloud-agent/use-cloud-agent-on-github)).
Nobody models the issue as a live contract the agent re-reads. Flotilla's
"ready means body-is-contract" discipline is the same insight from the other
side — if the body is the contract, it must be complete at dispatch.

### 6.2 Settlement: two exceptions and a lot of PRs

Covered in [§1](#1-executive-summary). The additional detail worth recording:

- Anthropic's `/goal` is honest about its limits: it "is a wrapper around a
  session-scoped prompt-based Stop hook", and "The evaluator judges your
  condition against what Claude has surfaced in the conversation. **It doesn't
  run commands or read files independently**"
  ([goal](https://code.claude.com/docs/en/goal)). That is a claim-grader, not a
  world-observer — precisely the distinction ADR 0028 draws.
- **Stop hooks are a real gate**: returning
  `{"hookSpecificOutput": {"hookEventName": "Stop", "decision": "block", "reason": "…"}}`
  prevents the turn ending ([hooks](https://code.claude.com/docs/en/hooks)).
- Copilot's CI **does not run by default**: "workflows are not triggered until
  Copilot cloud agent's code is reviewed and a user with write access… clicks the
  Approve and run workflows button"
  ([risks-and-mitigations](https://docs.github.com/en/copilot/concepts/agents/cloud-agent/risks-and-mitigations)).
- Codex's **Goal mode** names the right triple locally — Outcome, Constraints,
  **Verification**, where "The goal text becomes both the first prompt and the
  completion criteria"
  ([long-running-work](https://learn.chatgpt.com/docs/long-running-work)) — but
  it is unenforced and unavailable in cloud.

### 6.3 Credential delivery: the two good ideas

**Codex strips secrets before the agent runs.** Verified:
"By default, Codex blocks internet access during the agent phase. Setup scripts
still run with internet access", and secrets "are only available to setup
scripts. **For security reasons, secrets are removed before the agent phase
starts**"
([internet-access](https://learn.chatgpt.com/docs/cloud/internet-access)). Layered
on top: presets None/Common/All, HTTP methods restrictable to `GET, HEAD,
OPTIONS`, and "All outbound internet traffic passes through this proxy". Four
layers, coherently designed. Note also that OpenAI's own canonical
prompt-injection example is *issue-tracker-driven dispatch* — "Fix this issue:
…/issues/123" where the issue body contains a `curl` exfiltration.

**Amp and Devin use OIDC workload identity and inject no secrets at all.** Amp:
orbs prove identity "without having to inject any secrets into orbs", and "the
tokens are all short-lived and tightly scoped"
([secrets-of-the-orb](https://ampcode.com/news/secrets-of-the-orb)). Devin:
"Every Devin session automatically receives a short-lived identity token, signed
by Devin… No static API keys or secrets need to be stored in Devin", with
org/session/user-email claims usable in trust policies
([oidc](https://docs.devin.ai/product-guides/oidc.md)).

A third distinct approach: Anthropic's local **credential masking** — the command
sees a sentinel and "the sandbox proxy replaces the sentinel with the real value"
only for `injectHosts`, with JWT decode and AWS SigV4 re-signing. Critically,
`mask`/`tlsTerminate`/`awsPairs` "are all ignored in a repository's
`.claude/settings.json`", so repo-controlled settings cannot authorise
injection. That trust boundary is exactly right and worth copying verbatim.

Also worth knowing: Anthropic's cloud environments are **explicitly not a
secrets store** ("Anyone who uses the environment can read the values"), and a
cloud session "can access **any repository the connecting GitHub account can
see**… App installation is not a session-level access control"
([cloud-environments](https://code.claude.com/docs/en/cloud-environments)).

### 6.4 Attach-and-intervene: three verbs, converging

The vendors have converged on distinguishing three acts, and Flotilla should not
collapse them into one "send message":

- **Queue** — delivered at the next turn. Codex: "**Queue** saves the message for
  the *next* run" (Tab).
- **Steer** — delivered into the running turn, work preserved. Cursor:
  "Follow-ups wait for the next tool call instead of cutting the agent off
  mid-action." Copilot: "Copilot implements your input after it finishes its
  current tool call" — and notably, "Steering consumes AI credits per message",
  i.e. steering is metered. Codex: "**Steer** adds the message to the *current*
  run" (Enter).
- **Interrupt** — stops the turn, keeps work done so far. Claude Code: `Esc`
  "Stop the current response or tool call mid-turn so you can redirect. Claude
  keeps the work done so far. If you have messages queued, Claude Code sends
  them next" ([interactive-mode](https://code.claude.com/docs/en/interactive-mode)).

Claude Code adds two more that are worth stealing: `Up` from the first line
**pulls queued entries back into the input box** for editing, and `/btw` runs a
side question that "doesn't interrupt the main turn".

**Machine takeover** exists in four flavours: Cursor remote desktop
(Enterprise-gated), Devin writable-terminal-plus-browser-VS-Code, Factory's
`droid computer ssh` (the only documented SSH), and Amp's "open a terminal in
the same machine". Nobody documents SSH into a Cursor or Anthropic cloud VM.

**Anthropic's teleport is the best-specified web↔local handoff**: "Claude
verifies you're in the correct repository, **fetches and checks out the branch**
from the cloud session, and **loads the full conversation history** into your
terminal", requiring clean git state, the same repo (not a fork), a pushed
branch, and the same account. It is one-way from the CLI, and after teleport
"The terminal gets its own copy of the session: new work there stays local."
That last sentence is a fork, not a move — a Succession question in Flotilla's
vocabulary.

### 6.5 Multi-agent orchestration went mainstream

Two are worth close reading against Flotilla's own design.

**Amp's Puck** is a near-exact analogue of the meta-agent concept — "a quick
assistant and a home base for launching and coordinating other agents"
([meet-puck](https://ampcode.com/news/meet-puck)) — alongside agent-to-agent
spawning across machines with message and file exchange, and agents that "set
schedules and wake themselves up". That is Bosun and Quartermaster, shipped.

**Devin's Dynamic Workflows** is the only shipped durable-execution
orchestration substrate in the survey: "a **deterministic Python script that
orchestrates a team of Devin agents**" with `agent()/pipeline()/parallel()`, and
"Every agent call is recorded, so a workflow run is observable while it executes
and resumable if it is interrupted: completed agents replay their recorded
results instantly, and only the unfinished work runs again"
([dynamic-workflows](https://docs.devin.ai/work-with-devin/dynamic-workflows.md)).
Note the direction of travel: this is ADR 0008's "workflow semantics are
harvested from repeated practice and frozen as programs" arriving as a product.

Anthropic's **agent teams** carry a security property Flotilla will need the
same answer for: "a teammate **can't approve a permission prompt**… and a
teammate that was denied an action can't relay it to another teammate to bypass
the check" ([agent-teams](https://code.claude.com/docs/en/agent-teams)).

---

## 7. Terminal and session infrastructure

### 7.1 The three headline answers

**Cross-host attach, natively:** wezterm (ssh and TLS domains), zellij
(`zellij attach https://host:8082/session --token`, since 0.44), cmux
(`cmux ssh` plus a `cmuxd-remote` daemon), mosh, Eternal Terminal, sshx, tmate,
upterm, asciinema-server (view-only), VS Code tunnels, Coder. **Not native:**
tmux, shpool, abduco, dtach, ttyd, gotty, VibeTunnel, and cleat today.

Two caveats matter more than the list. wezterm's cross-host mux requires a
**byte-identical `CODEC_VERSION`** on every host (`codec/src/lib.rs`:
`pub const CODEC_VERSION: usize = 45;`, checked with
`info.codec_vers == CODEC_VERSION`) against a project with no tagged release
since 2024-02-03 — deploy the same nightly everywhere or nothing connects, and
every upgrade is a flag day that kills every session. tmux's non-native answer,
`ssh host tmux -C attach`, is version-tolerant and is what iTerm2 actually ships.

**Multiple simultaneous viewers:** tmux (including *grouped sessions*, where
sessions share a window set but keep independent current-window state), zellij
(with read-only tokens), wezterm, abduco, dtach, tmate, sshx, upterm, Coder,
asciinema. **Single-client:** shpool — explicitly, "only allows a single client
to be connected to a particular session at a time" — plus mosh, ttyd, gotty.

**Machine-readable control API,** ranked by what you could build on:
tmux control mode, cmux's v2 JSON-RPC, Coder's REST/WS, asciinema-server's REST
plus versioned binary WS, VibeTunnel, zellij's HTTP/WS plus WASI plugin API,
shpool's length-prefixed JSON, upterm's gRPC admin socket, and — weakly —
wezterm, which offers `--format json` on exactly 2 of 19 CLI subcommands and has
no event stream at all.

### 7.2 shpool — healthy, single-maintainer, still effectively Google

Apache-2.0, **v0.11.2 (2026-08-14)**, six releases in four months — the cadence
has accelerated. On the Google question: the repo lives under the
community-looking `shell-pool` org, not `github.com/google/shpool` (404), but
sources carry `Copyright 2024 Google LLC`, CONTRIBUTING requires the Google CLA,
and the dominant author since 2025 is a `@google.com` address. **One Google
engineer's project, Google-CLA-governed, hosted outside the google org.** Bus
factor 1.

Protocol is a length-prefixed JSON `ConnectHeader` enum over a Unix socket
(`Attach`, `List`, `SessionMessage`, `Detach`, `Kill`, `SetLogLevel`). The
daemon writes a `VersionHeader` on every stream "which allows the client to
compare version strings for protocol negotiation" — a **warning, not a hard
equality gate**, which is the right choice and the opposite of wezterm's.
`AttachStatus::Busy` enforces single-client exclusivity.

Worth watching: **`shpool-vterm`**, an experimental replacement for the
`shpool_vt100` fork, landed in v0.11.2 (2026-08-14). The vt100 fork was the
known weak point, and this is the same problem cleat solved differently.

Two ergonomic touches worth stealing: automatic prompt-prefix injection for
bash/zsh/fish so the user always knows which session they are in, and
`shpool detach <name>` to forcibly evict a stale client so you can take over.

### 7.3 tmux and control mode

tmux is the healthiest substrate here — **six releases in nine months** after a
13-month gap (3.7c on 2026-08-17), 27 open issues, mirrored from OpenBSD CVS.

Control mode is the reason it remains the strongest orchestration substrate.
From [tmux.1](https://raw.githubusercontent.com/tmux/tmux/master/tmux.1): output
blocks are framed `%begin` … `%end`/`%error`, and — the guarantee that makes a
parser tractable — "**A notification will never occur inside an output block.**"
Three features lift it from a byte pipe to a control plane:

- **Selective subscription with backpressure.** Client flags `no-output` and
  `pause-after=seconds`, plus per-pane
  `refresh-client -A <pane-id>:on|off|continue|pause` — "if all clients have
  turned the pane off, will stop reading from the pane."
- **Declarative format subscriptions.** `refresh-client -B name:what:format`
  where `what` may be `%*` (all panes) or `@*` (all windows); changes arrive as
  `%subscription-changed`, coalesced to at most once a second. Subscribe to
  `#{pane_current_command}` across all panes and you get pushed agent-state
  changes for free.
- **Non-disruptive observer attach.** `attach-session -r` = `read-only,ignore-size`:
  "only keys bound to the `detach-client` or `switch-client` commands have any
  effect" and "the client does not affect the size of other clients." **No other
  tool in this survey has both halves** — a watcher that can neither type nor
  resize the human's view.

Reading pane output splits three ways, and the distinction matters for agents:

| | `capture-pane` | `pipe-pane` | control mode `%output` |
|---|---|---|---|
| Model | pull / snapshot | push to a subprocess | push over the client stream |
| Content | **rendered grid** | raw bytes | raw bytes, octal-escaped |
| Misses bursts? | yes (scrollback-bounded) | no | no (`%extended-output` reports lag) |
| Backpressure | n/a | none | **yes** |
| Exclusivity | none | one pipe per pane, globally | many clients |
| TUI-friendly? | **yes** — final screen state | no | no |

For a TUI agent, `capture-pane -p -e -J -N -S -` is the right read. The
composition that works is **control mode as the event spine with `no-output`
set, then `capture-pane` on demand when an event says something changed.**

**tmux MCP servers are not worth depending on.** All are thin wrappers over
`capture-pane` + `send-keys`; none uses control mode; the most-linked one has had
no default-branch commit since 2025-08-24. Their convergent conclusions are the
useful signal: polling is the wrong shape, mutations must return stable IDs, and
the product requirement is "a tmux session that a human can attach to during the
same task."

### 7.4 zellij — and the agent-plugin ecosystem

MIT, **v0.45.0 (2026-08-20)**, the most actively released project in this
section. Three things bear on Flotilla.

**The web client makes URL the session identity.**
`http://127.0.0.1:8082/my-session` creates, attaches, *or resurrects*
([docs](https://zellij.dev/documentation/web-client)). **Read-only tokens are
first-class**: `zellij web --create-read-only-token --token-name "observer-token"`
produces credentials whose holders "can view sessions but cannot send input".
Terminal and browser are peers — `zellij attach https://host:8082/session --token`
attaches a *terminal* client across hosts with the same auth. And a separate
`zellij-no-web` binary ships for people who do not want the capability compiled
in, which is supply-chain hygiene worth imitating.

**Session resurrection serialises every second to a human-readable layout** that
can be inspected, edited, and moved between machines, optionally including
viewport and scrollback. The safety detail: resurrected command panes do **not**
auto-run — they sit behind a `Press ENTER to run...` prompt. **Restoring a
layout must not silently re-execute an agent.**

**The plugin API is permission-gated and can read scrollback in-process:**
`get_pane_scrollback` (full or viewport), `get_pane_running_command`,
`get_pane_cwd`, `get_pane_pid`, plus `web_request` so a plugin can call an HTTP
control plane directly; permissions are `ReadApplicationState`,
`ReadPaneContents`, `RunCommands`, `OpenFiles`, `ChangeApplicationState`
([plugin-api-commands](https://zellij.dev/documentation/plugin-api-commands)).

The ecosystem now has real agent plugins, and one of them contains the single
most useful engineering lesson in this section.
[`marktoda/zj-radar`](https://github.com/marktoda/zj-radar) is **push-driven,
not poll-driven**: status arrives via an explicit `zellij pipe` broadcast from
per-agent hooks, and the plugin never issues blocking host queries. This is a
deliberate hard constraint — its predecessor plugin melted a many-agent session
by polling every pane on every output event. Wire format is a versioned JSON
payload (`zj_radar.status.v1`) so any producer can emit it. See also
[`ishefi/zellaude`](https://github.com/ishefi/zellaude) (per-tab activity,
permission-request flash, click-to-focus the waiting pane, elapsed-time-in-state
for spotting stuck sessions).

### 7.5 cmux — the closest architectural analogue, with unusually candid docs

`manaflow-ai/cmux` is the one Flotilla already integrates
(`crates/flotilla-core/src/providers/presentation/cmux.rs`). **Licence note: it
is now GPL-3.0-or-later upstream**, though the local checkout's `package.json`
still says AGPL-3.0-or-later. Note also there are three unrelated projects called
cmux — `soheilhy/cmux` is a Go connection multiplexer, `craigsc/cmux` is a
different agent tool.

Its `docs/` are the most useful artefact. Four ideas:

**Deterministic binding, stated as a prohibition.** From
`docs/agent-session-tracking-spec.md`: "Surface to session binding is
established by construction at terminal/agent start, keyed by a cmux-minted
token. **Never from terminal-title string matching. Never from
newest-file-by-mtime scans.**" And: "The binding key is the surface id, which
MUST be invariant for a terminal's whole life (persisted, rehydrated verbatim on
restore, never re-minted). Workspace id is volatile and is never the key."
Mechanism is protected env keys injected at spawn (`CMUX_SURFACE_ID`,
`CMUX_WORKSPACE_ID`, …), so "a hand-typed `claude`/`codex` in any cmux terminal
already inherits the surface token"; hooks resolve by flag → env →
controlling-tty.

**Resumable event streams with gap detection.** From `docs/events.md`: "Every
event has a monotonically increasing process-local `seq` and a `boot_id`.
Persist the latest processed `seq`, then reconnect with `after_seq`… If cmux
restarts, `boot_id` changes and the server marks stale cursors as a **resume
gap**." Dual sink: `~/.cmuxterm/events.jsonl` for audit and catch-up, socket
`events.stream` for live delivery with bounded replay. This maps directly onto
the aggregator-delta and gap-recovery problem `flotilla-client` already has, and
onto the **Generation** concept in CONTEXT.md — `boot_id` *is* a generation.

**Two more principles worth quoting.** "Push is best-effort; pull is
authoritative… A missed or duplicated push self-heals on the next pull." And
"`ended` is retained, not deleted. Ended is a flag that disables the input bar.
It must not gate presence and must not delete the session."

**Cross-host is real**: `cmux ssh` uploads a SHA-256-pinned `cmuxd-remote`,
gets persistent remote PTY sessions, and uses a reverse CLI relay over TCP
rather than a Unix socket "because many servers have
`AllowStreamLocalForwarding` disabled" — a constraint Tender will hit. One
deferred finding settles a design question outright: "**tmux control mode
requires a lossless byte stream, while Mosh exposes synchronized terminal screen
state.**"

### 7.6 Recording, replay, and web attach

**asciicast v3 is released, not draft** — supported by asciinema CLI v3.0+,
player v3.10.0+, server v20250509+
([spec](https://docs.asciinema.org/manual/asciicast/v3/)). The load-bearing
change for Flotilla is that **timing is now relative** (`interval` from the
previous event, replacing absolute `time`), which makes a cast **appendable
without knowing the start time** — exactly what a long-lived agent session needs.
Also new: a required `term` object in the header, a `tags` array, and an `x`
(exit status) event. Markers "can act as breakpoints or be used for playback
navigation and automation" — the natural hook for turn boundaries.

**asciinema-server** (Apache-2.0, note the split from the GPL CLI) has the most
instructive streaming design here: producer at `/ws/S/<producer-token>`, viewers
at `/ws/s/<public-token>`; **the server runs its own VT emulator** so "late-joining
viewers immediately see the current display"; streams are created via
`POST /api/v1/streams` **before** any WebSocket connects; and a **60-second
producer reconnect grace window** keeps a stream "live" through a blip. The ALiS
binary protocol normalises dialects: "Consumers always receive ALiS v1 encoded
stream regardless of the protocol the producer uses."

**Coder** has the best-engineered reconnecting PTY, and does multi-viewer
*without* a multiplexer: `agent/reconnectingpty/buffered.go` keeps a 64 KiB
circular buffer and an `activeConns` map, replaying the buffer to each new
connection. Session identity is the reconnect UUID
(`GET /api/v2/workspaceagents/{id}/pty?reconnect=<uuid>`); the same UUID from
two browsers is two viewers of one PTY. Its `screen.go` backend documents the
hazard cleat would hit too: "Screen will happily spawn two separate sessions
with the same name if multiple attaches happen in a close enough interval" —
guarded with a mutex.

**Only two projects do record + replay + live attach for agent sessions.**
[VibeTunnel](https://github.com/amantus-ai/vibetunnel) (MIT) records all
sessions in asciinema format at `~/.vibetunnel/recordings/<uuid>.cast` while
fanning out live over one multiplexed binary WebSocket with **per-session
subscription flags `Stdout` / `Snapshots` / `Events`** — a subscriber can ask
for *screen state* rather than raw stdout. [Codeman](https://github.com/Ark0N/Codeman)
(MIT) spawns agents inside persistent tmux sessions and streams to a browser,
but does not use asciicast, so its "replay" is tmux scrollback.

**Skeptical note:** the "AI agent observability" category that dominates search
(Langfuse, AgentOps, rrweb, OpenReplay) is not terminal tooling — it traces LLM
calls or replays DOM. Nothing else does deterministic terminal
record-replay-attach.

**ttyd and gotty are not shared-session tools.** ttyd **spawns a new process per
WebSocket client and kills it on disconnect** (`spawn_process` per connection,
`pty_kill` on close); `--max-clients` gates independent sessions, not sharing.
Its one good design choice is `-W/--writable` — read-only by default. gotty's
original repo is dead (last commit 2017); the live fork is
`sorenisanerd/gotty`, same per-client-spawn model. **VS Code tunnels are
licence-blocked** for this use: the server licence forbids "provide the software
as a stand-alone offering or combine it with any of your applications for others
to use".

**upterm** is worth one note: its gRPC admin socket exposes **`ClientCount`** —
the only verified "is anyone watching right now?" query anywhere in the survey.
That is a useful signal for deciding whether an agent should pause for human
attention. (dtach's socket executable bit and `tmux list-clients` are the poorer
equivalents.)

---

## 8. Fleet deployment and multi-host coordination

### 8.1 clan.lol — the closest thing to Flotilla outside the agent space

[clan.lol](https://clan.lol/docs) is "a declarative framework for reliable,
self-hosted computing… peer-to-peer infrastructure management built on NixOS"
without a central controller. MIT; development on their own Forgejo
(`git.clan.lol/clan/clan-core`, updated 2026-08-20, no tagged releases). Four
mechanisms are directly applicable.

**Inventory: registry plus roles over tags.**

```nix
inventory = {
  machines = { ... };   # What exists
  instances = { ... };  # What runs on it
};
```

Machines carry `tags`, `deploy.targetHost`, and **`machineClass`** which
"defaults to `nixos`. Set it to `darwin` for Macs." Three tags exist by default:
`all`, `nixos`, `darwin`. Instances assign services to machines by **role**,
where roles take either `.machines."name"` or `.tags = [ "all" ]`. A
heterogeneous Mac-plus-Linux personal fleet is a first-class supported case —
rare, and exactly Flotilla's shape. Note the explicit design cut: "Clan builds
configurations statically from the inventory, without connecting to machines
first… **There is no auto-detection.**"

**`evalHost` / `buildHost` / `targetHost` as three orthogonal named roles.**
[Clan's ADR 05](https://git.clan.lol/clan/clan-core/raw/branch/main/docs/src/decisions/05-deployment-parameters.md)
diagnoses precisely the confusion Flotilla will hit: "Confusingly install always
evals locally and update always evals on the targetHost, so hosts have different
semantics in different operations contexts." The fix names all three for both
operations, settable in the inventory and overridable per invocation.
**Flotilla has the same three roles latent in `fleet-install` and should name
them before they diverge.**

**Priority-ordered networking with automatic fallback.** You declare multiple
networking services and "Clan tries them in priority order until one succeeds":
`p2p-ssh-iroh` (3000) → `internet` (2000) → `wireguard` (1000) → `zerotier`
(900) → `mycelium` (800) → `tor` (10). "If your direct connection fails, it
falls back to your VPN. If the VPN is down, it falls back to Tor. You don't
have to decide which path to use." This is a better model for Tender than a
single configured dial path, and it fits the hub/disconnectable topology
directly.

**`vars`: secrets as a generator DAG with an activation-phase ordinal.**
Generators declare `files`, `dependencies` and `prompts`; dependencies form a
DAG. Type safety is by construction: "Secret files are accessed via `.path` only
(their plaintext content is never readable at evaluation time)." Backends are
pluggable (sops, password-store, age, custom). **`neededFor`** controls *when
during activation* a secret materialises — `partitioning`, `activation`, `users`,
`services` — and **`share = true`** makes one secret span machines, for cluster
join tokens and mesh pre-shared keys.

Note the convergence: Clan arrived at inventory-plus-roles-over-tags without
setting out to build Kubernetes; Flotilla arrived at a k8s-isomorphic plane.
Same destination, different roads.

### 8.2 Generation and rollback mechanisms

**deploy-rs magic rollback is the single most transferable design in this
survey**, and it is about twenty lines of logic
([activate.rs](https://github.com/serokell/deploy-rs/blob/master/src/bin/activate.rs),
[deploy.rs](https://github.com/serokell/deploy-rs/blob/master/src/deploy.rs)):

1. On the target, activation writes a **canary lock file** and sets an inotify
   watcher on it, accepting only `Remove(RemoveKind::File)`.
2. It enters `danger_zone(done, confirm_timeout)` — a timeout on the event
   channel.
3. The deployer opens **a brand-new SSH connection** and runs `rm <lock_path>`.
4. If the file is removed in time the deployment stands; on timeout the target
   rolls *itself* back.

**The health check is "can the operator still reach me on a fresh connection",
not a service probe.** That catches the failure class that makes a machine
unrecoverable — SSH port changed, firewall wrong, sshd dead — by construction
rather than by enumerating checks. Two properties fall out for free: the
confirmation is an end-to-end proof of the control path, and **the rollback
decision lives on the host**, so it survives the operator's laptop closing.

Note the asymmetry the options encode: `autoRollback` (default true) handles
"activation script exited non-zero"; `magicRollback` (default true) handles
"activation succeeded and the machine is now unreachable". Conflating them loses
the second — the one that costs you a drive to the lab. Two timeouts, not one:
`activationTimeout` 240s bounds "did activation finish", `confirmTimeout` 30s
bounds "is it still healthy".

**greenboot-rs** is the reference design for health-check rollback below
userspace ([README](https://github.com/fedora-iot/greenboot/blob/main/README.md);
the Rust rewrite is [greenboot-rs](https://github.com/fedora-iot/greenboot-rs),
v0.16.4, after greenboot itself was deprecated 2026-02-10). Four ideas: health
as exit codes from a **drop-in directory** (`required.d` fails the boot,
`wanted.d` may fail harmlessly) with no plugin API and no registration; a
**decrementing counter in persistent bootloader state**, so the rollback decision
survives a host that never reaches userspace; and a bounded **blame window**
after promotion (`GREENBOOT_WATCHDOG_GRACE_PERIOD`).

**balenaOS splits rollback into two independent layers**
([rollbacks.md](https://github.com/balena-os/meta-balena/blob/master/docs/rollbacks.md)):
`rollback-altboot.service` for "the new OS is unbootable and does not get to
Linux userspace" (bootloader bootcount), and `rollback-health.service` for
"gets to userspace but something is unhealthy" (tests every minute for 15
minutes). Each is armed by a breadcrumb flag written during the update, enabling
it for exactly one boot. **Conflating the layers means a generation that hangs
`flotillad` is indistinguishable from one that reboots cleanly.**

Balena also contributes **the cleanest generation model in the survey — a
two-level pin with fallback**: `balena fleet pin <FLEET> <RELEASE>` /
`balena fleet track-latest`, plus `balena device pin <UUID> <RELEASE>` /
`balena device track-fleet`. A fleet-wide pointer plus a per-host override that
falls back to the fleet maps one-to-one onto "hold the lab servers on generation
N-1 while the desk Mac takes N". And **update locks**: `safe_reboot` refuses to
reboot while any container holds an exclusive lock on
`/tmp/balena/updates.lock` — the obvious analogue being "don't roll a generation
onto a host mid-convoy", which a file lock any workload can grab solves far more
cheaply than a scheduler.

**Talos needs no health-check DSL at all.** Its upgrade sequence is: cordon and
drain → shut down → verify disk → image upgrade → **set bootloader to use the new
kernel *once*** → reboot → **verify** → **make the change permanent**
([upgrading-talos](https://docs.siderolabs.com/talos/v1.12/configure-your-talos-cluster/lifecycle-management/upgrading-talos)).
Boot it once, prove it, then make it default. Talos is also the only *push*
model in the non-Nix tier and therefore the closest analogue to `fleet-install`;
its direction of travel (v1.13's `LifecycleService.Upgrade` with "real-time
progress reporting and parallel upgrade capabilities across multiple nodes") is
the shape to aim at.

**Mender states the correctness constraint most likely to bite a homegrown
implementation:** "going into `ArtifactRollback` is still possible after an
`ArtifactCommit`, and it must still roll back successfully, particularly when
the device loses power after running the steps inside `ArtifactCommit` but
before recording completion"
([interface-protocol](https://docs.mender.io/orchestrate-updates/interface-protocol)).
**Commit is an interval, not an instant, and the interval must be crash-safe.**

**systemd-sysupdate** contributes three: the generation-consistency invariant
stated explicitly ("An update can only complete if the relevant URLs provide
their resources for the same version"); cross-artifact atomicity via alphabetical
transfer ordering so "entry point resources (like bootloader kernels) are written
last"; and **two timers, two policies** —
`systemd-sysupdate-update.timer` downloads, `systemd-sysupdate-reboot.timer`
activates, which is exactly the split between "staged on the host" and "adopted
by the host". `sysext`'s compatibility manifest is the other lift: each image
declares which host generations it is compatible with, and the host **refuses**
to merge otherwise rather than silently misapplying.

**Two cautionary data points.** bootc's newer composefs backend has **neither
boot-entry counting nor a boot-complete service yet**
([boot-failure-detection.md](https://github.com/containers/bootc/blob/main/docs/src/boot-failure-detection.md)),
and Fedora CoreOS's automatic-rollback tracker issue has been
[open since 2018-09-11](https://github.com/coreos/fedora-coreos-tracker/issues/47).
This is harder than it looks.

**Colmena** contributes two small things with outsized value: **`keys` as a
standalone goal** — push secrets without touching the generation — and
**`uploadAt: pre-activation | post-activation`**, two words that encode a whole
class of ordering bug. Also `meta.allowApplyAll = false`, which refuses an
unfiltered fleet-wide action unless explicitly permitted.

**k3s and k0s both express "roll generation N across the fleet" as a declarative
Plan resource reconciled by a controller**, with `concurrency`, label-selector
targeting, and pre-drain as first-class fields
([system-upgrade-controller](https://github.com/rancher/system-upgrade-controller),
[k0s autopilot](https://docs.k0sproject.io/stable/autopilot/)). Given Flotilla
already has reconcilers and a resource store, **a `GenerationRollout` resource
with `selector`, `concurrency`, and a drain hook is the idiomatic replacement
for an imperative `fleet-install`.**

**One finding specific to the desk Mac:** `darwin-rebuild` has **no
remote-deployment flag at all** — grepping
[the script](https://github.com/nix-darwin/nix-darwin/blob/master/pkgs/nix-tools/darwin-rebuild.sh)
for `target-host` returns zero matches. Flotilla's `fleet-install` rolling to a
Mac is doing something the upstream tool declines to do, and ecosystem support is
thinner there than on Linux.

### 8.3 Transport, mesh, and the UDS-forwarding question

**`tailscale serve` can front a Unix socket — verified in source, undocumented
in the KB.** `cmd/tailscale/cli/serve_v2.go` passes `"unix"` to
`ipn.ExpandProxyTargetValue`, help text confirms
`unix:/tmp/myservice.sock`, and `ipn/ipnlocal/serve.go` genuinely dials it with
`d.DialContext(ctx, "unix", socketPath)`. The published
[docs](https://tailscale.com/kb/1242/tailscale-serve) list only ports, URLs,
file paths and `text:` — they are stale.

**But `tsnet` cannot listen on a Unix socket**: `tsnet/tsnet.go` returns
`errors.New("unsupported network type")` for anything outside tcp/udp variants.
So **embedding and UDS-fronting are disjoint in Tailscale**, and an in-process
fleet member that also exposes UDS endpoints is a capability Tailscale does not
offer. Tender providing both is new value rather than reinvention.

**The security lesson applies directly to Tender.** Tailscale commit
`fa542426e` (2026-06-03), "require local admin to serve Unix domain sockets":

> "This resolves a local privilege escalation (LPE). Prior to this change, a
> non-admin user could utilize serve to access local Unix sockets they otherwise
> should not be able to access. For example,
> `tailscale serve --http 80 unix:/var/run/docker.sock` would give the user
> access to the Docker socket (usually root only). This works because tailscaled
> has root access and implements the proxy to the socket (see also: 'the
> confused deputy problem')."

**Tender's UDS-forwarding surface has exactly this shape.** A privileged daemon
that will attach to an arbitrary socket path on request *is* a
privilege-escalation primitive. Decide who may name which socket path at design
time. Tailscale's fix was the blunt one: refuse unix targets unless the
requester is root.

**Nothing does UDS natively over a mesh.** Every working answer is a userspace
proxy at each end, in three shapes: `ssh -R remote_socket:local_socket`
(transport and proxy in one process — what Flotilla already does),
`tailscale serve unix:` (mesh transport plus a privileged in-daemon proxy), and
socat or `systemd-socket-proxyd` (proxy only, bring your own transport).
**Flotilla's current design is not a workaround; it is one of three legitimate
options and the only one needing no extra daemon.**

Three ssh options are the operational core and are easy to get wrong (all in
`ssh_config(5)`, not `ssh(1)`): `StreamLocalBindUnlink=yes` is **mandatory for
reconnection** — a stale socket file from a dead session is the most common
failure mode of long-lived reverse UDS forwards; `StreamLocalBindMask` prevents
the forwarded socket being world-accessible on the far host, since **the forward
*is* a privilege grant**; and `ExitOnForwardFailure=yes` turns silent partial
forwarding into visible failure.

`systemd-socket-proxyd` is local-only, but **`--exit-idle-time` is worth
stealing regardless**: the listener exists persistently and cheaply, the
expensive connection is established on first use and torn down when idle. That
decouples "the endpoint is addressable" from "the transport is up" — precisely
what a disconnectable-laptop topology needs.

Two enrolment mechanisms are worth naming. **Nebula's `nebula-cert sign -in-pub`
signs a public key generated on the target host**, so the private key never
leaves it and nothing secret transits during enrolment; Yggdrasil's
self-certifying addresses (`AddrForKey(publicKey)`) are the limiting case, where
a host's address *is* its public key. And **Consul's `auto_config`** delivers all
credentials in one validated bootstrap: the host presents a JWT via
`intro_token_file`, the server validates it against
`bound_issuer`/`bound_audiences`/`claim_assertions`, and returns TLS certs,
gossip key and ACL token together. That is a signed rather than bearer setup
token, and it is the closest published design to "Tender enrols a host and
delivers all its credentials at once".

**headscale** (BSD-3-Clause, v0.29.3) is the existence proof that fleet
membership can be a protocol under a permissive licence at personal scale — its
stated scope is "a *single* Tailscale network (tailnet), suitable for a personal
use, or a small open-source organisation". Note the shape of the split:
headscale reimplements *coordination* while the data plane stays stock
Tailscale. That is the same cut Flotilla is making between Tender (identity,
inventory, transport) and the resource plane.

**NATS** is plausible as a data plane — leaf nodes dial *outbound*, and JetStream
KV is "a stream named `KV_<bucket>`" with revisions, CAS and Watch, which is the
minimum viable set for a resource store — but **it has no UDS listener** (issue
#320, opened and closed the same day in 2016), so it replaces the transport
rather than solving the forwarding problem.

---

## 9. Focus question answers

### (a) Who else does multi-host personal-fleet orchestration?

**The honest answer is not "nobody" — but the personal-fleet framing is still
essentially unoccupied**, and the distinction is sharp rather than semantic.

The "daemon per machine plus aggregating client plus git-worktree isolation plus
PTY as session substrate" architecture has been independently rediscovered often
enough in the last six months to be the field's **consensus architecture rather
than a differentiator**. Verified instances: Omnara ("connect your own laptop or
VM… An agent can run with no machines or use several at once… You can add or
remove machines while the agent is running", Apache-2.0), Multica ("a *runtime*
is any machine agents can work on — your laptop, or a cloud box", one daemon per
machine into a Go/Postgres backend), AgentsMesh ("Run a hundred AI coding agents
across your own machines", runners dial out over gRPC+mTLS advertising capacity,
BSL-1.1), Paseo (`paseo --host workstation.local:6767 run "..."`, leaning on
Tailscale for transport), ai-maestro (peer mesh, every machine equal, no central
server, MIT), Comet (per-device daemon with optional multi-device sync), and
Centaur (each thread gets a k8s sandbox; "a lightweight k3s cluster on a small
always-on host is enough"). Plus imbue's `mngr` and OpenHands Agent Canvas.

**But not one of them does capability-aware placement.** Every multi-machine
tool verified either makes you name the host explicitly or has a trivial "any
free runner" pool. AgentsMesh gets closest and stops at raw capacity slots. None
does "this needs macOS and Xcode → the desk Mac; this needs the GPU → the lab
box". Most are venture-shaped, hub-and-spoke with a hosted control plane, and
"your machines" generally means a homogeneous pool of interchangeable Linux
boxes.

**The primitives that would solve it exist, and they all come from the build/CI
world — nobody has carried them across.** Ray's per-node custom resource
declarations (`--num-gpus`, arbitrary named resources) are the capability-matching
primitive. Jenkins' labels-plus-executors is the cheapest correct answer to "how
many agent sessions can this Mac mini take" — and precisely what AgentsMesh
reinvented as "runners advertise capacity". icecream pairs a scheduler with
**automatic compiler-environment shipping** so nodes need no matching toolchain.
distcc is heterogeneity-tolerant by design: "does not require all machines to
share a filesystem, have synchronized clocks, or to have the same libraries…
They can even have different processors or operating systems." GNU parallel
re-reads `--sshloginfile` when modified mid-run, so fleet membership is a live
editable file. SkyPilot has the cleanest self-owned-machine UX (SSH Node Pools in
`~/.sky/ssh_node_pools.yaml`) — **but its on-prem path requires a Debian-based OS,
so it cannot take your Mac.** That is exactly the heterogeneity a
desk-Mac-plus-home-lab fleet must handle.

**Anthropic's own products bracket the gap precisely.** Self-hosted environments
define Environment (a named routing destination), Runner ("the idea is the same
as a self-hosted CI runner"), and Session, with all traffic outbound — but it is
Team/Enterprise beta only, GitHub-only checkout, and the control plane stays
Anthropic's. Meanwhile the same docs point at the personal case: "If you want to
run Claude Code on your own always-on machine and drive it from other devices,
use Remote Control". **One** always-on machine, many client devices. **There is
nothing in between for an individual who owns several machines.**

**The home-lab constituency has not shown up.** Searches return the *inverse* —
people using an agent to manage their homelab, not distributing agent sessions
across it. The one on-target write-up found is Johnny Bilotta's "Claude Code on a
Home Server" (five machines, agents configured per host, dispatch via n8n's SSH
node, Tailscale for reachability) — one person, no reusable artefact. The
instructive counter-example is the modal answer: Domenic Denicola's "My Agentic
Coding Setup, July 2026" has three client devices on Tailscale but runs all
agents on **one** always-on Ubuntu VM, parallelised by worktrees. "The biggest
unlocks are using Tailscale and a dedicated Linux VM." **Centralise, don't
distribute.**

**Verdict: Flotilla's defensible ground is not "daemon per machine" — that is now
table stakes — but capability-aware placement across heterogeneous self-owned
hardware, with both planes owned locally.** That is `VesselRequirement` +
placement resolution + `PlacementPolicy`, and it is the part of the design least
worth compromising.

### (b) Credential delivery to contained agents

Ranked for a Rust daemon injecting into Docker containers and ssh-reachable
hosts:

1. **Credential proxy over a bind-mounted Unix socket or helper command.** The
   only pattern that works identically for Docker (bind-mount the socket) and
   ssh (`ForwardAgent /path/to/scoped.sock` — `ssh_config` accepts an explicit
   agent socket path, not just yes/no). The daemon keeps the real credential;
   the vessel gets an oracle it can be cut off from mid-run. Four independent
   prior arts share the same three-verb shape — ssh-agent, git credential
   helpers (`get`/`store`/`erase`), Docker credential helpers, and Claude Code's
   `apiKeyHelper` — so vessel-side integration is often **zero code**: point
   `credential.helper` at your socket. Note `credential.useHttpPath = true`
   narrows a git credential per repository path.
2. **Per-turn scoped GitHub App installation tokens minted by that proxy.**
   1-hour expiry, optional `repositories`/`repository_ids` scoping (up to 500),
   optional `permissions` subsetting, never exceeding the app's grant
   ([docs](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)).
   Composes with #1 so nothing long-lived enters the container, and it serves
   settlement gating directly: a token scoped to one repo cannot land work
   elsewhere.
3. **tmpfs file mounts at `/run/secrets/<name>`, mode 0400, per-container grant.**
   The universal fallback. Note the gap: **plain `docker run` has no secret
   primitive** — `docker secret` is Swarm-only and `docker run` has no
   `--secret` flag, so you construct the tmpfs mount yourself. Podman does have
   `--secret` plus a **`shell` driver that is itself a credential-helper hook**.
   Compose's own rationale is the argument to cite: env vars "can also be printed
   in logs when debugging errors without your knowledge", while file secrets get
   filesystem ACLs.
4. **systemd credentials for the daemon's own secrets at rest, per host.**
   `LoadCredentialEncrypted=` with `systemd-creds --with-key=host+tpm2`;
   delivered via `$CREDENTIALS_DIRECTORY`; access "restricted to the service's
   user"; **they do not propagate down the process tree**; stored in
   non-swappable memory; 1 MB per service. The detail worth copying is
   **`--name=` binding**: the credential name is embedded in the ciphertext "to
   ensure encrypted credentials cannot be renamed and reused for different
   purposes" ([systemd.io/CREDENTIALS](https://systemd.io/CREDENTIALS/)). This
   replaces plaintext under `~/.config/flotilla/` on Linux hosts.
5. **Egress control as the complement, not the competitor.** `srt`'s
   deny-by-default allowlist, Modal's three additive modes, Codex's four layers,
   Devin's Security Profiles with per-session escalation requests. A leaked
   credential that cannot reach the internet is a much smaller problem. Note
   GitHub's honesty about scope: its firewall "Only applies to processes started
   by the agent", does not cover MCP servers or setup steps, and "should not be
   considered a comprehensive security solution".
6. **RFC 8693 vocabulary now, even without an authorisation server.** Adopt
   subject-versus-actor, delegation-versus-impersonation, and the `act` chain —
   `draft-ietf-oauth-identity-chaining-17` is **in the RFC Editor queue**, so the
   cross-domain shape is about to stabilise. Worth knowing the OpenID
   Foundation's AI-agent work is a **Community Group, which by charter does not
   produce specifications** — there is no OIDF spec to adopt. The live work is at
   IETF OAuth (`draft-yakung-oauth-agent-attestation`,
   `draft-mcguinness-oauth-ai-agent-instance`,
   `draft-emerson-oauth-user-mediated-delivery`).
7. **SPIFFE/SPIRE as the model, not the deployment.** The transplantable insight
   is that *the party that created the container is the best attestor of it* —
   SPIRE's docker workload attestor derives selectors from container labels and
   image IDs. Flotilla's daemon already knows container ID, image, labels and
   worktree, so it can do in miniature what SPIRE does with a plugin stack. **No
   primary SPIFFE source applies this to agent sandboxes** — could not verify.

Environment variables rank last: both systemd and Compose docs argue against
them explicitly, and E2B's own docs warn its env vars are "not private in the OS".

### (c) Attach-and-watch UX ideas

The best-articulated primary source is **Claude Code's agent view**, and it is
close to a statement of Flotilla's own Demand/Regard/Salience problem:

> "Agent view, opened with `claude agents`, is one screen for all your background
> sessions: what's running, what needs your input, and what's done… watch their
> state at a glance instead of scrolling through transcripts, and **step in only
> when one needs you**." ([agent-view](https://code.claude.com/docs/en/agent-view))

Four mechanisms from it are worth lifting:

- **Six states, colour-coded**: Working, Needs input, Idle, Completed, Failed,
  Stopped.
- **An explicit attention ordering**: Pinned → **Ready for review (pull request
  status)** → Needs input → Working → Completed. Note that a *world observation*
  (PR state) outranks the agent's own liveness state in the sort. That is the
  same principle as ADR 0028's world terminals, expressed as a sort key.
- **Progressive disclosure**: `Space` opens a peek panel — "Most of the time the
  peek panel is enough and you don't need to open the full transcript."
- **Machine-readable attention signals as hooks**: the `Notification` event has a
  `notification_type` matcher with values `agent_needs_input`, `agent_completed`,
  `permission_prompt`, `idle_prompt`, alongside `Stop`, `SubagentStop` (carrying
  `agent_id`, `agent_type`, `last_assistant_message`), `SessionEnd`, and
  `TeammateIdle`. **Hooks can be HTTP, POSTing the full JSON to a supervisor
  URL** ([hooks](https://code.claude.com/docs/en/hooks)) — which is a ready-made
  Demand source for Flotilla's daemon, requiring no polling and no scraping.

Beyond that: **Nimbalyst's mobile attach** (push notification when an agent needs
you, voice reply, swipe-to-approve diffs) is the most novel UX in the survey.
**Amp's Multiplayer** is the most developed shared-session model — workspace
members "send messages and access the orb's files, changes, portals, and shared
terminal", lasting up to 7 days. **Claude Code's checkpointing** is the best-
specified rewind: a checkpoint per user prompt, 100 most recent snapshots,
persisted with the conversation so `/rewind` survives resume, with restore split
into **code, conversation, or both** — and honest limitations that matter
(bash-command file changes are not tracked, subagent edits are usually not
restored, "Not a replacement for version control").

And **`zj-radar`'s postmortem is the operational warning**: a status sidebar that
polls every pane on every output event will melt a many-agent session. Push from
per-agent hooks; never issue blocking host queries from the render path.

### (d) Issue-tracker-driven dispatch with settlement gates

Covered in [§1](#1-executive-summary), [§6.1](#61-issue-tracker-binding-shapes)
and [§6.2](#62-settlement-two-exceptions-and-a-lot-of-prs). The three
additions relative to
[`2026-07-29-state-transition-verification-prior-art.md`](2026-07-29-state-transition-verification-prior-art.md):

**Argo's `Fulfilled` conjunction plus force-settle timeout** — the structural
answer, detailed in [§1](#1-executive-summary). Read
`workflow/controller/taskresult.go` and
`pkg/apis/workflow/v1alpha1/workflow_types.go` in the local checkout. Its node
phase vocabulary is also instructive: `Pending`, `Running`, `Succeeded`,
`Skipped`, `Failed`, `Error`, `Omitted`, with `Error` distinct from `Failed` and
`Omitted` distinct from `Skipped`, and skipped/omitted deliberately *not*
"completed".

**Temporal's async completion is a capability to settle one specific run.** The
activity fetches a binary `TaskToken` from `activity.GetInfo(ctx)`, hands it to
an external system, and returns `activity.ErrResultPending`; later some other
party calls `CompleteActivity(taskToken, result, err)`
([docs](https://docs.temporal.io/develop/go/asynchronous-activity-completion)).
If Flotilla accepts settlement claims from crews, **minting a per-turn settlement
token beats accepting a claim keyed by issue number** — the token is unforgeable
and single-purpose.

**Airflow's deferrable operators say where the waiting should live.** A
deferrable operator suspends itself and frees the worker, relocating the wait to
a triggerer running "small, asynchronous pieces of Python code designed to run in
a single Python process"
([docs](https://airflow.apache.org/docs/apache-airflow/stable/authoring-and-scheduling/deferring.html)).
Read-across: verifying "PR merged" should be a deferred subscription in the
daemon — which is what the Leaf Engine already is — never a poll loop occupying a
vessel slot.

**Terminal vocabularies converge on more than success/failure.** Argo has `Error`
and `Omitted`; OpenHands' SDK has **`stuck`** as a first-class non-success
terminal (`ConversationExecutionStatus` with an explicit `is_terminal()` where
"IDLE is NOT a terminal state"); ACP has `refusal` and `max_turn_requests`;
Linear has `stale`. **A two-valued disposition will not survive contact.**

**Protocols worth tracking, briefly.** MCP's current revision is **2026-07-28**
and it is a major break — protocol-level sessions and `Mcp-Session-Id` removed,
the `initialize` handshake gone, `resources/subscribe` replaced by a single
`subscriptions/listen` stream, SSE resumability removed, Roots/Sampling/Logging
deprecated, and **tasks moved to an official extension**
(`io.modelcontextprotocol/tasks`) offering "asynchronous execution of
long-running operations, with polling, mid-flight input, and durable handles" —
a near-exact description of a Flotilla turn
([changelog](https://modelcontextprotocol.io/specification/2026-07-28/changelog)).
**A2A** shipped v1.0 (2026-03-12) under the Linux Foundation with the same task
vocabulary shape; track it, don't speak it. **AGENTS.md** is now stewarded by the
Agentic AI Foundation with ~20 supporting tools — emit it rather than inventing a
seeding format. **ACP's `StopReason` enum** (`end_turn`, `max_tokens`,
`max_turn_requests`, `refusal`, `cancelled`, with agents *required* to return
`cancelled` rather than propagating exceptions) is the closest published analogue
to Flotilla's Disposition and worth mirroring in the turn record.

---

## 10. Ranked adoptable ideas for Flotilla

Ordered by expected value: how much it improves Flotilla, divided by how much
work it is.

**1. deploy-rs magic rollback for `fleet-install`.** Canary lock file on the
target, inotify watcher, promoter reconnects on a **fresh** connection to remove
it, host self-rolls-back on timeout. Twenty lines. Two timeouts, not one. Keep
`autoRollback` and `magicRollback` distinct. The rollback decision lives on the
host, so it survives the laptop closing. *Nothing else in this survey is this
cheap relative to what it prevents.*

**2. `srt`-style walls-first containment for worktree vessels.** Flotilla's
`contained` Stance has no realisation on a host-direct git worktree today.
Anthropic's `srt` (Apache-2.0) sandboxes an arbitrary process with Seatbelt on
macOS and bubblewrap on Linux, with deny-by-default egress via HTTP+SOCKS5
proxies. This is the wrapper sandbox CONTEXT.md already names, available off the
shelf, on both platforms in the fleet.

**3. Credential proxy over a bind-mounted socket, minting per-turn scoped
tokens.** The one credential design that works identically for Docker and ssh,
with four independent prior arts sharing the same three-verb protocol, so most
tools need zero integration. Pair with per-turn GitHub App installation tokens
scoped to the convoy's repositories. Adopt container-use's **secret-reference
URIs** (`op://`, `env://`, `vault://`, `file://`) so no secret value ever enters
config.

**4. Name `evalHost` / `buildHost` / `targetHost` before they diverge.** Clan
wrote an ADR because collapsing them produced operation-dependent semantics —
install evaluating locally while update evaluated on the target. Flotilla has
all three latent in `fleet-install` today. Naming them is a documentation change
now and a migration later.

**5. `GenerationRollout` as a resource, not an imperative command.** k3s's
system-upgrade-controller and k0s's autopilot both express "roll generation N"
as a declarative Plan with `selector`, `concurrency`, and a drain hook,
reconciled by a controller. Flotilla already has reconcilers, a resource store,
and label selectors. Add balena's **two-level pin** (fleet pointer plus per-host
override falling back to the fleet) and Omni's `size` cap.

**6. Adopt the queue / steer / interrupt distinction in the turn model.** Three
vendors converged on it independently, with precise delivery semantics: queue is
delivered at the next turn, steer at the next tool-call boundary within the
current turn, interrupt stops the turn and keeps work done so far. Flotilla's
Brief delivery and re-prompt mechanics should name all three rather than having
one "send message". Add Claude Code's `Up`-to-edit-queued-messages and `/btw`
side-channel as follow-ons.

**7. Consume Claude Code's `Notification` hooks as a Demand source.** The hook
event carries `notification_type` ∈ {`agent_needs_input`, `agent_completed`,
`permission_prompt`, `idle_prompt`}, plus `Stop`, `SubagentStop`, `SessionEnd`,
`TeammateIdle` — and **hooks can POST to an HTTP URL**. That is a first-class,
push-based, zero-polling Demand feed from the harness Flotilla dispatches most
often. Codex and other harnesses will need their own adapters, which is exactly
what `AgentAdapter` is for.

**8. Argo's claim-versus-observation conjunction, with the force-settle
timeout.** `Landed = worldTerminalObserved && claimSynced`, plus a bounded wait
after which a crew that died without claiming still settles on observation
alone. Flotilla's Settlement Claim and Exit Table are already the right shape;
the timeout branch is the piece to add. Consider Temporal's per-turn settlement
token so a claim is a capability rather than an assertion keyed by name.

**9. Add liveness-derived states to the disposition vocabulary.** Linear's
`stale`, OpenHands' `stuck`, ACP's `refusal` and `cancelled`, Argo's `Error`
distinct from `Failed`. Flotilla's Disposition list is per-Brief and therefore
extensible by design, but the *builtin* vocabulary should include at least one
liveness-derived terminal that the crew cannot claim.

**10. cmux's deterministic binding prohibitions, as a written rule.** "Never from
terminal-title string matching. Never from newest-file-by-mtime scans." Bind by a
minted token injected as protected env at spawn, keyed on a **surface identity
invariant for the terminal's whole life** — never on a volatile workspace id.
Flotilla's attachable adoption path should state the same prohibitions before
someone writes the mtime scan.

**11. asciicast v3 for cleat session logs.** Relative intervals make a cast
**appendable without knowing the start time**, which is what a long-lived agent
session needs; the `x` exit-status event and markers give turn boundaries; and
the format buys `agg`, the asciinema player, and marker-based clipping for free.
Pair with asciinema-server's design: mint the stream before transport connects,
run a VT emulator server-side so late joiners see current state, and allow a
60-second producer reconnect grace window.

**12. Push with backpressure, following tmux control mode.** For cleat's control
surface: framed output blocks with the guarantee that notifications never appear
inside one; per-pane pause/continue so an unwatched session stops being read;
declarative format subscriptions coalesced to at most once a second; and an
**observer attach mode that can neither type nor resize** (`attach-session -r`).
Add cmux's resumable event stream with `seq`, `boot_id` and explicit gap
detection — which is the same shape as Flotilla's Generation and the aggregator's
watch-from-version.

**13. Clan's priority-ordered transport fallback for Tender.** Declare
reachability as an ordered list of services with automatic descent — direct →
mesh → relay → last resort — rather than a single configured dial path. This
composes with the hub/disconnectable topology and the connectivity-only dial
direction ruling.

**14. Colmena's `uploadAt` and Clan's `neededFor` for credential ordering.** Two
words that encode a whole class of bug: some credentials must exist before the
process starts, others must not be written until the new generation owns the
path. Flotilla will hit this the moment a generation carries credentials.

**15. Ergonomics worth copying wholesale, individually small.** shpool's
prompt-prefix injection so a user always knows which session they are in, and
`detach <name>` to evict a stale client. zellij's `Press ENTER to run...` gate so
restoring a layout never silently re-executes an agent. container-use's `merge`
versus `apply` as two named settlement verbs. container-use's `watch` as a
one-second git-graph tail. balena's update lock, so a generation never rolls onto
a host mid-convoy. upterm's `ClientCount`, so the system knows whether anyone is
actually watching before it raises a Demand.

**Deliberately not recommended.** Standing up Kubernetes to get agent-sandbox
semantics — read the CRDs, ignore the software. Adopting A2A or speaking MCP's
task extension yet — track the vocabularies. Any dependency on Daytona,
vibe-kanban, Arrakis, tmate, or wezterm's mux protocol. And building a second
event bus: ADR 0027 already ruled there is exactly one, and everything in this
survey supports that.

---

## 11. Open questions this survey raises

Not recommendations — things worth a decision, or a ticket.

- **Does Flotilla want a `stale`-equivalent that the crew cannot claim?** Linear
  derives it from a liveness deadline. Flotilla's Leaf Engine could evaluate
  staleness as an Observation, but ADR 0028 currently calls staleness "an
  attention flag" rather than a terminal. Is that still right once a convoy can
  be parked at depth?
- **Where does the `srt` wrapper sit relative to `Stance` realisation?** If it
  becomes the host-direct realisation of `contained`, the effective-stance
  recording and the loud-failure-on-under-realisation rules need to cover
  "`srt` unavailable on this host".
- **Should `boot_id` be Flotilla's Generation on the wire?** cmux independently
  arrived at the same construct for the same reason. Worth checking whether the
  observed-store generation and the transport-level resume gap are actually the
  same concept wearing two names.
- **What is the placement vocabulary for capabilities?** Ray's per-node custom
  resources and Jenkins' labels-plus-executors are the two shapes. Flotilla has
  `VesselRequirement` but the capability *vocabulary* (platform, toolchain,
  signing identity, GPU) is not enumerated anywhere. This is the differentiator
  from [§9(a)](#a-who-else-does-multi-host-personal-fleet-orchestration) and it
  is currently implicit.
- **Is Vercel's per-agent-Linux-user isolation cheaper than a container for the
  shared-checkout case?** GitButler and Sculptor both argue for multiple agents
  in one working directory; Cloudflare's per-session env overlay and Vercel's
  per-agent user are two mechanisms for that. Flotilla's Workspace Set assumes
  one checkout set per vessel.

---

## 12. Explicit non-verifications

Recorded so nobody treats absence of evidence as evidence.

**Highest-consequence.** The [§9(a)](#a-who-else-does-multi-host-personal-fleet-orchestration)
multi-host tier rests on one fetch per project. Star counts for several of them
(Multica, opencode, Orca) are implausibly high and should be re-checked before
being cited. Centaur's licence reads `NOASSERTION`; Multica's licence is
"Apache-2.0 plus conditions"; Paseo's README and API disagree. **If a decision
turns on "who else does this", re-verify that table first.**

**Sandbox tier.** Modal's underlying isolation technology (gVisor vs microVM) is
not stated in its Sandbox guide; Modal's `pty=True` and `modal shell` were
search-sourced, not directly fetched. Northflank's hypervisor, interactive TTY,
and on-prem BYOC. Daytona's hypervisor and what "Bring Your Own Compute" entails.
Morph's secret delivery, and whether its snapshots include memory (demonstrated
by a process-preservation example, not stated). Whether container-use secrets are
excluded from the container filesystem and git history. How Vercel delivers
*application* secrets as opposed to service auth tokens. Fly Sprites' attach,
secrets and network policy (only the overview was fetched; a separate
`research-fly-sprites` pass exists).

**Orchestrator tier.** Whether OpenHands Agent Canvas aggregates across backends
or only switches. Whether Conductor's Checks are an enforced gate (both docs
pages 404). Sculptor's container-versus-worktree transition (marketing and
shipped docs contradict; no changelog found). GitButler's in-GUI Agents Tab
(both doc URLs 404). Terragon's shutdown reason. Whether `async-code`,
`claude-code-agent-farm`, `claudia`, `catnip`, and `mux` exist and are relevant —
searched, not confirmed.

**Vendor tier.** Cursor's default VM specs, formal status enum, SSH access.
Devin's per-ACU rate and GitHub issue-assignment dispatch. Codex's
GitHub-issue→Codex assignment, whether cloud task logs stream live or poll, and
whether mid-run cloud steering exists. Anthropic's result-message `subtype` enum
(current docs show only `is_error`) and any native Linear/Jira dispatch trigger.
Copilot's GHES support, BYO-model, the GraphQL assign payload, and whether raw
terminal output is exposed. Jules' CLI licence and any egress control — and note
its changelog's newest entry is 2026-03-09, roughly 5.5 months of silence.
Kiro's issue-tracker dispatch. Antigravity's credential handling and licences.

**Terminal tier.** libghostty-vt has no published documentation site and no
stability commitment beyond a source comment; the reading came from a local
checkout at HEAD 2026-07-04. wezterm has no formal maintainership announcement,
and "no mux restart persistence" is a source-derived conclusion rather than a
documented statement. Eternal Terminal's multi-viewer support and survival across
`etserver` restart. cmux's multi-viewer model (not documented;
`cmux.com/docs/cli` 404s, so the CLI surface came from the repo's
`docs/cli-contract.md`). Codeman's "exactly-once input delivery" and
"full-scrollback replay" claims. tmate's website returned HTTP 503 on three
consecutive attempts.

**Fleet tier.** Fedora Silverblue (docs behind an anti-bot gate). systemd's
licence (not fetched). bootc's dual MIT. Whether `tailscale funnel` accepts
`unix:` targets, whether `tsnet.Dial` can reach one, and whether headscale's
Serve covers them. Whether Rust's `async-nats` exposes `CustomDialer`
equivalents. Client-versus-server precedence for `StreamLocalBindUnlink` on the
remote side of a `-R` forward. WireGuard having no UDS concept (inferred from L3
design, not verified). Whether any maintained OSS fork of Consul or Nomad exists.
agent-sandbox's kind/minikube quickstart (404).

**Cross-cutting.** Any primary SPIFFE/SPIRE material applying workload identity
to agent sandboxes. GitHub fine-grained PAT scoping and lifetime (both documented
URLs 404). Kubernetes Job condition version history and ordering guarantees
(page truncated). OpenTelemetry GenAI attribute stability — the semantic
conventions moved to their own repo and the schema URL is still TODO there; worth
re-checking in a quarter.
