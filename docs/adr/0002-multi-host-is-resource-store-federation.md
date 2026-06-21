# Multi-host is resource-store federation, not peer-merge

**Status:** Accepted
**Date:** 2026-06-21

Multi-host coordination is delivered by **federating the resource store**, not by
a bespoke peer snapshot/query-state merge protocol.

- A slim, standalone, **per-host Tender** (the federation daemon) owns host
  inventory, identity, pluggable transport (ssh control-masters now; wireguard/https
  later), and **arbitrary UDS-listener forwarding**, exposed via its own
  HTTP-over-UDS control API. It carries no flotilla domain semantics. The whole
  product family (cleat, porthole, uishell, flotilla) uses it, so remoting is
  ambient: a service dials a local socket and the Tender forwards it.
- "See host B's work" becomes **"watch host B's resource store over a forwarded
  UDS"** — there is no peer-merge protocol.
- The control plane sits *above* the Tender: it models `Host` and forward-rules as
  resources and **actuates** the Tender through its control API (a federation
  actuator, same pattern as the docker/checkout actuators). Federation depends on
  nothing flotilla-specific; the control plane depends on federation.
- Any **merging/aggregation** across hosts is a **view/aggregator** concern,
  performed on demand where needed — not a core always-on service.
- Large installations are expected to use **real Kubernetes** as the backing
  store, where "one resource controller" is a whole cluster (later).

## Why

The current peering (~5,400 LOC in `flotilla-daemon/src/peer`: `manager.rs`,
`merge.rs`, the peer wire protocol, ssh/channel transports, and their tests)
entangles a generic transport with flotilla's own snapshot-merge semantics.
Splitting out a generic Tender and treating multi-host as store-federation lets
most of the peer-merge layer be deleted, leaves a much smaller flotilla on top of
a shared substrate, and gives the rest of the product family the same remoting
model.

## The event log is the replication primitive

The resource store's watch stream (ADR 0001's `resourceVersion` +
watch-from-version semantics) *is* a kafka-like log. This reframes "federation" as
**remote access to that log**, and makes replication fall out for free:

- A **materialized replica** of another host/cluster is just "tail its resource
  log from a version and apply" — the same operation a local watcher already does.
- **Aggregators** (the demoted form of correlation, ADR 0003) are **pure
  functions over one or more logs** — ephemeral, rebuildable, multiple view
  configs (per-host, per-agent), no merged state persisted.
- This is the concrete realization of the long-standing "kafka-like log" multi-host
  direction (issue #256): each source writes its own scoped log; replication is
  outsourceable to a real log substrate later; the double-merge bug class is
  eliminated by construction because you never feed merged state back into a
  source log.

The **Tender** carries the log between hosts; it does not interpret or merge it.

## Consequences

- The bespoke peer snapshot/query-state merge subsystem is slated for removal once
  resource-store federation is in place. This is the deletion the decision pays
  for.
- The Tender becomes a new always-on per-host process (small, single-purpose).
- A federation actuator/controller is added to the control plane to drive the
  Tender from `Host`/forward-rule resources.
