# Verified guest build lookup

The query is one complete native tool set by OCI guest platform, before connection
or delivery. The admitted matrix has one guest target and two controller targets;
this is independent of convoy history or resource counts. The catalog and payload
live locally in the already verified immutable controller generation. There are
zero durable-resource-store reads and zero network requests in lookup.

The complete payload is verified once per daemon process, then shared as paths.
Retaining all binary bytes per vessel would multiply payload size by vessel
count; rehashing the same installed build on every provisioning attempt would
multiply disk bandwidth by the workload's peak 10 admissions/s. A process-local
OnceLock avoids both costs. It holds one path set rather than one per vessel or
history item, and also keeps a missing-build refusal stable until the next daemon
restart. Public explicit-generation resolution remains uncached for incoming
post-install validation and isolated fixtures. The immutable-installed-generation
assumption expires if in-place modification is admitted; that would require a
new identity/invalidation strategy before keeping this cache.

The initial validation reads one file at a time (at most 128 MiB), with a 16,384
entry inventory bound and depth limit 16. Catalog maps are ordered for deterministic
validation; there is one target, so a general runtime plugin registry would add
no useful query capability. Source pins and run identity are shared catalog
facts, not repeated per file. Candidate archives retain the existing 1 GiB
expanded limit. Provisioning has no new retry pool or background synchronization:
providers keep their existing lifecycle ownership, and unsupported artifacts fail
before guest delivery. Transient socket hello failures retain the existing
bounded retry; a platform mismatch is final.

Catalog schema 1 is an additive release payload. Consumers accept archives without
it for previous-generation validation/rollback but runtime lookup refuses missing
builds. Producers emit the complete new shape. No resource kind, generated table,
resource schema or external resource manifest changes. New targets require an
explicit matrix expansion and verified existing native build, not controller-byte
fallback. The catalog's source set reuses the existing candidate pins; cohort
identity additionally binds the Actions run. Promotion is the boundary where the
verified Linux output is copied into the unsigned Darwin archive before signing.
