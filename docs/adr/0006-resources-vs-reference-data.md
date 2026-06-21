# Observed things split: resources vs reference data

Not everything Flotilla observes is a resource. The distinguishing cut is
**lifecycle ownership**, *not* where the thing physically lives:

- **Resources** — things with a Flotilla-owned/managed lifecycle or placement,
  whether they live locally or are provided by an external service:
  `Checkout`/`Clone`, `CloudAgent`, `TerminalSession`, `Environment`, `Host`
  (plus managed `Convoy`/`Task`). Observed instances are ephemeral per ADR 0004.
  - `CloudAgent`/`ManagedAgent` (never bare "Agent") is typically
    **service-provided** (claude.ai, cursor), reached via the service's API.
  - `Environment` may be host-local (host-direct, docker, sandbox-exec,
    firecracker) **or** service-provided (runpod, modal, aws).

- **Reference data** — service-scoped external records with no Flotilla-owned
  lifecycle, link-only, often numerous: `ChangeRequest` (PR), `Issue`. They are
  **bits of data linked to things**, scoped to an external service + repo. They
  live in a **reference/cache layer** (building on the existing issue cache and
  the stateless query service from #565/#568), fetched **per service+repo** with
  incremental sync (etag/since) and pagination. The **Aggregator links** them to
  resources for views.

- **External results** — a convoy's "main result" is frequently an artifact in an
  external service: a PR, a CMS article, a YouTube video. Producing one is an
  **action with an external result** that Flotilla *references*, not a resource it
  models.

## Reachability is a separate axis

Where a thing lives / how it is reached is **orthogonal** to whether it is a
resource:

- A resource may be reached **over the Tender** (host/environment-local) or via an
  **external service API** (service-provided `CloudAgent`, service-provided
  `Environment`).
- Reference data is always reached via an external service API.

So "needs the Tender" tracks *location*, not the resource/reference distinction.

## Open: the host / environment / location model

Most resources live **in an Environment**. A `Host` is/has a direct environment
and may contain nested environments; a service-provided environment may itself be
an ephemeral `Host`. This relationship is **not yet pinned down** and is tracked
as a separate design (see backlog), out of scope for the observer reshape.

## Why

- The cut that matters is lifecycle ownership vs link-only external record — not
  physical location. This avoids cramming lifecycle-less external records into the
  store and keeps the Aggregator's job as **linking** (what `AssociationKey`
  already did), cleanly separated from retired union-find *merging*.
- The dashboard (S1) still reads one *aggregated* view over two sources (resource
  store + reference cache).

## Consequences

- `ChangeRequestTracker` / `IssueProvider` become reference-data fetchers/cache,
  not resource contributors.
- #616 splits into: (i) migrate observed *resource* providers (checkouts, cloud
  agents, sessions) off `ProviderData`; (ii) wire the reference-data layer +
  Aggregator linking for PRs/issues; then delete the old
  `ProviderData → WorkItem → Snapshot` pipeline.
- #614's tracer (`Checkout`) is unaffected and remains the right first slice; an
  observed `Checkout` references the `Environment` it lives in (the host's native
  environment for the local-checkout tracer).
- A convoy's external result is a future "link/output reference" concept.
