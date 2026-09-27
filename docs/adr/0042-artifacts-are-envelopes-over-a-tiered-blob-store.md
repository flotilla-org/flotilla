# 42. Artifacts are envelopes over a tiered blob store

Date: 2026-09-27

## Status

Accepted

Resolves #748. Grilled 2026-09-27 (rulings recorded on #748, #1652 and #2024). Amends ADR 0036 §6.

## Context

Flotilla produces things that are neither resources nor repository content: briefs, review rounds and bundles, decision ledgers, explainers for human reviewers, test reports, and eventually crew recordings. Today each has an improvised home:

- **Briefs** are stamped into checkouts, because the repository is the only durable store available (#746).
- **Review bundles** (ADR 0036) are written to a per-installation S3 bucket by the crew itself, using a scoped credential staged into the vessel (`review-bundle-writer`).
- **Decision ledgers** are PR comments (ADR 0034).

Two constraints shape the answer:

- **The resource store replicates.** Bodies such as review rounds must not be copied around the fleet. Only something small enough to replicate belongs there.
- **Homes disconnect.** Laptops leave the lab and hosts sleep. Creating work must never fail because a store is unreachable (the ADR 0015 stance), and a single-machine fleet must work with no fleet store at all.

## Decision

### 1. A blob store seam, in tiers

Blob bodies live behind one **BlobStore** seam with pluggable backends:

1. **Local**: a content-addressed directory on every host. Always present, and sufficient on its own for a one-machine fleet.
2. **Bring-your-own S3-compatible**: self-hosted, private or internet-facing. The lab's store is this tier, and it's the only fleet store for now.
3. **Cloud** (later): a store included with a flotilla.work account.

An installation may configure more than one fleet store. Blobs are addressed by content digest, so every copy is identical and immutable: union is the whole merge, with no conflict semantics.

### 2. The daemon does all store I/O

Crews never hold store credentials. A crew hands a file to its daemon across the vessel walls (`flotilla artifact put`), and the daemon writes it. Reads also go through the daemon (`flotilla artifact get`), which materialises the body into the vessel. This keeps store access on the hull side of the walls (ADR 0010) and retires crew-staged store credentials such as `review-bundle-writer` (amending ADR 0036 §6).

### 3. Writes are local-first; reads are lazy by digest

- A write lands in the host's local store immediately and never fails on fleet-store availability. A background sync uploads to the configured fleet store(s) when reachable.
- A read looks up the digest in the local store, then the fleet store(s), and caches what it fetches.

**Durability rule:** a blob outlives its host only if it reached a fleet store. A local-only blob on a dead disk is lost, and the host's inventory says so (ADR 0016).

### 4. `Artifact` is a resource kind: the envelope

Every such thing is described by one resource kind, `Artifact`, rather than by blob-reference fields on the resources that produce or use it. An `Artifact` carries:

- `kind`: an open vocabulary (`brief`, `review-round`, `review-bundle`, `decision-ledger`, `explainer`, `recording`, `test-report`, …);
- `producer`: stamped by the daemon from the calling crew session, never supplied by the CLI. This is **attribution, not an authorization boundary** (see #2024): crews sharing a vessel share a container, so it identifies reliably but doesn't secure;
- `subject`: the ref the artifact is *about* (a commit, tree, claim, or convoy);
- an **owner link** to the convoy (see §5), for discovery;
- a small **summary** that conditions can read (e.g. `disposition`, `status`);
- the body's **digest**, size and media type.

An artifact is addressable by `(convoy, producer role, kind, subject)`, so a single-record leaf (ADR 0029) can read it (ADR 0043). Envelopes replicate as **home-bound runtime**, following their convoy's home. **Bodies never enter the resource store.**

### 5. Artifacts outlive their convoy; retention is per kind

- The owner link is a **non-controller** reference. Reaping a convoy doesn't cascade-delete its artifacts, which keeps ADR 0016's promise that death leaves readable memory.
- **Retention is declared per kind, as data.** Small, high-value kinds (brief, decision ledger, review round, explainer) are kept long. Bulky, low-value kinds (recordings, raw test output) expire soon. An operator can pin an artifact to keep it.
- **Blob GC is by reference.** A blob that no envelope references is deleted after a grace period, independently on each backend.

### 6. Briefs are the first consumer

Admission writes the convoy's brief as a `brief` artifact instead of stamping it into the checkout. Re-provisioning on another host fetches it by digest (resolving #746's open question).

## Consequences

- One mechanism, one CLI surface and one retention story replace per-artifact improvisation. New artifact types are data (a new `kind`), not code.
- The resource store carries only small envelopes, so replication stays cheap however large the bodies get.
- Store credentials live only on daemons. Contained crews gain no network or credential surface.
- Offline and single-machine fleets work with the local backend alone; adding the lab store later only adds a sync target.
- Review evidence, ledgers and explainers survive the convoys that produced them, and can be queried without a forge round-trip.

## Deferred, with owners

- **Peer fetch.** Reading a blob from its producing host when no fleet store has it (over the peer channel / Tender). Not needed while the lab store is the only tier.
- **Cloud tier and archive tier.** Eventually only the *hot set* of envelopes replicates, and older artifacts move to archived storage rather than every envelope replicating everywhere forever.
- **A cut-down flotillad as a Cloudflare Worker** (or similar) that receives just enough replication to make sense of what it holds, beside the cloud tier and the relay (ADR 0041).
- **Searchability** of artifacts (carried from #446 via #748).
