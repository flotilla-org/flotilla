# 41. External events arrive as hints through a dial-out relay

Date: 2026-09-26

## Status

Accepted. Amended 2026-09-27 (#2072, see Amendment below).

Amends ADR 0029 (resolves its deferred "Webhook refreshers" entry). Grilled on
#1680.

## Context

Forge state reaches flotilla by polling. The aggregator and the change-request
refresher re-read pull requests per convoy, and on 2026-09-26 that polling
exhausted the operator token's 5000/hour REST budget. That in turn stalled
convoy admission, which needs REST (#2045 is the interim mitigation). Polling
cost grows with the number of convoys, not with the number of changes.

ADR 0029 already names the answer: "push transports (webhooks) are a later
refresher, not a new engine," deferred as "a transport upgrade inside the
refresher." What it left open is how a push reaches a daemon at all:

- **Nothing can call in.** Flotilla runs in homelabs, behind NAT, on hosts that
  sleep (feta wakes on LAN, laptops close). If an intermediary could reach a
  home daemon, the forge could too, and no intermediary would be needed.
- **Homes disconnect.** Home follows the work: a laptop carrying convoys can
  leave the lab, and it still needs events for the convoys it carries.
- **It is not one lab's problem.** Other people should be able to install the
  same flotilla setup and sign up for event delivery.

Tender (PR #2015) does not solve this. It is a live, name-pinned byte-stream
forwarder with no payload buffering, and it discards routes on restart.
Durability of observations stays where ADR 0029 put it: the replicated resource
store.

## Decision

### The relay is a mailbox that homes dial out to

A hosted **relay** accepts events from producers and holds them per install.
Daemons **dial out** to it: a websocket, with long-poll as the fallback. Each
consumer reads from a **cursor** and acks as it goes. The relay never connects
to a home.

The relay keeps events for a bounded retention window. A consumer resuming
within the window continues from its cursor. A consumer whose cursor is older
than the window gets an explicit **gap**, and resynchronises by refreshing
everything it demands.

### Events are hints, not state

A producer's event is verified at the relay (per-source HMAC secret) and a
per-source **adapter** reduces it to a hint:

```
{ source, subject, kind, delivery_id }
```

`subject` uses the leaf engine's own address vocabulary (for example
`cr/github.com/flotilla-org/flotilla/2031`). The payload is discarded. A
consumer acting on a hint calls the existing refresher for that subject, which
re-reads the truth from the forge.

Consequences of hint semantics:

- Delivery is at-least-once and idempotent. A duplicate hint costs one extra
  read; two consumers acting on the same hint converge.
- The relay retains no forge content. For a hosted service carrying other
  people's events, "something about repository X changed" is the whole of what
  it holds.
- Adding a source means adding an adapter, not changing the protocol.

### One mailbox per install; the protocol is the contract

Each install gets its own mailbox (a Durable Object in the reference
implementation). **Install id, consumer credentials, and per-source secrets are
first-class from the start**, even while there is one install. A hosted
multi-tenant relay that others sign up to is the target shape; signup and
provisioning are not built yet.

The relay **protocol** is the contract, not the hosting:

- producers: `POST /i/<install>/<source>`, signature-verified with that
  source's secret;
- consumers: websocket (or long-poll) on `/i/<install>/stream?cursor=<c>`, with
  acks, and a `gap` response when the cursor is older than retention.

Self-deploying the reference relay, or writing an independent implementation of
the protocol, is supported.

### Every configured daemon connects; the observing authority acts

Every daemon with the relay configured holds its own connection to its install's
mailbox. Hints for an install are broadcast to its connected consumers. There is
no designated entry host, no failover, and no leader election.

A daemon acts on a hint only if it is the subject's **observing authority**
(`ChangeRequestSpec.observing_authority`). Anyone else drops the hint and
receives the refreshed record through replication. This enforces ADR 0029's
existing rule — "the first demanding host creates the record and runs its
refresher; everyone else reads replicas" — which the refresher must honour
whether a refresh is triggered by a hint or by its cadence.

### Ownership moves by staleness

A host with live demand for a subject **claims** observing authority when the
record's `observed-at` is older than the refresher's `stale_after` threshold,
with hysteresis (stale for more than one threshold) to prevent flapping. The
claim is an ordinary optimistic-concurrency write of `observing_authority`,
followed by a refresh. This covers a home that leaves the lab: its copies of
records owned elsewhere go stale, and it takes them over. Staleness is the
signal that the owner is unreachable. No connectivity detection is needed.

Contended ownership may later be surfaced to a human as a question (for example
in the wheelhouse) rather than resolved silently.

### Polling becomes the backstop

Hints are a refresher transport, as ADR 0029 anticipated. While a daemon's
relay connection is healthy, its refresher drops to a slow backstop cadence.
When the connection is down, or the relay reports a gap, the refresher returns
to its normal cadence and refreshes everything it demands.

### Reference implementation

A Cloudflare Worker with a Durable Object per install, written in Rust
(`workers-rs`) so protocol types are shared with the daemon, living in this
repository as a crate. Each install's forge webhook (for GitHub, the App's
webhook) points at that install's ingress. The webhook secret is a credential
held by the relay. Crews never hold it.

## Consequences

- Forge reads scale with changes, not with convoys. Polling remains only as a
  backstop against missed or expired hints.
- A home that was offline resumes from its cursor within the retention window,
  or resynchronises on a gap. Nothing is lost silently.
- Enforcing observing authority removes today's cross-host duplicate refreshes,
  independently of the relay.
- The hosted path adds a Cloudflare dependency and a new trust surface: hints,
  install credentials, and per-source secrets, but no forge content.
  Self-deployment remains available.
- The relay is general. It is not specific to GitHub or to this lab.

## Deferred, with owners

- **Issue subjects.** The leaf vocabulary is change-request-only. Issue events
  need an `Issue` subject kind and refresher before hints for them can be
  acted on (#1680 slice).
- **Multi-install signup and provisioning.** Designed for, not built.
- **Ownership contention as a human question.** Wheelhouse/attention work.
- **Infrastructure-as-config.** Deploying the hosted relay (and a self-hosted
  flotilla governor) likely belongs in a flotilla-ops repository.

## Amendment (2026-09-27): per-subject mailbox, per-install secrets

Ruled in #2072, after review of the first reference implementation (#2069).

### The mailbox coalesces per subject

The mailbox was a hint log capped at 256 hints and 24 hours. Hints are
invalidations, so that shape spent its capacity on repeats. One pull-request
push with a few CI jobs emits dozens of check events, so a handful of active
convoys forced every consumer into a gap and a full resync within the hour, and
a home that slept overnight always gapped. That is the polling cost the relay
exists to remove.

The mailbox keeps **the latest delivery per subject** instead. Each new hint
takes the next cursor and replaces its subject's entry. `read(cursor)` returns
the subjects whose latest cursor is newer than `cursor`, oldest first. A burst
on one subject occupies one entry, and a consumer resuming after a quiet period
gets each changed subject once.

Retention is by time (default 7 days, configurable) plus a per-install subject
cap. The relay records the newest cursor it has pruned (its **horizon**). A
consumer whose cursor is below the horizon may have missed a pruned subject and
gets `gap`, with the same meaning as before: refresh everything it demands, then
resume from `latest_cursor`. Delivery stays at-least-once. A redelivery of a
subject's latest delivery (same delivery id) is dropped.

In the reference implementation, each subject is a row in the install's
SQLite-backed Durable Object.

### Credentials live with their install; operators provision them

The first cut kept every install's consumer token and webhook secrets in one
Worker secret: one blast radius across tenants, and a redeploy per install.
Instead:

- Each install's per-source webhook secrets and consumer tokens are stored
  **with that install** (in the reference implementation, its Durable Object's
  storage). Consumer tokens are stored **hashed**. An install may hold several
  valid tokens, and several secrets per source, so rotation is add, switch,
  revoke, with no window in which nothing verifies.
- An **operator-authenticated admin API** creates and deletes installs, mints
  and revokes consumer tokens, and adds and revokes source secrets. The relay's
  only global secret is the operator credential, stored as a digest. It holds
  no tenant material.
- Requests for an unknown install fail exactly as requests with bad credentials
  do, so install ids cannot be enumerated.

This supersedes "signup and provisioning are not built yet" above:
provisioning is built, and **public signup remains deferred**.

### Subjects are case-normalized

GitHub owner and repository names are case-insensitive, but subjects are
compared as exact strings. For GitHub subjects the service and the
`owner/repo` scope are **ASCII-lowercased**. The rule is defined in
`flotilla-relay-protocol` (`Subject`). Consumers that key local state by
subject, such as the daemon's change-request record name, must apply the same
normalization (#2051).

### Review feedback wakes the refresher

`issue_comment` (on a pull request it produces a `cr/…` subject, on an issue an
`issue/…` subject), `pull_request_review_comment`, and
`pull_request_review_thread` are handled events, so review comments and thread
resolution trigger a change-request refresh (#1680).
