# 48. Manifest reconciliation state lives on a ManifestRoot resource; annotations carry provenance only

Date: 2026-09-28

## Status

Accepted

Grilled 2026-09-28 on #2023 (rulings recorded there). Refines ADR 0001 and ADR 0024. Supersedes the refusal-annotation design from #1726.

## Context

The manifest reconciler (#1726) applies documents from a declared root (project-map, ops repositories) onto resources. It records **everything** as `flotilla.work/` annotations on each managed object:

- **Provenance:** `manifest-source`, `manifest-path`, `manifest-revision`, `manifest-reconciler-root`, `manifest-baseline-hash`.
- **Controller judgement:** `manifest-refusal` (the reason), plus `manifest-live-hash` and `manifest-desired-hash`.
- **Operator intent:** `manifest-suspend`, and `manifest-resolution` (a sync/adopt request that the reconciler consumes and clears).

That was chosen so every kind could show refusal state through generic get/list/watch without schema changes. But it departs from the k8s-isomorphic model (ADR 0001):
- Controller-derived state sits in untyped, clobberable metadata on objects the controller doesn't own.
- The read-modify-write of another object's annotations produced the TOCTOU races seen in #1726's review.
- There is a real prospect of running these kinds as actual Kubernetes CRDs. At that point k8s idiom stops being aesthetic and becomes interop, and "controller state in annotations" becomes a migration liability.

Manifest roots aren't resources today. They are a config-level notion that `reconcile-now` special-cases.

## Decision

### 1. Split by who authors the fact

- **Provenance stays in annotations on the managed object:** source, path, revision, reconciler root, and the baseline hash (last applied). The applier writes it with the object. This is k8s's own idiom (`kubectl.kubernetes.io/last-applied-configuration`).
- **Controller judgement moves to typed status on a new `ManifestRoot` resource.** Its status lists each document's state, written through the status path with compare-and-swap:
  - `applied`, `refused`, `drifted` or `suspended`;
  - the refusal reason;
  - live, desired and baseline hashes;
  - `observed_at`.
- **Suspension is operator intent,** so it moves to the ManifestRoot's **spec** as a per-document suspend set.

This mirrors Argo CD's `Application` and Flux's `Kustomization`: reconciliation state belongs on the thing that reconciles, not on every object it touches. Managed kinds need no schema change, and several of them have no status at all.

### 2. One-shot resolution requests are tokens, not set-and-clear flags

- **The request goes on the ManifestRoot's spec:** `resolutions: { <document>: { action: sync | adopt, token, requested_by } }`, written by `flotilla resource sync|adopt`.
- **The reconciler records completion in status** (`documents[<document>].resolved_token`, with the outcome and the time) and never edits spec.
- **Pending** means `spec.token != status.resolved_token`. That makes the protocol level-triggered and idempotent (at most once per token), which is the `kubectl rollout restart` pattern.
- `requested_by` is attribution, not authorization (ADR 0043).

### 3. Lifecycle

- **The daemon materialises one ManifestRoot per root its host declares in config.** Its **home is the host whose checkout it reads.** It replicates read-only, so every host and surface can show refusals.
- **Document identity** is the root-relative path plus the declared `(kind, namespace, name)`. A moved or renamed document is a new key, and a key whose object is no longer authored drops out.
- **Removing a root doesn't delete what it applied.** Objects keep their provenance and become unmanaged. Prune policy is a separate, explicit decision.

### 4. Stalls

A refused or undecodable document is a **controller-maker terminal row** on its ManifestRoot (ADR 0045 §4). It raises `Stalled` there and reaches "Needs you", naming the file, field and reason. This subsumes #2147 and #2057's surfacing step.

### 5. The general rule

For every resource kind:
- **Annotations carry applier-owned metadata and provenance only.** That means what the author writes alongside the object.
- **Controller-derived state lives in typed status on the resource that owns the reconciliation,** written via the status path with compare-and-swap. If no such resource exists, that is a signal one should (as here).
- **One-shot requests are spec tokens,** matched by an observed or resolved token in status, never set-and-clear flags.

## Consequences

- Refusal state is typed, race-free, CRD-compatible, and visible from any host.
- The #1726 TOCTOU class disappears, because the reconciler no longer mutates other objects' metadata.
- Manifest failures become stall conditions with a clear home, instead of only WARN lines.
- **Cost:**
  - a new resource kind;
  - migrating the refusal, hash, suspend and resolution annotations onto it. Under ADR 0047 that means one roll where the old annotations are read and ignored, and stale ones are cleaned from managed objects.
- `reconcile-now manifest-root` becomes an ordinary resource-addressed operation.
