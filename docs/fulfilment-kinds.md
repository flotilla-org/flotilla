# Fulfilment kinds (ADR 0046 A1)

`FulfilmentKind` is a home-authored, home-bound runtime resource. Its owning
host publishes it and other hosts consume read-only replicas. This follows the
existing live `PlacementPolicy` authorship and avoids competing definitions of
one host's realisation. The crew image baseline remains a separate federated
Definition (ADR 0039).

During A1, each host registers kinds with the names of its live placement
policies. The three `docker-crew-image-*` manifests still author their policies;
the daemon projects them into kinds when they appear or change. Docker kinds
grant Linux and scoped network access. Host-direct kinds grant the host
platform, GUI session reach, host account reach, and host network access. Host
status says whether a GUI session is currently logged in. Operator-added
grants persist when a policy's image or pool changes. Prepared placement policy
snapshots remain frozen and are skipped by migration. Admission continues to
read policies until A2.

Malformed policies with both realisations are skipped during kind migration so
they cannot interrupt host heartbeats. Host-direct GUI observation reads display
variables on X11 and Wayland, and checks the user's launchd GUI domain on macOS.

Each Host status carries observations keyed by kind name. Host-direct probes
run through its injected command runner; Docker probes execute the binary in
the resolved image with `docker run --rm --pull=never`. Harness versions and
toolchain versions come from `--version`. Candidate Claude models are `sonnet`
and `opus`; a comma-separated `FLOTILLA_PROBE_MODELS` value supplied through
the injected environment replaces that list (an empty value disables model
probes). A one-turn, tool-free invocation records model
acceptance as `probe` evidence. A failed launch leaves the model unknown;
explicit CLI rejection records it unusable. Probes run on daemon start and
again when a Docker kind's resolved image changes. A missing or conflicted image
baseline leaves that kind without current facts. Each command has a 15-second
deadline, and each kind has a 45-second total deadline; a timed-out kind is
retried on the next observation. Current terminal pools have no
configured vessel-slot ceiling, so an available pool reports unbounded capacity;
an unavailable pool reports zero free slots.

`flotilla fulfilment list`, `flotilla host list`, and the TUI fleet pane use
these same Host facts. They are observations, not admissions; the policy based
admission path remains active in A1.

## Contained crew memory budgets

The Docker placement policy carries `docker_per_vessel.memory_policy`:

```yaml
memory_policy:
  host_memory_percent: 50
  expected_concurrent_crews: 4
  swap_bytes: 0
```

Every newly provisioned Docker crew gets a hard RAM limit of
`Docker MemTotal × host_memory_percent / 100 / expected_concurrent_crews`.
The default assigns one eighth of host RAM per crew and reserves half of the
host's RAM for services and interactive workloads at the expected concurrency.
This is a quota, not an admission reservation: operators must size expected
concurrency for the crews and other workloads they permit on that host.
`swap_bytes` is additional swap per crew. Its default disables swap; Docker's
`--memory-swap` receives RAM plus swap, not the swap amount alone.
Zero concurrency, percentages outside 1–100, budgets below Docker's 6 MiB
minimum, unknown capacity, and unsupported memory/swap enforcement refuse
provisioning. Existing containers retain their original limits until replaced.
Policy snapshots freeze the budget for admitted work, and registration preserves
operator-authored budgets.

Environment and Vessel status carry `runtime_observation`; Convoy status and
`convoy explain` retain per-vessel `environment_observations`. Observations include
configured limits, last successful cgroup RAM usage with its timestamp, and exit
code, inferred signal, Docker `OOMKilled`, cause and supporting journal evidence.
On Linux with the systemd cgroup v2 driver, RAM usage is sampled from
`memory.current`. Unavailable cgroups preserve the previous sample; missing
samples stay unknown. Exit 137 alone proves neither a kernel OOM nor an oomd
kill. Host attribution requires a full container ID match in the kill record,
inside the container's start-to-finish journal window. Journal access is best
effort; unavailable evidence leaves the kill cause unknown. Conventional exit
143 is classified as a normal SIGTERM stop, but a stop of backing that should be
Ready still fails the vessel. Dirty work remains protected by existing reclaim
checks, and the failure message lists retained checkout paths for recovery.

The operator acceptance check after deployment is to run a deliberately
memory-hungry crew, confirm it hits its own cgroup limit, and confirm
`convoy explain` reports the limits and `cgroup_oom` death cause. This check must
not run as part of unit tests or CI.
