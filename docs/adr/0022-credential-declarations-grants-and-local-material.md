# Credentials: replicated declarations, host-local material, stance-first grants

**Status:** Accepted — amended 2026-07-28 (see Amendment below)
**Date:** 2026-07-27
**Relates to:** ADR 0016 (replication classes — declarations and grants ride
the definitions class), ADR 0010 (Hull/Crew boundary — credential state is
crew/vessel state), the fork-stance rulings (#978, #1047/#1049), #954
(attribution), issue #1050 (the grill that fixed this contract) and the
credential-pattern research
(`docs/research/2026-07-27-agent-harness-credential-patterns.md`),
map #1046.

Today every crew inherits the ambient identity of the human whose machine it
runs on: `gh` acts as Robert, harness logins are copied caches of personal
subscriptions, and an uncontained vessel can read all of `~/.config`. The
research established that no single env-var shape covers the supported
harnesses (Codex requires a login transformation, Docker a config entry,
some consumers a vendor-schema file), and that treating injection as
"copy env vars in" would preserve the ambient-identity problem for exactly
the consumers it appears to solve.

## The split: declarations replicate, material does not

- **Credential material is never a resource and never replicates.** The
  bytes live host-local (generalizing the existing
  `~/.config/lab-forgejo-<agent>-token` pattern), owned by the host that
  provisions vessels with them. Secrets do not travel the resource log,
  replicas, snapshots, or any future archive.
- **`CredentialSpec` declarations replicate** (definitions class): name,
  consumer adapter (`claude`, `codex`, `gh`, `forgejo`,
  `docker-registry`, …), source, lifecycle
  (`static | refreshable | issued`), and placement requirements. The ten
  requirements in the research are the field checklist (vendor-schema
  files, login transformation, mutable caches, multi-field entries, no
  hull/workspace persistence).
- Which credentials a host actually *holds* is an admission fact, same
  family as adapter availability: "feta lacks `codex` → cannot take this
  workflow" is refused early, mesh-wide, without any secret leaving home.

## Host-local resolution, evolving

The `source` axis starts as `file | env | issue-command` and is expected
to grow, all behind the same declaration and invisible to consumers:

- **vault-style managers**, fetched as late as possible;
- **provider-based minting** — GitHub App installation tokens (one hour,
  repo- and permission-bounded) are the model;
- **credential proxies**, where existing permissions are too coarse or
  tokens too limited.

## Delivery: per-consumer adapters with mandatory preflight

A generic transport cannot finish the job (research conclusion). Each
consumer adapter knows its harness's accepted forms, precedence, required
transformations (`codex login --with-api-key`,
`docker login --password-stdin`), and whether its cache must stay
writable. **Preflight is mandatory**: the adapter proves the credential
present and usable before the crew reports started — a missing credential
is a bounded provisioning failure, never a silently retrying crew.

## Identity: crews never carry a human's forge identity

- **GitHub**: a dedicated machine account for crew work now; evolving to
  a GitHub App minting per-crew installation tokens. One machine account
  for all crews initially — per-crew isolation comes from per-crew App
  tokens later, not account sprawl. PRs are authored by the crew
  identity; humans appear as reviewer/merger (#954 resolved honestly).
- **Lab Forgejo**: a crew-class agent user with tracker-scoped tokens,
  extending the per-agent-user pattern. Forgejo PATs do not expire, so
  rotation is operational discipline until an Authorized-Integration
  issuer exists (recorded unknown).
- **Human ambient identities are desk-only** and are never delivered into
  any vessel. Enforcement against an uncontained vessel is best-effort —
  filesystem visibility makes scoping advisory there, and this is stated
  rather than pretended away. A future global security/setup agent gets
  "encourage and drift-check credential discipline" as a standing duty.

## Reach: default-deny grants, stance first

- A declaration says what exists; a **grant** binds credential names to
  selectors — **stance** as the primary key (fork-stance crews receive
  model-API credentials only; trusted-repo crews add the crew forge
  identity), refined by project/repo. A vessel receives exactly the union
  of matching grants, resolved at admission. Nothing is ambient.
- Grants are policy: they replicate alongside declarations.
- **Uncontained vessels get an env allowlist at launch** as the backstop:
  only granted variables pass through.

## Migration: no flag day

1. Contained crews are **default-deny from day one** — a new path with no
   legacy to preserve.
2. Uncontained crews keep working ambiently while the vocabulary lands,
   then move to the allowlist with a warning phase.
3. Operator tasks are explicit HITL work: create the machine account,
   place tokens on hosts, create the Forgejo crew user.
4. Ambient inheritance in crew provisioning is then **removed**, not
   discouraged: reaching a human identity afterwards requires escaping
   the allowlist, not being handed it.

## Amendment (2026-07-28): subscriptions primary, lease-located material, forge/model split

Ruled during the #1140 seeding design (rulings and evidence in that issue;
mechanics in `docs/research/2026-07-28-multi-crew-agent-config-seeding.md`).

### Forge identity and model identity are different categories

"Crews never carry a human's forge identity" **stands unchanged** — forge
credentials are the crew's own (GitHub App `flotilla-crew`, Forgejo crew
user). But **model-provider auth may deliberately be the operator's
subscription**: solo devs, homelabs, and small teams are the primary target,
and their economics are plan subscriptions, not metered API keys. This ADR's
original framing listed "harness logins are copied caches of personal
subscriptions" among the problems; the amendment sharpens it — the defect was
*ambient, unmanaged* copying, not subscription-backed crews. Subscriptions are
the **primary** auth driver; API keys are the secondary/alternative.

