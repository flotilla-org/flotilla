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
pulling disabled. A remote successful build continues waiting for digest transfer;
registry publication, host inventory, transfer, and final identity freezing belong
to #2729. Build garbage collection belongs to #2733.
