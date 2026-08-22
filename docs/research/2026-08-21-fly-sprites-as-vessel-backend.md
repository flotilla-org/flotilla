# Fly Sprites as a third vessel backend, and their credential gateway

**Date:** 2026-08-21

**Status:** Primary-source research note. Not a ruling, not a proposal.

**Motivating questions:** (1) what a Sprite actually is; (2) whether its API can
host an interactive coding-agent session a remote client attaches to; (3) how
Fly's "secrets via proxy" story really works and whether Flotilla can copy it;
(4) what a Fly-backed `PlacementPolicy` would need; (5) what else in Fly's
platform maps to the Tender/federation direction.

## Executive summary

1. **A Sprite is not a Fly Machine with extra features — it is a different
   product with a different storage stack, built (today) *on* Machines.** Fly's
   own engineering post says so plainly: "They're related to Fly Machines but
   sharply different in important ways. They have an entirely new storage
   stack," and "Currently, Sprites run as standard Fly Machines," described as
   an implementation detail rather than a requirement
   ([design-and-implementation](https://fly.io/blog/design-and-implementation/),
   [code-and-let-live](https://fly.io/blog/code-and-let-live/)). The load-bearing
   line is **"the durable state of a Sprite is simply a URL"** — object storage
   plus replicated SQLite metadata, not an NVMe volume welded to one host.
2. **The interactive-session story is genuinely good and is the single most
   directly useful part for Flotilla.** Exec is a WebSocket with TTY, cols/rows,
   live resize messages, sessions that *survive client disconnect*, reattach
   with buffered output, and a separate list/kill surface
   ([exec API](https://docs.sprites.dev/api/v001-rc48/exec/)). This is a better
   fit for a crewed vessel than the Machines API, whose `exec` is one-shot with
   string stdin/stdout and **no PTY at all**
   ([openapi.json](https://docs.machines.dev/openapi.json)).
3. **The "secrets never land in the VM" story is real but narrower than the
   framing suggests.** Sprites Connectors are a **reverse-proxy gateway**, not a
   transparent forward proxy. The workload calls
   `https://api.sprites.dev/v1/gateway/<provider>/<connection_id>/<path>` and
   the gateway attaches the stored credential
   ([connectors](https://docs.sprites.dev/concepts/connectors/)). Nothing
   intercepts a call to `api.github.com`. **This works for anything with a
   base-URL override (LLM APIs) and works poorly for `git`, `gh`, and the agent
   CLIs whose credential shapes Flotilla actually delivers today.** See §4.4.
4. **The best idea to steal is not the proxy — it is the identity trick under
   it.** The gateway "identifies the calling Sprite from Fly.io's request
   signature (no Authorization header needed)". Flotilla already has the
   equivalent primitive and does not use it this way: the per-vessel
   contained-daemon Unix socket at `/run/flotilla-daemon`
   (`crates/flotilla-core/src/providers/environment/mod.rs:107-117`). **Socket
   presence is ambient vessel identity.** A broker on that socket needs no
   bearer token in the vessel at all. §4.5.
5. **As a vessel backend, Sprites is a poor fit for Flotilla's *current* shape
   and an interesting fit for its *stated* shape.** Poor: no inbound dial to the
   daemon mesh, no SSH, an opinionated fixed image (Ubuntu 25.10, 8 vCPU, 100 GB),
   and no way to bind-mount a host worktree — which kills
   `WorktreeOnHostAndMount` outright. Interesting: `CreateOpts` was *written* for
   this case ("remote sandbox providers may upload files or expose sockets
   through their own transport",
   `crates/flotilla-core/src/providers/environment/mod.rs:29-31`), and
   `FreshCloneInContainer` maps almost directly.
6. **The lock-in is concentrated in exactly one place: checkpoints.** Create,
   exec, files, ports, policy all have obvious equivalents elsewhere. Checkpoint/
   restore/fork of a 100 GB durable filesystem in ~1 s does not, and it is the
   thing you would design a workflow around and then be unable to leave.

## Method and confidence

Everything below was fetched from the source that owns the claim: `sprites.dev`
/ `docs.sprites.dev`, `fly.io/docs`, `fly.io/blog`, `docs.machines.dev`, and
public `superfly` repos. Machines-platform facts were gathered by a parallel
agent under the same primary-sources-only constraint. Repo-side facts are
verified against this checkout at `docs/adr-workflow-substrate`.

Confidence is **high** on API shape and mechanism (these come from reference
pages and the OpenAPI document), **medium** on latency numbers (vendor claims,
not measured), and **low** on pricing (see §2.5 — the exact figures are only
stated in login-gated forum posts I could not verify).

The Sprites API is versioned `v001-rc48` in its doc URLs and was `v001-rc30` in
search results captured shortly before. **This is a release-candidate API that
is visibly moving.** Treat every endpoint below as current-as-of-today.

---

## 1. What a Sprite is

### 1.1 Definition and positioning

> "A Sprite is a persistent, hardware-isolated Linux environment for running
> arbitrary code."
> — [docs.sprites.dev](https://docs.sprites.dev/)

> "Sprites are Linux virtual machines. You get root. They create in just a
> second or two."
> — [design-and-implementation](https://fly.io/blog/design-and-implementation/)

The product framing is explicitly agent-shaped: full Linux computers for agents,
"exactly as persistent and disposable as you want them to be." Fly's argument is
that ephemeral sandboxes are the wrong primitive for agent work, which wants a
durable environment across sessions
([code-and-let-live](https://fly.io/blog/code-and-let-live/)).

The image ships preinstalled: Ubuntu 25.10 with Node.js, Python, Go, Ruby, Rust,
Elixir, Java, Bun, Deno, Git, curl, wget, vim
([working-with-sprites](https://docs.sprites.dev/working-with-sprites/)) — and
Claude, Gemini and Codex, which the blog notes can run in
`--dangerously-skip-permissions` mode
([design-and-implementation](https://fly.io/blog/design-and-implementation/)).

### 1.2 The storage stack (the actual differentiator)

Three layers, described as JuiceFS-inspired
([design-and-implementation](https://fly.io/blog/design-and-implementation/)):

| Layer | Role |
|---|---|
| S3-compatible object storage | Immutable data chunks; the authoritative layer |
| SQLite metadata, durable via Litestream | Tracks chunk locations; checkpointed "aggressively" |
| Sparse 100 GB local NVMe | Read-through cache; **optional to durability** |

> "Nothing depends on local storage."

Cached chunks are "immutable and their true state lives on the object store."
The consequence Fly draws out is the one that matters: because durable state is
a URL rather than a host-pinned NVMe slice, **migration and recovery are
trivial**, and checkpoint/restore "merely shuffle metadata around" rather than
moving bulk data. That is why they can position checkpointing as "a basic
feature of the system and not as an escape hatch when things go wrong; like a
git restore, not a system restore."

Contrast with Fly Volumes, which are the opposite design: "A Machine can only
mount one volume at a time and a volume can be attached to only one Machine,"
an NVMe slice on the same physical host
([volumes](https://fly.io/docs/volumes/overview/)).

### 1.3 Orchestration model

Fly calls it **inside-out orchestration**: "the most important orchestration and
management work happens inside the VM." User code runs in an inner container;
root-namespace services handle the storage stack, checkpoint/restore, service
registration and restart, logging, and socket binding / proxy integration. This
lets them bounce a Sprite "without rebooting the whole VM, even on checkpoint
restores" ([design-and-implementation](https://fly.io/blog/design-and-implementation/)).

Creation avoids image pulls entirely — every Sprite "runs from a standard
container" pre-positioned on workers — which is where the one-to-two-second
create time comes from. **This is also the reason you do not get to supply your
own image.** See §5.3.

Public URLs propagate through Corrosion, Fly's gossip-based service discovery:
"we generate a Corrosion update that propagates across our fleet instantly."

### 1.4 Lifecycle, and what survives what

Three states ([lifecycle](https://docs.sprites.dev/concepts/lifecycle/)):

| State | What it is | Resume | Processes |
|---|---|---|---|
| **Active** | Running, billable compute | — | running |
| **Warm** | VM suspended, memory frozen | **100–500 ms** | "resuming exactly where they were" |
| **Cold** | VM stopped, in-memory state dropped | **1–2 s** | start fresh |

The idle window before pausing is **"about 30 seconds today"** — note the
hedge; this is a tunable, not a contract.

Survives a pause: files, directories, installed packages, git repositories,
on-disk databases. Lost on cold wake: running processes, in-memory state. And
the line that matters most for a daemon holding a session:

> **"Open network connections do not survive a pause, warm or cold."**

Resources: 8 vCPUs, memory that "autoscales under pressure rather than being a
constant allocation", 100 GB storage that does not autoscale and is
"TRIM-friendly: you pay for the bytes you actually write, not the full 100 GB."

### 1.5 Checkpoints

Checkpoints snapshot **the writable filesystem overlay only**
([checkpoints](https://docs.sprites.dev/concepts/checkpoints/)):

- **Captured:** files, directories, installed packages, config, on-disk DBs.
- **Not captured:** running processes, in-memory state, open connections.
- "This is the same line Lifecycle and Persistence draws between disk and
  memory: disk is in the snapshot, memory is not."

Mechanics: copy-on-write, so creation is fast and non-interrupting. Sequential
IDs (`v0`, `v1`, …). `sprite checkpoint create --comment "…"`; `sprite restore
v1`. Restore is **destructive** — "Restore is destructive. Checkpoint first." —
overwriting the filesystem and terminating active sessions. The last five
checkpoints mount read-only inside the Sprite at `/.sprite/checkpoints/`, so you
can `diff /.sprite/checkpoints/v34/etc/hosts /etc/hosts` without restoring. The
platform also takes background checkpoints under `auto-` IDs
(`sprite checkpoint list --include-auto`).

Agents can self-checkpoint from inside via `sprite-env checkpoints create` or
the management socket at `/.sprite/api.sock`. Fly is explicit that this is
**not** version control: "A checkpoint captures environment state, not code
history."

> ⚠️ **Documentation conflict.** The
> [working-with-sprites](https://docs.sprites.dev/working-with-sprites/) page
> describes checkpoint scope as "entire filesystem including permissions, file
> ownership, and process state (processes stop during creation). Takes 10–30
> seconds depending on data volume" — which contradicts the concepts page on
> both process state and duration, and contradicts the blog's "instantly" /
> "about one second"
> ([code-and-let-live](https://fly.io/blog/code-and-let-live/)). Do not design
> against either number without measuring.

### 1.6 Pricing shape

**Not verifiable from primary sources.** `https://fly.io/sprites/pricing/`
returns 404, `https://fly.io/pricing/` has no Sprites section, and the Sprites
landing page served me only install/auth content.

What the docs *do* state: Sprites "put themselves to sleep automatically when
inactive, and cost practically nothing while asleep", storage is billed on
bytes written rather than the 100 GB capacity
([design-and-implementation](https://fly.io/blog/design-and-implementation/)),
and compute is billed only while active
([lifecycle](https://docs.sprites.dev/concepts/lifecycle/)).

Search snippets attribute a two-tier storage model to Fly staff forum posts —
cold (Tigris-backed) ≈ $0.02/GB-month, hot (NVMe working set) ≈ $0.5/GB-month,
plus plan tiers with allowances for max concurrent active Sprites, warm Sprites,
CPU-hours, RAM-hours and storage GB-months. **These live on login-gated
`community.fly.io` threads and I could not open them. Treat as unverified.**

For calibration, the Machines pricing that *is* public
([pricing](https://fly.io/docs/about/pricing/)): stopped Machines bill only
rootfs at $0.15/GB per 30 days; running compute bills per second
(`shared-cpu-1x`/256 MB ≈ $2.02/mo, `performance-1x`/2 GB ≈ $32.19/mo);
volumes $0.15/GB-month. An always-warm 8-vCPU Sprite is not a cheap object;
the economics depend entirely on the 30-second idle window doing its job.

---

## 2. Sprite vs Fly Machine, side by side

| | Fly Machine | Sprite |
|---|---|---|
| Durable state | Volume pinned to one machine on one host ([volumes](https://fly.io/docs/volumes/overview/)) | Object storage + Litestream'd SQLite metadata; "state is a URL" ([blog](https://fly.io/blog/design-and-implementation/)) |
| Image | You supply any OCI image (`config.image`, required) ([openapi](https://docs.machines.dev/openapi.json)) | Fixed pre-positioned standard container |
| Create latency | "maybe low double digit seconds" first creation ([overview](https://fly.io/docs/machines/overview/)) | "just a second or two" |
| Suspend | Firecracker snapshot; **≤ 2 GB memory, no swap, no schedule**; resume "a few hundred ms" ([suspend-resume](https://fly.io/docs/reference/suspend-resume/)) | Warm state; resume 100–500 ms; no documented memory ceiling |
| Suspend durability | "Snapshots are tied to the exact code and state… If you deploy new code, the old snapshot can't be resumed safely and will be discarded" | Cold wake is a normal, documented path |
| Exec | `POST …/exec`, one-shot, string stdin/stdout, **no PTY, no streaming** | `WSS …/exec` with TTY, resize, detach/reattach |
| Interactive shell | `fly ssh console --pty` via hallpass over WireGuard ([blog](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)) | `sprite console`; **SSH not exposed, install `sshd` yourself** |
| Inbound | Fly Proxy + declared service, or 6PN/WireGuard direct ([services](https://fly.io/docs/networking/services/)) | `https://<name>-<org-id>.sprites.app/`, or `sprite proxy` (TCP over WSS) |
| Egress control | Network policies API; "once you create a rule for a direction… the default for that direction becomes deny all" ([network-policies](https://fly.io/docs/machines/guides-examples/network-policies/)) | DNS-based domain allowlist at `/.sprite/policy/network.json` |
| Rootfs on stop | Reset ("blank slate on every startup") | Persistent by construction |

The short version: **a Machine is a VM you configure; a Sprite is a durable
filesystem you occasionally attach a VM to.**

---

## 3. API surface

Base URL `https://api.sprites.dev/v1`, auth `Authorization: Bearer $SPRITE_TOKEN`
([sprites API](https://docs.sprites.dev/api/v001-rc48/sprites/)). (Note this is
cleaner than the Machines API, where fly.io/docs says `Bearer` and
docs.machines.dev says `FlyV1` and the OpenAPI document's `securitySchemes` is
`null`.)

### 3.1 Lifecycle

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/sprites` | `name` (required), `wait_for_capacity`, `url_settings.auth` (`sprite`\|`public`) |
| `GET` | `/v1/sprites` | `prefix`, `max_results` (1–50), `continuation_token` |
| `GET` | `/v1/sprites/{name}` | |
| `PUT` | `/v1/sprites/{name}` | `url_settings` |
| `DELETE` | `/v1/sprites/{name}` | 204 |

Sprites are addressed **by name**, not by an opaque generated id — convenient
for a control plane that already owns naming. There is no explicit
start/stop/sleep endpoint in this resource; sleeping is automatic and waking is
implicit in any request (§1.4).

### 3.2 Exec — the interactive path

[exec API](https://docs.sprites.dev/api/v001-rc48/exec/):

| Method | Path | Notes |
|---|---|---|
| `WSS` | `/v1/sprites/{name}/exec` | **"Commands continue running after disconnect"** |
| `GET` | `/v1/sprites/{name}/exec` | List active sessions: command, creation time, activity |
| `POST` | `/v1/sprites/{name}/exec` | "Simpler alternative… for environments that can't handle websockets" — **non-TTY only** |
| `WSS` | `/v1/sprites/{name}/exec/{session_id}` | **Reattach to a running session, with buffered output** |
| `POST` | `/v1/sprites/{name}/exec/{session_id}/kill` | `signal` (default SIGTERM), `timeout` (default 10s); returns streaming NDJSON |

Parameters: `tty`, `cols` (default 80), `rows` (default 24), `stdin`, `id` (to
attach), and `max_run_after_disconnect` — **TTY default 0, meaning forever**;
non-TTY default 10s. Client sends `{type: "resize", cols, rows}`. Server message
types include `session_info`, `exit`, `port_opened`, `resize`.

The Python SDK confirms the transport: "Commands use WebSockets", with an
optional `control_mode=True` that multiplexes new commands over a single control
connection and "falls back to the standard exec WebSocket otherwise"
([sprites-py](https://github.com/superfly/sprites-py)). PTY is
`sprite.command("bash", tty=True, tty_rows=24, tty_cols=80)`.

CLI equivalents: `sprite exec [--tty --env --dir --file --http-post
--no-port-forward] -- cmd`, `sprite console`, and a session layer —
`sprite sessions list|attach <id>|kill <id>`, with **`Ctrl+\` to detach and
reconnect later** ([working-with-sprites](https://docs.sprites.dev/working-with-sprites/)).

**Verdict on question 2: yes, comfortably.** Detached TTY sessions with
reattach-and-replay is exactly cleat's job description, offered as a hosted
primitive. See §6.2 for the caveat that makes this less free than it looks.

### 3.3 Filesystem

[filesystem API](https://docs.sprites.dev/api/v001-rc48/filesystem/) — a full
POSIX-ish surface, not just upload/download:

`GET /fs/read` (raw bytes), `PUT /fs/write` (raw body, `mode` octal, `mkdir`),
`GET /fs/list`, `DELETE /fs/delete` (`recursive`, `asRoot`), `POST /fs/rename`,
`POST /fs/copy` (`preserveAttrs`), `POST /fs/chmod`, `POST /fs/chown`
(`uid`/`gid`), and `WSS /fs/watch` for change notification. Most mutating calls
take `workingDir` and `asRoot`.

`chmod` + `asRoot` matters: Flotilla's credential delivery writes 0600 token
files and 0700 helper scripts
(`crates/flotilla-daemon/src/credential.rs:932-994`), which this surface can
express directly rather than via `sh -c` gymnastics.

### 3.4 Ports, proxy, services

- **Inbound URL:** `https://<sprite-name>-<org-id>.sprites.app/`, routing to
  port 8080 by default. `--url-auth sprite` (org members only, the default) or
  `public` ([networking](https://docs.sprites.dev/concepts/networking/)).
- **TCP tunnel:** `WSS /v1/sprites/{name}/proxy`; send a `ProxyInitMessage`
  `{host, port}` and "the connection becomes a transparent relay to any port"
  ([proxy API](https://docs.sprites.dev/api/v001-rc48/proxy/)). CLI: `sprite
  proxy 5432`, `sprite proxy 3001:3000`, with `-W/--stdio` and `--ssh` flags.
- **Services:** `sprite-env services create web --cmd python3 --args
  "-m,http.server,3000" --http-port 3000`, with `--env`, `--dir`, `--needs`
  (dependency ordering). The runtime owns them: start on boot, restart on crash,
  restart in dependency order after a **cold** boot, survive a **warm** wake
  intact. The proxy "automatically starts the service if it's not running when
  requests arrive." Logs at `/.sprite/logs/services/<name>.log`
  ([services](https://docs.sprites.dev/concepts/services/)).
- **SSH is deliberately absent** — "Sprites don't expose SSH directly for
  security"; you `apt install openssh-server` and register it as a service
  yourself ([working-with-sprites](https://docs.sprites.dev/working-with-sprites/)).

### 3.5 Tasks — the keep-awake primitive

Because a Sprite sleeps after ~30 s idle and **open TCP connections drop on
pause even when warm**, a long-running agent needs an explicit hold
([keeping-sprites-running](https://docs.sprites.dev/keeping-sprites-running/)).

Tasks are HTTP/JSON over the in-Sprite management socket `/.sprite/api.sock`:
`POST /v1/tasks`, `PUT /v1/tasks/:name` (refresh), `DELETE /v1/tasks/:name`.
Max lifetime **1 hour per registration**. The documented pattern is a 5-minute
expiry refreshed every minute — "four-heartbeat margin before pause", and
automatic cleanup if the process crashes.

This is a nice piece of design: the keep-awake claim is held *from inside*, by
the process that cares, and expires on its own. Compare Flotilla's `MaterialPool`
leases (`crates/flotilla-resources/src/material_pool.rs:8-38`), which are
control-plane-held rather than self-renewing.

### 3.6 Policy

[policy API](https://docs.sprites.dev/api/v001-rc48/policy/) — three
independently gettable/settable/deletable policies per Sprite:

- `policy/network` — `{"rules": [{"action": "allow"|"deny", "domain": "…"}]}`,
  supporting exact matches, wildcard subdomains (`*.npmjs.org`), and **preset
  rule bundles** (`{"include": "defaults"}`).
- `policy/privileges` — capability and device restrictions.
- `policy/resources` — memory limits.

The network policy is a **DNS-based allowlist**, enforced at resolution: "Raw IP
connections blocked unless resolved from allowed domains", "Private IP ranges
always blocked", changes reload live and drop newly-blocked connections, and a
blocked lookup returns `REFUSED` to `dig`
([networking](https://docs.sprites.dev/concepts/networking/)).

The rule bundle idea (`{"include": "defaults"}` for "LLM-friendly destinations")
is a small, good primitive: a named, vendor-maintained egress set that a policy
can compose rather than enumerate.

### 3.7 Remote MCP

`https://sprites.dev/mcp`, OAuth'd against a Fly account with org selection at
setup. Org-level tools (list/create/destroy) plus Sprite-level tools "generated
from the public Sprite environment API". Notable default:
**"the connector can only create Sprites whose names start with `mcp-`"** —
capability attenuation as the out-of-box posture
([remote-mcp](https://docs.sprites.dev/integrations/remote-mcp/)).

---

## 4. Connectors: the credential-injecting gateway

This is the part Robert flagged, so it gets the most care. **The reported
framing — "secrets never land in the VM; an egress proxy injects credentials
into outbound requests" — is half right, and the wrong half matters.**

### 4.1 The actual mechanism

Three components ([connectors](https://docs.sprites.dev/concepts/connectors/)):

1. **Encrypted credential storage** in the organization's database. "The token
   itself is never returned by the API."
2. **An access policy**, deny-by-default: which Sprites may use the connector,
   and which provider paths they may reach. "A connector with no policy refuses
   every Sprite."
3. **A gateway endpoint:**
   `https://api.sprites.dev/v1/gateway/<provider>/<connection_id>/<path>`

The workload calls the gateway instead of the provider:

```
curl -X POST "https://api.sprites.dev/v1/gateway/github/conn_gh789abc012/repos/acme/website/issues" \
  -H "Content-Type: application/json" \
  -d '{"title": "Flaky test in CI", "body": "Opened from a Sprite"}'
```

The gateway then:

- **identifies the calling Sprite from Fly.io's request signature — no
  `Authorization` header needed;**
- validates the access policy;
- forwards to the provider with the stored credential attached.

Note what is *absent* from that curl: any credential at all, including a
credential identifying the Sprite. That is the interesting bit (§4.5).

### 4.2 What is configured where

| Knob | Where | Values |
|---|---|---|
| Which Sprites may use a connector | Dashboard or API `access_policy` | `allow_all`, name prefix (`prod-` matches `prod-1`, `prod-api`), or `sprite_labels` (Sprite must carry **all** listed labels) |
| Which provider paths | API only (`allowed_endpoints`) | Exact (`/repos/acme/website/issues`) or wildcard prefix (`/repos/*`, `/*`) |
| Block rules | API only | **Checked before allow rules**; a path matching both is denied |

Providers today: **GitHub** (OAuth, personal or org-scoped by requested
scopes), **OpenRouter** (BYOK or a managed connector billed with the Sprites
plan), and **Custom API** (API key + base URL, wrapping "any token-authenticated
HTTP API").

Provisioning API:

```
GET  /v1/oauth/github/authorize?scopes=repo,read:org     # start OAuth
POST /v1/oauth/connections/api_key                       # {provider, api_key, access_policy}
POST /v1/oauth/connections/provision                     # {provider}  — managed
PUT  /v1/oauth/connections/<id>                          # {access_policy: {sprite_labels, allowed_endpoints}}
```

Labels are set at creation (`sprite create --label`,
[CLI](https://docs.sprites.dev/cli/commands/)), which makes the label the join
key between a Sprite and its credential grants — structurally the same move as
Flotilla's `CredentialGrantSelector`
(`crates/flotilla-resources/src/credential.rs:99-122`).

### 4.3 Discovery, and the agent-facing ergonomics

Sprites ships a **`sprite-api-gateway` skill preinstalled for Claude Code,
Cursor, Codex and Gemini**. Agents enumerate what they can reach with:

```
curl -s https://api.sprites.dev/v1/gateway/list
```

which returns available providers with a `gateway_base_url` and a
`usage_snippet`. The agent then talks to APIs in natural language "without raw
token exposure."

This is worth pausing on. The delivery mechanism for credential *capability* is
a skill file plus a discovery endpoint — the agent is *taught* where its
credentials live rather than handed them. That is a real pattern, and it is
orthogonal to the proxy.

### 4.4 The limitation that decides this

Fly states two limits: endpoint allow/block lists are API-only, and "gateway
calls must originate from inside a running Sprite (not from localhost or CI)."

The bigger limit is unstated and structural. **This is a reverse proxy, not a
forward proxy.** There is no TLS interception, no `HTTPS_PROXY`, no CA cert in
the trust store. A process that calls `https://api.github.com/...` directly gets
no credential and no help; only a process that has been *rewritten* to call
`api.sprites.dev/v1/gateway/github/<conn_id>/...` benefits.

The docs' own summary of the split is good and worth keeping as vocabulary:

> the policy determines **what** a Sprite may reach; Connectors determine
> **what it may reach as**.

So the pattern's reach is bounded by base-URL overridability:

| Consumer | Base-URL override? | Gateway viable? |
|---|---|---|
| Anthropic / OpenAI / OpenRouter HTTP APIs | Yes (`ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`) | **Yes** — this is the sweet spot, and why OpenRouter is a first-class connector |
| Arbitrary REST API called by agent-written `curl` | Yes, the agent writes the URL | **Yes** — hence the skill |
| `git push` over HTTPS | No — the remote URL is the provider host | **No** |
| `gh` CLI | Partially (`GH_HOST`), but the gateway's `/v1/gateway/github/<conn_id>/` path prefix does not match the `https://<host>/api/v3/` shape `GH_HOST` expects | **Doubtful** — untested, flagged as an open question |
| `claude` / `codex` CLI OAuth sessions | No — these are credential *files* and login state, not bearer tokens on outbound calls | **No** |

Cross-check against what Flotilla actually delivers today
(`crates/flotilla-daemon/src/credential.rs:846-1121`): `GH_TOKEN` plus a
`gh auth git-credential` helper; a GitHub App installation token in a 0600 file
with a `git-credential-github-app` helper; a Forgejo token file plus
`git-credential-forgejo`; `ANTHROPIC_API_KEY`; `CLAUDE_CODE_OAUTH_TOKEN` +
`CLAUDE_CONFIG_DIR`; `CODEX_HOME` after `codex login --with-api-key`.

**Exactly one of those seven — `ANTHROPIC_API_KEY` — is a clean fit for a
gateway.** The git-credential-helper family and the two agent-login families are
not. Adopting the Sprites pattern verbatim would leave Flotilla's highest-value
credentials exactly where they are.

Note also what Fly does for its *own* highest-value secret in the Claude
Managed Agents integration ([integration
docs](https://docs.sprites.dev/integrations/claude-managed-agents/)): not a
gateway at all. The org `ANTHROPIC_API_KEY` "stays off the Sprite" by simply
never being sent; a narrower **environment key** goes in instead, written to
`/root/runner.env`, and the supervising service "sources the environment file
then deletes it (credentials never persist on disk)." That is scope reduction
plus ephemeral file delivery — the same two moves Flotilla already makes. Their
answer to their own hardest credential problem is not the proxy.

### 4.5 What Flotilla should actually take from this

**The transferable insight is ambient vessel identity, not the proxy.**

The gateway works without a bearer token because Fly can authenticate the
*caller* from infrastructure it owns ("Fly.io's request signature"). Strip that
away and the pattern collapses into "put a different token in the vessel."

Flotilla already owns the equivalent primitive and currently uses it for
something else. `CONTAINED_DAEMON_SOCKET_DIRECTORY = "/run/flotilla-daemon"`
is bind-mounted per vessel — deliberately the parent *directory*, so a daemon
restart can replace the socket inode
(`crates/flotilla-core/src/providers/environment/mod.rs:107-117`). **Presence of
that socket in a vessel's mount namespace is proof of vessel identity, granted
by the same host that decided the placement.** A broker listening there needs no
token, no signature, and no shared secret: the mount *is* the credential, and
revocation is unmounting.

That yields a delivery shape with a genuinely different security property from
today's, without inventing an identity system:

- **New delivery mode, not a new source.** ADR 0022 already anticipates the
  source axis growing to "vault/minting/proxies"
  (`docs/adr/0022-credential-declarations-grants-and-local-material.md:40-49`),
  but this is not where a credential *comes from* — it is how it *reaches* the
  consumer. Today that distinction is implicit: `AdapterDelivery { env,
  git_credential }` (`crates/flotilla-daemon/src/credential.rs:163-172`) is the
  only shape, and every consumer in `prepare_adapter` hard-codes it. A brokered
  mode would want that to become an explicit axis.
- **What lands in the vessel:** a base-URL environment variable pointing at a
  loopback or socket-backed endpoint. No token, no file.
- **What the broker does:** resolve socket → vessel → granted
  `CredentialSpec`s (the `credential_refs` already stamped at
  `crates/flotilla-controllers/src/reconcilers/vessel.rs:1100-1113`), then
  attach material held on the host and forward.
- **What it buys:** material never crosses the containment boundary for
  `Contained`-stance vessels (`crates/flotilla-controllers/src/reconcilers/vessel.rs:1305-1310`),
  revocation is immediate rather than requiring re-provisioning, and every
  credential use becomes an auditable event at a chokepoint Flotilla owns.
- **What it costs:** the consumer matrix in §4.4. A first cut covers the
  Anthropic/OpenAI-shaped consumers and nothing else, and `Gh`/`GithubApp`/
  `Forgejo`/`ClaudeOauth`/`Codex` keep today's file-and-env delivery.

If the goal is to cover `git` and `gh` too, that requires the *other* design —
a real forward proxy with `HTTPS_PROXY`, TLS termination, and a per-vessel CA
in the trust store, injecting `Authorization` on the way out. That is a
substantially bigger and more invasive piece of machinery than what Fly built,
and **it is not what Fly built.** Worth naming explicitly so the two are not
conflated in a future ruling.

---

## 5. Suitability as a third vessel backend

### 5.1 Where it would plug in

The Explore pass identified four distinct seams a new backend must cross, all
currently docker-shaped:

1. **Policy vocabulary** — a third `Option<…>` block on `PlacementPolicySpec`
   (`crates/flotilla-resources/src/placement_policy.rs:57-101`), a new
   `PlacementStrategy` variant
   (`crates/flotilla-controllers/src/reconcilers/vessel.rs:135-158`), an arm in
   `placement_strategy()` (`vessel.rs:1114-1145`), and arms in roughly eleven
   `match &strategy` sites in the vessel reconciler.
2. **Environment resource** — a third `Option<…>` on `EnvironmentSpec`
   (`crates/flotilla-resources/src/environment.rs:9-15`).
3. **Reconciler runtime trait** — `DockerEnvironmentRuntime`
   (`crates/flotilla-controllers/src/reconcilers/environment.rs:11-15`), which
   is *named for docker* and only invoked when `obj.spec.docker.is_some()`
   (`environment.rs:104-114`). This one needs renaming before it can carry a
   second contained backend.
4. **Provider layer** — `EnvironmentProvider` / `ProvisionedEnvironment`
   (`crates/flotilla-core/src/providers/environment/mod.rs:192-220`), registered
   by descriptor name, with lookup by the literal string `"docker"`
   (`crates/flotilla-daemon/src/runtime.rs:2021-2027`).

Seam 4 is in good shape and was written with this exact case in mind:

> "Docker currently lowers them to bind mounts; **remote sandbox providers may
> upload files or expose sockets through their own transport**."
> — `crates/flotilla-core/src/providers/environment/mod.rs:29-31`

> "The source is deliberately described as a host path rather than as a bind
> mount. Bind mounting is one provider's delivery strategy, not the contract."
> — `mod.rs:63-66`

Sprites' `PUT /fs/write` is precisely the "upload files" lowering that comment
anticipates. `EnvironmentTool` with `EnvironmentToolAssetKind::{File,Directory}`
lowers cleanly; `UnixSocket` does not (§5.4).

### 5.2 What a Sprite-backed PlacementPolicy would carry

| `DockerPerVesselPlacementPolicySpec` field | Sprite equivalent |
|---|---|
| `host_ref` | **Nothing** — placement is Fly's, not a host's. Wants an org/account ref instead |
| `image` | **No equivalent.** Fixed image; closest analogue is a **checkpoint id** to restore from |
| `pull_policy` | Not applicable |
| `agent_adapters` | Still meaningful — the promise the base image + checkpoint makes |
| `default_cwd` | `workingDir` on exec and fs calls |
| `env` | Per-exec `--env`, or a service definition |
| `checkout: WorktreeOnHostAndMount` | **Impossible.** No bind mount of a host path exists |
| `checkout: FreshCloneInContainer` | Direct fit — `exec git clone` into `/home/sprite/…` |

Plus genuinely new fields with no Docker analogue: `labels` (the credential-grant
join key), `url_auth` (`sprite`/`public`), a `network_policy` rule set, and a
`checkpoint` to restore at provision time.

The `host_ref` row is the structural one. `HostDirectPlacementPolicySpec` and
`DockerPerVesselPlacementPolicySpec` both carry `host_ref`
(`placement_policy.rs:73-101`), and the vessel-placement projector routes
remotely-authored Vessels to the host that will actuate them
(`crates/flotilla-controllers/src/reconcilers/vessel_placement.rs:23-185`). A
Fly-backed vessel has an *actuating* host (whoever holds the API token and runs
the reconciler) but no *hosting* host. Those two meanings are fused today. That
is the cut worth getting right before writing any Fly code, and it is the same
cut a `Tender` will force anyway.

Stance is unambiguous: Sprites is `Contained`
(`crates/flotilla-controllers/src/reconcilers/vessel.rs:1305-1310`).

### 5.3 Blockers

1. **No inbound dial to the daemon mesh.** Flotilla's contained-vessel design
   assumes the vessel reaches the host daemon over a bind-mounted socket
   (`providers/environment/mod.rs:107-117`). Sprites has no bind mounts and no
   mesh join. The vessel can dial *out* (subject to the DNS allowlist), and the
   daemon can dial *in* over WSS exec/proxy — but the socket-mount pattern
   itself does not survive. Every capability currently riding that socket needs
   a different carrier.
2. **No custom image.** Fast create is *purchased* by the pre-positioned
   standard container (§1.3). Flotilla's image-recipe direction
   (`placement_policy.rs:88-89`) has no landing spot; you would express
   environment shape as a **checkpoint** instead — which is a genuinely
   interesting substitution (a prepared environment is a `v7`, not a tag) and a
   total lock-in (§5.5).
3. **`WorktreeOnHostAndMount` is out**, so a Fly backend supports exactly one of
   the two checkout strategies.
4. **Sleep breaks held connections.** "Open network connections do not survive a
   pause, warm or cold" (§1.4). Any long-lived attach must reconnect; any
   long-lived agent needs a Task heartbeat (§3.5). This is a real cost for a
   daemon that models a crewed vessel as continuously present.
5. **Release-candidate API.** `v001-rc30` → `v001-rc48` inside a short window.
6. **No SSH**, so `SshCommandRunner`
   (`crates/flotilla-core/src/providers/ssh_runner.rs:38-155`) does not compose
   here the way it does for remote Docker hosts.

### 5.4 The hop chain is the good news

`Hop::EnterEnvironment { env_id, provider }`
(`crates/flotilla-core/src/hop_chain/mod.rs:24-29`) is already the right
abstraction, and a Sprite hop is a *better* fit than the Docker hop rather than
a worse one. The Docker resolver flattens to a quoted shell string
(`docker exec -it -w '/workspace' flotilla-env-… <cmd>`), which is where the
quoting hazards live. A Sprite hop is a structured WebSocket open with
`{command, tty, cols, rows, workingDir, env}` — **no shell, no quoting, no
nesting**. Argument vectors stay vectors all the way in.

`DockerEnvironmentRunner` (`providers/environment/runner.rs:20-135`) has a
direct analogue: a `SpriteCommandRunner` decorating `CommandRunner` over
`POST /exec` for non-TTY, with `write_file` going to `PUT /fs/write` (which
keeps content out of argv even more cleanly than the current stdin trick at
`runner.rs:121`). `writable_base()` becomes a constant.

The one real casualty is `EnvironmentToolAssetKind::UnixSocket`
(`providers/environment/mod.rs:94-99`) — there is no mechanism to project a host
socket into a Sprite. Tools delivered as sockets would need a TCP shim over
`WSS /proxy`.

### 5.5 Lock-in vs portable

| Portable | Locked in |
|---|---|
| Create/destroy by name, list with prefix | **Checkpoint / restore / browse-without-restore** — no equivalent anywhere; the whole storage stack is the moat |
| Exec over WebSocket with PTY + reattach | The `.sprites.app` URL model and its org/public auth toggle |
| File CRUD over HTTP | The fixed base image and its preinstalled agent set |
| TCP tunnel over WebSocket | Connectors and the `sprite-api-gateway` skill |
| Domain-allowlist egress policy | Auto-sleep timing and Task heartbeat semantics |

The asymmetry is stark and worth stating plainly: **everything that makes
Sprites easy to adopt is portable; the one thing that makes it *better* is
not.** A Flotilla integration that treats Sprites as "a container that happens
to live elsewhere" stays cheap to leave. One that models prepared environments
as checkpoint lineages, agent state as restorable snapshots, or vessel identity
as Sprite labels does not.

Given the Plane-A/Tender transition is already consuming the disaggregation
budget, and given the §5.3 blockers (particularly the daemon-reach one), the
honest read is: **the seams are worth building, a Fly backend is not worth
building yet.** The valuable output of this note is §4.5 and §6 — the ideas —
not a Fly provider.

---

## 6. Other Fly platform patterns worth noting

### 6.1 Macaroons — directly relevant to Tender

Fly's tokens are macaroons with a chained-HMAC-SHA256 construction: each caveat
extends the chain `newTail = enc(hmac(oldTail, cavStr))`, so **any holder can
attenuate a token without the root secret**, and caveats only ever restrict —
"every single caveat must pass, evaluating True against the request… in
isolation" ([macaroons-escalated-quickly](https://fly.io/blog/macaroons-escalated-quickly/)).
Third-party caveats carry an encrypted ticket plus challenge; the third party
validates however it likes and issues a **discharge macaroon** presented
alongside the original — "a plugin system for security tokens." Extracted code
at [github.com/superfly/macaroon](https://github.com/superfly/macaroon)
(Apache-2.0), with the author's own warning: "We don't think you should use any
of this code; it's shrink-wrapped around some peculiar details of our production
network."

Concretely usable today: `fly tokens create machine-exec --command` /
`--command-prefix` restricts a token to running *specific commands* on a machine
([tokens](https://fly.io/docs/security/tokens/)). That is a capability shape
Flotilla's multi-host command routing has no equivalent of — a peer credential
that permits "run this exact verb" rather than "you are a peer."

For a federation layer where a hub delegates to a laptop that delegates to a
convoy, attenuation-without-the-root-secret is the property you want, and
third-party caveats are how a grant gets conditioned on an external check
without teaching the issuer about that check. Worth a look when Tender's
authorization model is designed, alongside the note in
`docs/adr/0022:40-49` about the credential source axis growing.

### 6.2 Detached sessions and cleat

Sprites' `max_run_after_disconnect` with a **TTY default of 0 (forever)**, plus
`WSS /exec/{session_id}` reattach with buffered output, is cleat's contract
offered as a platform primitive. Two observations:

- It validates the cleat design — the same conclusion reached independently.
- It does **not** remove the need for cleat on a Fly backend, because the buffer
  is Fly's and its retention is undocumented, and because a Sprite that goes
  cold restarts processes fresh. A durable transcript still wants to be
  Flotilla's.

### 6.3 Corrosion, 6PN, and the mesh

Fly's private networking is a per-org WireGuard mesh with IPv6-only 6PN
addressing and DNS at `fdaa::3`, exposing `<appname>.internal`,
`<machine_id>.vm.<appname>.internal`, `top<n>.nearest.of.<appname>.internal`,
and TXT records enumerating peers and machines
([private-networking](https://fly.io/docs/networking/private-networking/)).
Developer access is `fly wireguard create` plus a `.conf`, and `flyctl` embeds
**wireguard-go plus a gVisor netstack** so it can bring up a userland tunnel
in-process without root or kernel WireGuard
([ssh-and-user-mode-ip-wireguard](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)).

That last detail is the interesting one for the hub/laptop/forwarder topology:
**a userland WireGuard + userland TCP/IP stack linked into the client binary**
means a laptop joins a mesh with no privileged installation and no persistent
system state. If Tender ever wants transport-level meshing rather than
dial-out-only connectivity, this is the shape that avoids requiring root on
every desk machine.

Service discovery under it is Corrosion, gossip-based, propagating fleet-wide
"instantly" ([design-and-implementation](https://fly.io/blog/design-and-implementation/)).
Gossip-of-a-small-table is a plausible reference point for resource-store
federation, though the eventual-consistency semantics are exactly the thing
`docs/adr/0022`'s replication classes already reason about more carefully.

### 6.4 Small primitives worth stealing outright

- **Named rule bundles in policy** (`{"include": "defaults"}`) — composition by
  reference in an allowlist, so policies stay short and a vendor-maintained set
  stays updatable.
- **Self-renewing holds with a max lifetime** (Tasks: 5-min expiry, 1-min
  heartbeat, 1-hour ceiling) — crash-safe by construction, no reaper needed.
- **Default-attenuated integration tokens** (MCP connector limited to `mcp-*`
  names) — the safe posture is the default, not a checkbox.
- **Read-only mounts of recent snapshots** (`/.sprite/checkpoints/v34/…`) —
  inspection without restoration. Directly applicable to any Flotilla snapshot
  or state-history surface.

---

## 7. Adoptable ideas for Flotilla, ranked by leverage

**1. Ambient vessel identity via the contained-daemon socket, and a brokered
credential delivery mode built on it.** (§4.5)
The highest-value finding, and it does not require Fly at all. Fly's gateway
works because the caller is identified by infrastructure, not by a token;
Flotilla's per-vessel `/run/flotilla-daemon` mount is exactly that kind of
infrastructure fact and is already provisioned. Making delivery an explicit axis
alongside `CredentialSource`, with a brokered mode for base-URL-overridable
consumers, gets material out of `Contained` vessels for the LLM-API consumers,
makes revocation immediate, and creates one auditable chokepoint. Scope honestly:
it covers `Claude`(API-key) today and not the git/gh/oauth families.

**2. Name the delivery axis now, whether or not a broker gets built.**
`AdapterDelivery { env, git_credential }` is the only delivery shape today and
every consumer in `prepare_adapter` hard-codes it
(`crates/flotilla-daemon/src/credential.rs:846-1121`). ADR 0022 anticipated the
*source* axis growing but not the *delivery* axis. Fly's split — policy decides
what you may reach, connectors decide what you may reach *as* — is the right
vocabulary, and this is substrate: the cheap moment is before a second delivery
mode exists, not after.

**3. Rename `DockerEnvironmentRuntime` and de-literal the `"docker"` lookup.**
(§5.1) Small, mechanical, and unblocking. The trait
(`crates/flotilla-controllers/src/reconcilers/environment.rs:11-15`) is named
for one backend, gated on `spec.docker.is_some()`, and looked up by the literal
string `"docker"` (`crates/flotilla-daemon/src/runtime.rs:2021-2027`). Any second
contained backend — Fly, firecracker, or the `Clyde`/drydock extraction itself
(`CONTEXT.md:133-139`) — pays this cost first. Doing it independently of any
backend keeps it honest.

**4. Separate "the host that actuates a placement" from "the host that hosts
it".** (§5.2) Both `PlacementPolicy` blocks carry `host_ref` with those two
meanings fused. Service-provided environments — already named in the glossary as
runpod/modal/aws (`CONTEXT.md:408-412`) — have an actuator and no host. Tender
will force this cut regardless; a cloud backend just makes it visible sooner.

**5. Self-renewing, expiring holds instead of control-plane-held leases.** (§3.5)
Tasks expire on their own and are refreshed by the process that cares, so a crash
cleans up without a reaper. `MaterialPool` leases
(`crates/flotilla-resources/src/material_pool.rs:8-38`) are held by the control
plane and released by it. The Sprites shape is more robust for anything held by
a process that can die.

**6. Prepared environments as checkpoint lineages rather than image tags.**
(§5.3) A genuinely different idea: `sprite checkpoint create` after setup, then
restore for every subsequent vessel, replacing image build and push entirely
(and it is what Fly's own Claude integration recommends to skip repeated SDK
installs). Note this squarely as the **highest-lock-in** idea in this list —
worth understanding, not worth designing toward without a portable equivalent.

**7. Named rule bundles in egress policy.** (§3.6) `{"include": "defaults"}`.
Cheap, composable, and directly applicable if Flotilla ever gates vessel egress.

**8. Macaroon-style attenuation for Tender.** (§6.1) Any-holder attenuation
without the root secret, third-party caveats as a validation plugin point, and
command-scoped tokens (`fly tokens create machine-exec --command`). File against
Tender's authorization design rather than acting on it now.

**9. A Fly/Sprites vessel backend.** Last deliberately. The API is good and the
seams mostly exist, but §5.3's blockers — no daemon-reach socket, no custom
image, no host-worktree mount, connections dropped on sleep, an RC API — mean it
would consume disaggregation budget the Plane-A transition needs. Revisit when
the drydock boundary is proven and Tender has settled how a vessel reaches its
control plane without a bind mount.

---

## 8. Open questions and unverified claims

| Item | Status |
|---|---|
| Sprites pricing (hot/cold storage rates, plan allowances) | **Unverified.** Only on login-gated `community.fly.io`; `fly.io/sprites/pricing/` 404s and `fly.io/pricing/` has no Sprites section |
| Checkpoint scope and duration | **Contradictory across Fly's own docs** — see §1.5. Concepts page says filesystem-only and non-interrupting; working-with-sprites says processes stop and 10–30 s; blog says ~1 s |
| Whether `gh` can be pointed at a gateway via `GH_HOST` | **Untested.** Path shapes differ (§4.4); would need an experiment |
| Sprites exec output buffer retention on reattach | Not documented |
| Whether Sprites can be reached from a private network / VPC peering | Not documented; no 6PN equivalent found for Sprites |
| Concurrency limits, per-org Sprite caps, region selection | Not found in docs |
| Machines API auth header (`Bearer` vs `FlyV1`) | Docs conflict; OpenAPI `securitySchemes` is `null` |
| Fly network-policy endpoints | Documented in prose, **absent from `openapi.json`** |
| `sprite proxy --ssh` semantics | Flag exists in the CLI reference; behaviour undocumented |

## Sources

Sprites: [sprites.dev](https://sprites.dev/) ·
[docs.sprites.dev](https://docs.sprites.dev/) ·
[quickstart](https://docs.sprites.dev/quickstart/) ·
[working-with-sprites](https://docs.sprites.dev/working-with-sprites/) ·
[keeping-sprites-running](https://docs.sprites.dev/keeping-sprites-running/) ·
[lifecycle](https://docs.sprites.dev/concepts/lifecycle/) ·
[checkpoints](https://docs.sprites.dev/concepts/checkpoints/) ·
[connectors](https://docs.sprites.dev/concepts/connectors/) ·
[networking](https://docs.sprites.dev/concepts/networking/) ·
[services](https://docs.sprites.dev/concepts/services/) ·
[CLI](https://docs.sprites.dev/cli/commands/) ·
[sprites API](https://docs.sprites.dev/api/v001-rc48/sprites/) ·
[exec API](https://docs.sprites.dev/api/v001-rc48/exec/) ·
[filesystem API](https://docs.sprites.dev/api/v001-rc48/filesystem/) ·
[proxy API](https://docs.sprites.dev/api/v001-rc48/proxy/) ·
[policy API](https://docs.sprites.dev/api/v001-rc48/policy/) ·
[remote MCP](https://docs.sprites.dev/integrations/remote-mcp/) ·
[Claude Managed Agents](https://docs.sprites.dev/integrations/claude-managed-agents/) ·
[yolo-mode](https://fly.io/sprites/yolo-mode/) ·
[sprites-py](https://github.com/superfly/sprites-py)

Fly blog: [The Design & Implementation of Sprites](https://fly.io/blog/design-and-implementation/) ·
[Code And Let Live](https://fly.io/blog/code-and-let-live/) ·
[Macaroons Escalated Quickly](https://fly.io/blog/macaroons-escalated-quickly/) ·
[SSH and User-Mode IP WireGuard](https://fly.io/blog/ssh-and-user-mode-ip-wireguard/)

Fly docs: [Machines API](https://fly.io/docs/machines/api/working-with-machines-api/) ·
[openapi.json](https://docs.machines.dev/openapi.json) ·
[machine states](https://fly.io/docs/machines/machine-states/) ·
[suspend/resume](https://fly.io/docs/reference/suspend-resume/) ·
[autostop/autostart](https://fly.io/docs/launch/autostop-autostart/) ·
[machines overview](https://fly.io/docs/machines/overview/) ·
[volumes](https://fly.io/docs/volumes/overview/) ·
[private networking](https://fly.io/docs/networking/private-networking/) ·
[services](https://fly.io/docs/networking/services/) ·
[egress IPs](https://fly.io/docs/networking/egress-ips/) ·
[network policies](https://fly.io/docs/machines/guides-examples/network-policies/) ·
[tokens](https://fly.io/docs/security/tokens/) ·
[ssh console](https://fly.io/docs/flyctl/ssh-console/) ·
[pricing](https://fly.io/docs/about/pricing/) ·
[superfly/macaroon](https://github.com/superfly/macaroon)
