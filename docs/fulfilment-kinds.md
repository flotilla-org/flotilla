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
baseline leaves that kind without current facts. Current terminal pools have no
configured vessel-slot ceiling, so an available pool reports unbounded capacity;
an unavailable pool reports zero free slots.

`flotilla fulfilment list`, `flotilla host list`, and the TUI fleet pane use
these same Host facts. They are observations, not admissions; the policy based
admission path remains active in A1.
