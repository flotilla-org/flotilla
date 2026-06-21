# k8s-isomorphic resource model; k8s is a backend, not the API contract

**Status:** Accepted
**Date:** 2026-06-21

The control plane's typed resources are deliberately **isomorphic to Kubernetes
objects** — every resource has a lossless k8s-object representation (`apiVersion`,
`kind`, k8s-style `metadata` including `resourceVersion`, `ownerReferences`,
`finalizers`, `deletionTimestamp`, `spec`, `status`) and k8s-style semantics
(optimistic concurrency, list + watch-from-version, status subresource).

We split this into two capabilities that were being conflated (these **A**/**B**
labels are local to this ADR and unrelated to *Plane A* / *Plane B* in
`docs/roadmap.md`):

- **B — always maintain a k8s representation of every resource: yes, now.** The
  canonical k8s-object projection is a property of the resource *model*, not of
  any one backend. (Today the envelope mapping lives privately in the HTTP
  backend as `WireResource<T>`; the decision is to lift it into the model so
  in-memory, sqlite, and k8s backends all share one envelope.) This is low-regret
  because the typed model is already k8s-isomorphic in everything but field
  casing, and it makes a non-k8s backing store (sqlite/libsql) a near-mechanical
  port of the in-memory backend's version/watch semantics.

- **A — present a k8s-compatible API edge (external clients kubectl/REST against
  us): deferred.** This is a larger commitment (honouring the k8s *server*
  contract: selectors, apply, watch bookmarks, possibly admission). With B always
  true it stays a cheap optional adapter, built only when a real consumer needs
  it (e.g. federating to a real cluster, or being faked as one).

**Source of truth:** typed Rust structs are the spine; the k8s object is a
guaranteed projection derived from them — not the reverse. This keeps type safety
end-to-end and avoids resources degrading into opaque JSON blobs.

## Why

k8s semantics (resourceVersion / optimistic concurrency / watch) are the valuable,
hard-won, already-proven part, and they are exactly what makes alternative backing
stores tractable. k8s *wire ceremony and server contract* are the parts that lock
us in — so we keep the former always-on and treat the latter as an optional edge.
A bonus: k8s resources are heavily represented in model training data, which suits
the goal of small, coherent pieces agents already understand.

## Consequences

- Every resource we ever define must remain expressible as a k8s object
  (group/version/kind + structural-ish spec/status). We have already adopted this
  shape, so the marginal cost is ~zero.
- k8s is one backend among several (in-memory for tests, sqlite/libsql for
  embedded durability, real k8s REST for cluster-backed). The public
  `TypedResolver` API stays backend-agnostic.
- "Federate to k8s / fake being k8s" survives as future adapter work, not a
  present constraint.
