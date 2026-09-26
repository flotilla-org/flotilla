# 41. External events arrive as hints through a dial-out relay

Date: 2026-09-26

## Status

Accepted

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
