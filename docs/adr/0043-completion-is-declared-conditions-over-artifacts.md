# 43. Completion is declared conditions over artifacts

Date: 2026-09-27

## Status

Accepted

Grilled 2026-09-27 (rulings on #2024 and #1652). Builds on ADR 0042 (artifacts) and ADR 0029 (the leaf engine). Amends ADR 0034 and ADR 0036.

## Context

Flotilla is meant to support many workflows: single-agent, in-crew review, multi-platform matrices, explainer stages, and ones not yet imagined. It is not meant to hard-code any one of them. The risk showed up when designing in-crew review. The natural next step was to teach core what a review round is, when a reviewer is done, and how many rounds to allow. Each such rule becomes a workflow-specific completion condition scattered through the codebase.

It has partly happened already. #1974's role-scoped completion expectations are a closed set of checks implemented in code (`decision-ledger`, `change-request-ready`). A template can only name them, not declare new ones.

The standing test from ADR 0008 and ADR 0028 applies: does a proposed step **widen or narrow** the extension? Declared data widens it; baked-in completion logic narrows it.

## Decision

### 1. Artifacts are a leaf subject

The leaf vocabulary (ADR 0029) gains an **artifact** subject, addressed by `(convoy, producer role, kind, subject)` (ADR 0042 §4). A leaf compares one field of that single record: its summary fields (e.g. `disposition`), or the fact that it exists. No collections, no joins, no counting, as ADR 0029 requires.

### 2. Completion and exits are declared in the workflow template

A workflow template declares, per role, the conditions under which that role's work is complete, and its exits, as leaves over artifacts and the existing subjects. Core evaluates declared conditions; it contains no workflow-specific completion logic.

For example, a review workflow might declare the reviewer complete when the `review-round` artifact by `reviewer` about the coder's head has `disposition == approve`. An explainer stage might declare completion when an `explainer` artifact about the head exists. Both are template data.

**#1974's closed expectation set migrates onto this mechanism.** The decision ledger becomes a `decision-ledger` artifact kind with a declared "exists" condition. "Change request ready" remains a condition over the change-request subject.

### 3. Staleness comes from the subject

A condition names the subject it's about, e.g. the coder's current head. The subject is bound when the condition is subscribed and re-derived when it moves (ADR 0029), so an approval of an older head stops satisfying the condition by construction, in every workflow, with no special code.

### 4. Loop policy stays in the caller

Round budgets, when to escalate, and what counts as contested stay in the caller: the crew brief, or a workflow harvested as a program (ADR 0027, ADR 0008). Core records artifacts and evaluates conditions; it doesn't run loops.

### 5. Producer identity is attribution, not authorization

The producer a condition names is stamped by the daemon (ADR 0042 §4). This is reliable attribution but **not a security boundary**: crews in one vessel share a container. For now the in-crew review loop exists to produce better code through an adversarial exchange, not independent assurance. The human remains the final gate. A workflow that needs independence expresses it as placement (a separate vessel), not as a core guarantee.

### 6. The review verdict is a flotilla artifact; the PR is a bridge

In-crew review produces `review-round` artifacts (and a bundle, ADR 0036) in flotilla's store. A GitHub PR, check run or comment is at most a **projection** of that verdict while PRs remain the landing surface. There is **no separate reviewer identity on the forge** (#1652): crew review informs the human who merges rather than satisfying forge review policy.

### 7. Turn delivery can key on artifacts

Turn-delivery rules (ADR 0029) may be leaves over artifacts. That gives workflows a forge-agnostic trigger that also works across vessels, since artifact envelopes replicate. Using this as the inter-crew transport for review workflows belongs to the workflow layer and is not ruled here.

## Consequences

- New workflows, and new stages within them, are added as template data, never as completion code.
- Staleness and head-binding hold uniformly across workflows.
- The hard-coded expectations from #1974 are removed from core.
- In-crew review no longer depends on GitHub review semantics, which it could never satisfy with a shared crew identity (#1652).

## Amendments

- **ADR 0034.** A ledger-less completion is *refused* (the code's behaviour since #1974), not "flagged, not rejected". The ledger becomes a `decision-ledger` artifact kind rather than only a PR-comment convention; a PR comment may remain its projection.
- **ADR 0036.** The review bundle is a `review-bundle` artifact (ADR 0042), written by the daemon rather than by a crew-staged credential (§6). "Same-vessel reviewer versus a separate review convoy is a pure placement choice" holds once inter-crew transport runs over artifacts (§7), which is not yet built.

## Deferred, with owners

- **The review workflow itself**: round artifacts, turn-delivery transport between coder and reviewer, round budgets in the brief. Parked with the involved-workflow work (#2075).
- **The explainer stage**: a design-strong agent writes a human-facing explainer for involved changes.
- **Retiring handoff as a load-bearing transport**, once artifact-keyed turn delivery exists.
