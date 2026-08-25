# Settlement review and claim evidence: review without a PR surface

**Status:** Accepted
**Date:** 2026-08-24
**Relates to:** ADR 0021 (convoy lifecycle settlement anchors — this ADR
adds the observed-digest anchor, amendment rides with its implementation
slice), ADR 0034 (decision-ledger shape for findings and Demands), issue
#1760 (the ruling), issue #1759 (the ghostty fork project that forces
this — a project with no PR surface at all).

Review today is PR-shaped: the reviewable artifact is the PR, findings
live in its threads, and "checks green + review clean" is read off the
forge before a merge event settles the convoy. The ghostty fork project
has none of that — its output is per-patch branches integrated into a
force-pushed integration branch, and there will never be a PR anywhere
by design. Rather than build a ghostty-only path, this ADR defines the
general review model, of which the PR surface is one projection (the
TUI factoring lesson: extract the surface-agnostic core).

## Decision

**1. The reviewable unit is the settlement claim's ref pair** — base..head
of whatever the convoy proposes to land. For a PR project that is
branch-vs-main and the PR is its projection; for ghostty it is the
candidate integration branch vs the upstream base. Per-patch review is a
lens the reviewer walks over the claim; the verdict attaches to the
claim as a whole. Re-review rounds may focus on the turn delta, but
approval always covers the whole claim.

**2. Evidence is part of the claim — no bundle, no admissible claim.**
Review rounds ping-pong as files inside the vessel (coder and reviewer
share a filesystem). On claim they aggregate into a two-layer **review
bundle**:

- a *machine-readable index*: rounds, findings and their resolutions,
  checks run against the ref pair — what the governor and the landing
  gate query;
- *human-facing artifacts*: HTML explainer, diff summary, screenshots —
  what a human opens (a web page first; later rendered inside wheelhouse
  via katzensteg/luchs).

The claim references the bundle by URL plus the digest of the claimed
head. If the branch moves after review, the claim is stale **by
construction**, not by convention. What the bundle must contain is set
per project by review-prep instructions in the project's ops docs.

**3. The human landing gate is binding, not procedural.** The *landing*
credential is distinct from the working credential and is staged to the
vessel only when the claim's HumanGate Demand is approved — the existing
grant / staged-credential-file / `held_credentials` machinery, no new
enforcement plane. Where branch-scoped credentials are awkward on a
forge, the fallback stays inside the model: no push credential at all
until approval. Plane-executed push (the daemon itself pushes exactly
the claimed digest) is the documented evolution, deliberately not built
now.

**4. No unanswered findings.** A claim is inadmissible while any finding
lacks a response; every finding reaches a terminal state — *addressed*
(with the fix) or *rejected-with-rationale* (the coder's recorded
pushback). Disagreement therefore cannot deadlock a convoy: it surfaces
as a *contested* claim at the human gate, finding and response side by
side. Same-vessel reviewer versus a separate review convoy is a pure
placement choice — the bundle protocol is identical.

**5. Settlement anchors on the claimed ref observed at the claimed
digest.** Claim names digest Y, approval authorizes Y, settlement is the
observation of the remote ref at Y. A later force-push away is a new
claim cycle, not un-settlement (as a revert does not un-merge a PR).
Settlement is thereby forge-agnostic: a PR merge becomes one observable
claim anchor among others, not the privileged one.

**6. Bundles live in S3-compatible object storage, per installation.**
The lab's installation is rustfs; R2 or any other S3-compatible endpoint
is equally valid, and off-lab replication is a mirror job — a claim must
never depend on a specific cloud provider, so an outage or an offline
laptop never blocks a landing. Keys are convoy-scoped
(`reviews/<project>/<convoy>/<claim-seq>/…`) with the index at a
well-known name and the human page beside it — browsable by a person,
integrity already guaranteed by the digest in the claim. Vessels write
via a scoped credential staged like any other. A disconnected
plain-files fallback is a noted future concern, deliberately not
designed now.

## Consequences

- Review evidence becomes queryable ("all findings resolved, checks
  green on this ref pair") without a forge round-trip, and survives the
  convoy that produced it.
- The landing gate is enforced by credential possession, not prompt
  compliance — the "never land unapproved" guarantee joins the ghostty
  "never touch upstream" guarantee as a structural property.
- PR-surface projects can migrate onto the same claim-plus-evidence
  model incrementally; nothing here forks the review model by surface.
- New moving parts to build: the bundle index schema and claim evidence
  field, the object-store integration, approval-conditioned credential
  staging, the observed-digest anchor, and the crew-side rounds-as-files
  protocol with its bundle aggregator.