### Material is lease-located, not host-pinned

The original text said material is "owned by the host that provisions vessels
with them." Generalized: **material resides wherever its lease is currently
held** — it moves point-to-point at lease transfer and still never rides the
resource log, replicas, or snapshots. Rotating login material (a codex
ChatGPT login's refresh chain is single-use — reuse is a permanent,
server-detected failure) is managed as a **pool of exclusively-leased homes**:
mint up to a concurrency high-watermark, lease one to one holder at a time
(bind-mount the durable slot directory into the vessel so refresh write-back
lands in the store copy — single-writer enforced by lease exclusivity, not
discipline). Pool exhaustion is in-system semantics: the demander *waits
visibly*, and exhaustion offers minting another slot. Non-rotating material
(a `claude setup-token` long-lived token) needs no lease — one mint serves N
crews per-process.

### Minting is HITL, up-front, and in-system

Subscription logins require a browser: `codex login --device-auth` (one-time
account-level enablement; link + one-time code enterable anywhere) and
`claude setup-token`. These are **first-class flotilla flows** — an
interactive mint vessel surfaces the link/code through the normal attach
path — never per-task steps and never pasted incantations. Because providers'
own consoles cannot usefully enumerate active logins, **flotilla's pool
bookkeeping is the operator's login inventory**.

### Corrections to recorded facts

- Codex ≥0.145 accepts `OPENAI_API_KEY`/`CODEX_API_KEY` ambiently **for
  non-interactive surfaces** (`codex exec`, `codex doctor`) — there the
  `login --with-api-key` transformation becomes optional. **The interactive
  TUI ignores the env keys** and uses whatever login the home holds, so for
  interactive crews (today's crews) seeded login material or the login
  transformation remains required (original text listed the transformation as
  required for all cases; the truth is this split). Corollary: two
  *interactive* crews on two *different* accounts in one vessel is the one
  narrow case that genuinely revives per-crew `CODEX_HOME`s.
- Consumer adapters gain the seeding duty with a hard acceptance test: a
  spawned crew reaches its brief with **zero interactive prompts**.

### Proxying is not precluded

"Material in vessel" and "endpoint plus proxy-held auth" are two
implementations of the same delivery interface; a future model-API proxy
(per-crew attribution at the proxy, no material in vessels) must slot in
without reshaping `CredentialSpec`.

## Amendment (2026-08-14): trusted crews use deliverable material proactively

The #1140 credential-health ruling narrows the migration rule that trusted
stance inherits ambient identity. When a consumer has a long-lived material
form that its adapter can deliver, trusted crews receive that material just as
contained crews do. In particular, Claude Code receives the operator's
subscription `claude setup-token` per process as `CLAUDE_CODE_OAUTH_TOKEN`;
the token supplies authentication while the trusted adapter continues to use
ambient Claude configuration for shared settings, skills, and MCP servers.

Ambient identity inheritance remains only for consumers with no deliverable
material form. A missing grant or unavailable material for a consumer with a
deliverable form is therefore a named preflight refusal, not permission to
start a crew and discover stale ambient authentication interactively.
