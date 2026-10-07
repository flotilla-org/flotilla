# Image builds

Explicit Docker compositions without a bound image join `ImageBuild` demand during
convoy admission. Admission accepts the convoy while its Environment reports
`Provisioning` and the current execution's queue, build, or failure reason. A
failed execution never selects an older image. Layered generation-1 baselines
retain their authoritative running image during the composition transition.

Each immutable execution contains one frozen layer, source content hashes, target
architecture, resolved arguments and pins, actual parent image ID, parent recipe
key, reservation, and reason with old/new inputs. Only the first stage is created
before its parent exists. Environment reconciliation advances the frozen
composition after each successful stage, so later keys contain the realised
parent rather than a predicted digest. Source commits identify acquisition;
content identifies the recipe. Unpinned inputs use a host-scoped execution nonce
and cannot claim fleet sharing. Authors opt into `input_stability: pinned` only
when every package/base input is pinned.

The placement host is the default builder (one slot, one CPU, 1 GiB disk
reservation). `Host.spec.image_build_capacity` can explicitly disable builds:

```yaml
image_build_capacity:
  kind: none
```

Or declare capacity:

```yaml
image_build_capacity:
  kind: builder
  architecture: amd64
  slots: 2
  reservation:
    cpu: 2
    disk_bytes: 8589934592
```

When placement cannot build, admission chooses a declared builder with the same
architecture. Architecture aliases are normalised; cross-architecture builds are
refused. Reservations are persisted before starting a process. Slots bound
concurrent executions without blocking admission or the controller watch loop.
Remote demands are projected into the selected daemon's authoritative store;
replicated actuator evidence takes precedence over demand-only records.

The daemon adapter acquires the full source revision through the typed VCS port,
checks content against frozen inputs, and runs Buildx with the default Docker
driver, `--load`, and a private Docker configuration. Dockerfile fragments consume
`ARG BASE` / `FROM ${BASE}`. Each declared provide must have an argv probe:

```yaml
provides: [harness:codex@0.160.0]
probes:
  harness:codex@0.160.0: [codex, --version]
```

Probes override the image entrypoint and run without network or image pulling.
Build output, probes, and failure reason are stored as a `build-log` Artifact
(retained for 30 days). The adapter recovers an existing execution tag without
rerunning Buildx, then verifies provides again. Transient failures create up to
three immutable successors after 30, 60, and 120 seconds; deterministic failures
never retry identical inputs. Failure evidence remains available on predecessors
and produces an infrastructure log and Host health condition.

A local successful build provisions the Environment using its image ID with
pulling disabled. A remote successful build waits for the distribution adapter to
deliver and verify its digest before provisioning (see Availability and distribution
below). Build garbage collection belongs to #2733.

Build args and pin values must be non-secret: they appear in Docker argv and may
appear in build output. Credentials require a separate secret-delivery mechanism;
never put them in these fields. Buildx has a 30-minute wall-clock deadline and
probes have a 60-second deadline. Timed-out probes are explicitly removed because
terminating the Docker CLI alone does not terminate the container. Probes use the
reservation's CPU count, a 512 MiB memory limit, and a 128-process limit. Disk
reservation remains a planning floor; portable Docker disk quotas are not implied.

Resource failure reasons are capped at 2,048 characters; full output remains in the
Artifact. Classification uses the final non-empty diagnostic line, recognises
explicit HTTP rate-limit errors, and treats a recipe process's own exit as
deterministic. Trailing BuildKit output can make that short status less informative;
consult the full Artifact for the preceding error summary. A `RUN` step reporting
`exit code: 137` with an outer Docker exit of 1 remains a deterministic recipe
failure under this policy: identical inputs are not automatically retried. An outer
Docker exit of 137 or a runner deadline is transient. Recipe OOM remediation
requires changing the recipe or builder capacity rather than expecting a retry.
Source hashes stream file contents on the blocking pool. VCS acquisition uses
`git archive` into `context/`; its bare object cache, archive file, and completion
marker are sibling entries outside the hashed Docker context.

Build logs are shared by executions rather than owned by one convoy. Their empty
`convoy` field deliberately excludes them from convoy-specific lists; unfiltered
artifact listing and expiry/pinning-based retention still apply. Recovery preserves
the first immutable execution log and emits a debug diagnostic for that reuse.

## Availability and distribution

Hosts publish full local image IDs and known registry manifest digests under
Host.status.capabilities.image_digests. Inventories are observations, refreshed
every 30 seconds independently of asynchronous publication; placement still requires host readiness. Completed
ImageBuild.status.availability records the hosts holding its exact local ID and
an optional repository@manifest-digest publication. Availability may change
after execution completes; identity, inputs, verification and build-log evidence
remain immutable.

A fleet can opt into a shared cache on its singleton FleetDesignation:

~~~yaml
spec:
  project: fleet
  image_cache:
    repository: registry.example/fleet/crew-images
    pull_credential: image-cache-pull
    push_credential: image-cache-push
~~~

Each referenced CredentialSpec uses the docker-registry adapter and an
operator-staged material source. Host permissions are separate from work grants:

~~~yaml
spec:
  selector:
    host_action:
      action: image-push
      hosts: [builder-host-resource-name]
  credentials: [image-cache-push]
~~~

Use image-pull for vessel hosts (and builders which consume remote parents).
An empty hosts set selects all eligible hosts; push additionally requires an
explicit, positive Host.spec.image_build_capacity declaration. A host-action
selector cannot contain work selectors or landing credentials and never matches
a crew, even when its work selectors would otherwise be empty. Work grants remain
the only way to deliver registry material to a crew.

Builders publish through a temporary digest-derived tag, retaining only the
registry's reported manifest digest as availability. Pulls use that manifest
digest, then inspect both the registry digest and local image ID. Missing grants,
missing publication, or mismatched IDs leave the Environment in Provisioning with
the reason. No older image or tag substitutes for the requested execution.

Without a registry, a destination uses its authenticated resource-mesh route to
request /image-transfer/sha256:<local-ID>, naming the source node and visited
nodes in the query. Direct source routes are preferred; sparse meshes forward
through other resource peers, exclude visited nodes, and refuse routes beyond
eight hops. Intermediaries copy byte chunks directly without archive spooling.
The source streams docker save to a
temporary archive and serves it as a binary HTTP body; the destination streams it
to a temporary archive and streams that file into docker load. This bounds
memory independently of image size and leaves process/transport seams injectable.
Archives and Docker configs are removed on success, errors and cancellation.
The destination inspects the immutable local ID before provisioning with pulling
disabled. Transfers run asynchronously so the Environment watch loop remains
responsive. This uses resource replication's transport, not Plane-A peer merging.

Placement prefers an exact held digest, then a published registry digest, then
build or mesh transfer, after host liveness. Completed pinned stages can be reused
across hosts; unpinned inputs never claim a shared recipe. A builder receives a
shared parent before building the next stage. Environment and Vessel status pin
the realised local ID and, when pulled, the registry digest; the admitted layer
revisions remain frozen.

FleetDesignation and CredentialGrant are authored by fleet-ops/project-map;
Host inventory and ImageBuild availability are daemon-authored. The new fields
are optional with previous-generation decoder defaults. Existing external
manifests remain valid; opting into the cache requires authoring the binding,
registry declarations and host grants together. No workflow or CI registry
credential is required.
