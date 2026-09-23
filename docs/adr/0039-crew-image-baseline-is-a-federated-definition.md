# 39. Crew-image baseline is a federated Definition

Date: 2026-09-23

## Status

Accepted

## Context

The three `docker-crew-image-<host>` placement policies are home-bound runtime
resources. Embedding the fleet image tag in each policy made a fleet image
bump three independent edits. ADR 0033's merged decision reads remove read
inconsistency but cannot remove this authoring drift.

## Decision

A named, namespace-scoped `CrewImageBaseline` is a Definitions-class resource
under ADR 0016: edit anywhere, federate, and causally merge. Its `spec.image`
is the explicit crew-image tag. The fleet uses one baseline, `fleet-crew`.
Each host's PlacementPolicy references it through
`docker_per_vessel.image: { image_baseline_ref: fleet-crew }`. The policy
retains host selection, pull policy, adapter declarations, checkout layout,
and environment configuration. Placements with independent image lifecycles
may still name a literal image string.

This amends ADR 0007's resolver-side image configuration with a referenced
baseline. Images remain outside WorkflowTemplate and VesselRequirement.
ADR 0010's hull/crew boundary stays intact: the image supplies hull contents;
credentials, identity, stance, and brief remain crew provisioning concerns.
ADR 0026's fleet pin-set still controls binaries and wire generation; this
runner-side image mapping is deliberately a separate Definition.

Admission resolves the baseline through the Definition merge view, including
replicas. It freezes the concrete tag into the prepared placement snapshot.
A later baseline edit affects newly admitted work, not that snapshot or an
existing Environment. Direct vessel provisioning also resolves references
before emitting any Environment creation. Missing, deleted, empty, or
merge-conflicted baselines fail with `image-baseline <name> missing/unresolved`
before any Docker pull; no default image or merge winner is substituted.

## Consequences

A fleet image bump is one baseline edit, propagated by existing resource-store
replication. Per-host policies are configured once, on their own homes.
Federation is eventually consistent: during a partition, a host can still
admit against its last observed baseline. This removes independently authored
per-host pins, not network delay. Concurrent incompatible baseline edits block
admission until an operator resolves the Definition conflict.

Rollout is provider-before-config: deploy support to every host, apply the
baseline once, verify its replication, then replace each home's literal crew
policy with the reference. Subsequent bumps need no per-host policy applies.
Registry image construction and digest recording remain existing runner
responsibilities. This does not introduce image recipe resolution or rebuilds.
