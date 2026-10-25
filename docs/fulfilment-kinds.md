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

Each Host status carries observations keyed by kind name. Independent units
observe image identity/presence from the provider's local image inventory,
harness and toolchain versions from isolated `--version` invocations, GUI
session state from display variables or launchd, and capacity from discovered
terminal pools. Command failure or missing detection leaves that observation
unknown without suppressing unrelated successful observations.

Detection belongs to the provider. Local and agentless SSH observations use
the injected endpoint; image observations run in the resolved image without
pulling or network access. Detection requires target Bash for owned process
groups; missing Bash leaves detection unknown. Startup files, exported shell
functions and shell options are disabled before the observation wrapper runs.
Harness invocations have disposable HOME, explicit
harness and XDG config/cache/state directories, an empty authentication
environment, and a CWD outside operator/project configuration. Bounded cleanup
runs after success, failure, or caller cancellation. Each detection operation
has a 12-second execution deadline and a two-second cleanup deadline inside
the observation's 15-second limit. Kinds retain their 45-second pass deadline.

Account/model entitlement remains **unknown**. Observation and OAuth credential
preparation never send model turns. A selected model may be attempted with the
credential selected by its scoped grant, subject to ordinary harness/version,
capability and placement checks. Neither grant presence nor a successful
version check proves model access. Retired request caches remain decodable;
neither positive nor negative old model observations affect admission, and
cached observations lose their model facts before being published again.
Real, recognized standalone launch authentication diagnostics use the existing
adapter/lifecycle failure channel with fixed bounded messages. Unrecognized
model errors, quoted diagnostics, conversation screens and ambiguous exits
remain resumable; they are not evidence of a confirmed refusal.

Current terminal pools have no configured vessel-slot ceiling: an available
pool reports unbounded capacity and an unavailable pool reports zero slots.

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
`memory.current`. Unchanged usage retains its original sample timestamp and does
not trigger status writes; changed usage or other evidence does. Unavailable
cgroups preserve the previous sample; missing
samples stay unknown. Exit 137 alone proves neither a kernel OOM nor an oomd
kill. Host attribution requires a full container ID match in the kill record,
inside the container's start-to-finish journal window. Journal access is best
effort; unavailable evidence leaves the kill cause unknown. Conventional exit
143 is classified as a normal SIGTERM stop (including external SIGTERM; inspect
cannot identify who sent it), but a stop of backing that should be
Ready still fails the vessel. Dirty work remains protected by existing reclaim
checks, and the failure message lists retained checkout paths for recovery.

The operator acceptance check after deployment is to run a deliberately
memory-hungry crew, confirm it hits its own cgroup limit, and confirm
`convoy explain` reports the limits and `cgroup_oom` death cause. This check must
not run as part of unit tests or CI.
