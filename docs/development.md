# Development

For the post-r591 provider and label cleanup, see the
[compatibility retirement acceptance checks](compatibility-retirement-acceptance.md).

## Daemon logs

The daemon's canonical structured log is
`$XDG_STATE_HOME/flotilla/log/flotillad.jsonl`, or
`~/.local/state/flotilla/log/flotillad.jsonl` when `XDG_STATE_HOME` is unset.
Older size-rotated generations use numeric suffixes such as
`flotillad.jsonl.1` through `flotillad.jsonl.4` with the default configuration.

Stop a running daemon with `flotilla daemon stop`. The command is idempotent
when no daemon socket exists and asks a live daemon to finish active requests,
retire peer connections, close its socket, and exit cleanly. Deploy and install
scripts may replace the CLI first, then use this command before replacing the
daemon: shutdown is the one typed RPC admitted between different builds when
both speak the same `PROTOCOL_VERSION`, and it waits for acknowledgement before
reporting success. All other RPCs still require matching build identities. A
cross-protocol stop is refused with a protocol-version mismatch, so deploys
that cross a protocol bump must retain the old CLI until its daemon has stopped.
Do not use `pkill` for daemon restarts.

For example, show warnings from the peer subsystem with:

```bash
jq -c 'select(.level == "WARN" and (.target | startswith("flotilla_daemon::peer")))' \
  ~/.local/state/flotilla/log/flotillad.jsonl
```

Detached daemons write their normal tracing output only to this JSON-lines
log. A legacy `~/.local/state/flotilla/daemon.log`, when present, contains only
whatever stdout or stderr the host's launch command redirected there, such as
startup failures or panics; it is not the structured daemon log and may be
stale. The current built-in launcher sends that same pre-tracing and panic tail
to `~/.config/flotilla/daemon-panic.log` instead.

## Docker environment recovery

Run one authoritative Flotilla daemon per Docker endpoint. Environment container
labels identify the Environment, not the daemon or config directory. Separate
development or test daemons must use a separate Docker endpoint; different
Flotilla config directories alone do not isolate container ownership.

After startup restoration, the daemon inventories all containers labelled
`flotilla.environment` every 60 seconds. This includes legacy containers without
an owner label. A container absent from the local authoritative Environment store
for 10 minutes of successful observations is reclaimed by immutable container
ID. Failed inventory or store reads reset the grace period. Existing records,
including quarantined records, prevent reclamation.

Inventory failures appear every retry in the canonical structured log with
`orphan environment sweep failed; will retry`, the host identity, and the error.
A malformed inventory line stops the entire pass and resets the grace period;
inspect those warnings when reclamation does not progress.

## Cargo target cache policy

A checkout's `target/` is managed cache state. The fleet uses two complementary controls: a daily, per-host mtime-based sweep for old Cargo artifact families and a per-checkout size-cap backstop for unusually large targets.

### Daily mtime sweep and size caps

Install and immediately verify the schedule on each fleet host from a Flotilla checkout:

```bash
scripts/install-cargo-sweep-schedule.sh
```

The installer pins `cargo-sweep` 0.8.0 when the command is absent, copies the runner, `prune-target.sh`, and their shared cargo-sweep compatibility helper to `~/.local/libexec/flotilla/`, installs a systemd user timer on Linux or the checked-in launchd agent on macOS, enables the daily schedule, and starts one observed run. The installed policy runs `cargo-sweep --time 3` once a day. After that mtime-based three-day retention step, it applies `prune-target.sh` to the same root: oldest incremental generations are capped at 10 GiB and the complete target at 20 GiB. Re-run the installer to update existing schedules.

Each run sweeps:

- every immediate directory under `~/dev/` that has its own `target/`; and
- non-convoy checkouts with a `target/` beneath `~/dev/flotilla-repos`.

Both scheduled and manual pruning skip roots with a `convoy-*` ancestor after resolving symlinks. Checkout provisioning maintains this reserved directory convention (`<repository root>/<convoy name>/<branch>[/<repository>]`), so it serves as the ownership marker without depending on daemon availability. Live, terminal, and orphaned convoy checkouts are left to lifecycle teardown and GC. This also covers nested multi-repository checkouts and convoy roots outside the default repository directory.

The schedule explicitly operates on each discovered checkout's `target/`, overriding any custom `build.target-dir` setting; custom target locations are outside its discovery scope.

The runner records reclaimed bytes separately for the mtime and size-cap steps for every root, plus their combined total for the whole run, in:

```text
~/.local/state/flotilla/cargo-sweep-mtime.log
```

Reclaimed-byte figures use before/after disk usage and are approximate if builds run concurrently.

Targets that remain over either cap log a warning (for example, artifacts that cargo-sweep cannot remove). Before touching a non-convoy target, each policy validates its manifest with `cargo metadata --no-deps --locked`. This preflight cannot create or update a lockfile. Root-resolution failures also refuse cleanup. Metadata failures log Cargo's reason on one line and skip that root without failing the job; other roots continue. Cleanup command failures are logged and make the scheduled run fail, while other roots are still processed. The same `FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE` and `FLOTILLA_TARGET_MAX_SIZE` overrides apply to both scheduled and manual pruning when supplied in their environment.

Inspect the scheduler and the most recent result with:

```bash
# Linux
systemctl --user status flotilla-cargo-sweep-mtime.timer
systemctl --user status flotilla-cargo-sweep-mtime.service
tail -n 50 ~/.local/state/flotilla/cargo-sweep-mtime.log

# macOS
launchctl print "gui/$UID/org.flotilla.cargo-sweep-mtime"
tail -n 50 ~/.local/state/flotilla/cargo-sweep-mtime.log
```

Run the policy regression tests with `scripts/test-prune-target.sh` and `scripts/test-cargo-sweep-mtime.sh`; they require Cargo and provision cargo-sweep 0.8.0 in their temporary test directory when the command is absent. Offline hosts must have that binary installed before running these tests.

An identity-based artifact policy remains a candidate for future evaluation; it is not part of the installed policy.

### Incremental compilation split

Interactive desk builds keep Cargo's incremental compilation enabled because it materially shortens the edit-build loop. Crew vessel provisioning exports `CARGO_INCREMENTAL=0`, so short-lived crew builds do not mint incremental generations. GitHub Actions also sets `CARGO_INCREMENTAL=0` for the same short-lived-build reason.

To confirm the setting in a dispatched crew vessel, build once and check both the environment and target:

```bash
test "$CARGO_INCREMENTAL" = 0
cargo check --locked
test -z "$(find target -path '*/incremental/*' -print -quit)"
```

Current Cargo may create the empty `target/debug/incremental/` container even when incremental compilation is disabled; the verification checks that it contains no generated state.

### Development debuginfo

The workspace `[profile.dev]` defaults to `debug = "line-tables-only"` on desks, crews and CI. Test builds inherit this default.

Contained `rust-build-limits` tools export `CARGO_PROFILE_DEV_DEBUG=line-tables-only` and prepend a staged Cargo shim to `PATH`. The shim supplies `--config 'profile.dev.package."*".debug=0'`, so workspace crates retain file and line information in backtraces while third-party dependencies emit no debuginfo. CI supplies the same Cargo dependency override and workspace environment setting. Test builds inherit the dev profile. The override targets dev; test and custom profiles inheriting dev inherit these defaults. Release, bench, and independently configured custom profiles retain their existing Cargo settings.

For a build requiring workspace variables and types in a debugger, use `CARGO_PROFILE_DEV_DEBUG=full cargo test --workspace --locked`. This explicit override wins over the workspace profile and the tool environment's default. In crews and CI, dependency debuginfo remains disabled. Desk builds use the same line-table default and can use the same explicit full-debuginfo override. The shim forwards `+toolchain` selectors and other arguments to the next Cargo executable on `PATH`, preserving repository Cargo config and Rust flags.

### Executable build identity

The root build script generates `FLOTILLA_BUILD_ID` for `flotilla` and `flotillad`; both inject it into libraries before parsing CLI arguments or starting tasks. Fleet and candidate builds can still set `FLOTILLA_BUILD_ID` explicitly. Otherwise it includes the Git revision and a fingerprint of workspace sources, including dirty changes. Library-only embeddings report `unknown` unless they call `flotilla_core::build_info::initialize_build_id`. Repeating the same identity is safe; empty or conflicting identities are rejected. This diagnostic identity does not gate compatibility: the protocol-source fingerprint still controls client and peer admission and `fleet check`.

### Size-cap backstop

The daily host job applies age-based removal followed by the size caps. `scripts/prune-target.sh` also caps a single checkout on demand: it removes the oldest incremental generations until their total is at most 10 GiB, then asks `cargo-sweep` to reduce the complete target to at most 20 GiB.

Preview the size decisions, then apply them when no build or test is running:

```bash
scripts/prune-target.sh --dry-run
scripts/prune-target.sh
```

Preview mode runs the complete size-cap policy against a temporary hard-linked copy beside the target directory. This lets the complete-target decision observe the simulated incremental removals without changing the real target; the temporary copy is removed before the command exits.

Both ceilings can be overridden for an exceptional checkout:

```bash
FLOTILLA_TARGET_INCREMENTAL_MAX_SIZE=15GiB \
  FLOTILLA_TARGET_MAX_SIZE=30GiB \
  scripts/prune-target.sh
```

Use `scripts/prune-target.sh --root /path/to/checkout` to cap another checkout; the installed runner uses this explicit root so it does not depend on a source checkout beside the installed scripts. With Cargo's default configuration the script acts only on that checkout's `target/`. When `CARGO_TARGET_DIR` is set, the command intentionally honors it. Relative values are anchored to this checkout's root, so use an absolute value if Cargo is normally invoked elsewhere.

### CI cache decision

GitHub Actions keeps target caches because compiled dependencies are expensive and reusable across runs with the same lockfile. Compiler incremental state is disabled with `CARGO_INCREMENTAL=0`; every target-caching job removes restored incremental directories before the cache post-action saves a new entry, so incremental generations are neither restored nor re-uploaded.

## Crew idle supervision

Actor obligations require three minutes of continuously confirmed idle before
mechanical supervision sends a nudge. A workflow can configure this per crew row:

```yaml
stall_nudges:
  work/coder:
    max_per_episode: 2
    idle_grace_seconds: 180
```

The historical `max_per_episode` field now bounds the unmet actor obligation,
identified by its maker and leaves. Able Working attention clears the visible stall
but preserves durable accounting in convoy status. A changed observed checkout
HEAD or pushed revision, a settlement claim (including a refused claim), or a
crew stall declaration resets the corresponding budget. Unknown or unchanged
revision evidence does not. Nudges back off by the grace interval, then twice
that interval, and so on. Exhausting the budget enters the supervision ladder
only after the next backoff has elapsed.

Claude Code's `Stop` remains a candidate `Idle` observation: it ends a response,
not necessarily the assigned work. It is weaker evidence of inability than a
sustained quiet period; a fresh Stop restarts grace rather than bypassing it.
`PreToolUse` and `PostToolUse` report Working, and fresh screen confirmations
establish quiet duration. Retaining Stop avoids depending exclusively on screen
classification and preserves the existing attention consumers. A delivered
nudge's reply and operator/owner message echoes preserve the existing idle
clock and budget. Actual tool activity cancels idleness; if idle resumes after
real work, it must satisfy grace again. Remaining continuously idle permits
further nudges at the backed-off times, up to the obligation budget. Pending briefs remain
protected by the existing delivery ordering.

Working evidence also has a fifteen-minute inactivity bound. The leaf engine
persists when it first observes the turn, and compares the clock with the newest
tool activity, harness hook, meaningful screen output, or delivered-message response window. Screen
redraws of the Working spinner line do not refresh that bound: a Codex spinner can continue animating
while its turn is hung. An exceeded bound goes directly to the supervision
ladder, carrying the turn start, tool/hook/output timestamps, and latest screen
observation. Output digest changes are persisted at most once every thirty
seconds; screen capture and hashing also run during active output. Automatic
interruption is deferred to the supervisor and recorded in the evidence. Fresh tool, hook, or output activity restores the actor's ability.

A queued operator brief is released by a fresh idle observation newer than the
brief, including a Codex composer after interruption without a notify hook.
The existing convoy/session watch reconciler handles this on the authority host
under the same message lock as resume, withdrawal, and completion. Delivery
preserves sender attribution, stages credentials, and retains the pending brief
on staging failure. Codex screen classification distinguishes an idle composer
from a stable Working display or permission prompt; unknown screens remain
unobservable. Selection rows in Codex menus are not composers. Submitted prompt
history above the bottom composer does not make that composer unobservable,
including when a principal attach changes terminal geometry. Adapter-classified
prompt evidence governs both observation and terminal FIFO readiness, even when
pixel activity is active or unavailable. Review-round wakes, supervisor resumes,
and operator messages share that FIFO and its delivery receipts. Losing observation
does not declare a new turn: the durable turn start survives stale or Unobservable
evidence, and fresh activity restores ability. Fresh hook evidence still takes
precedence for up to two minutes.

Operator commands such as `flotilla crew resume` and `flotilla crew handoff`
create a new Message for each explicit invocation. If a command loses its response,
inspect `flotilla crew list` and the receiver's Message records before running it
again: another CLI invocation can enqueue the same instruction a second time.
Transport retries within the original command reuse its immutable Message ID;
operation tokens across separate CLI invocations are tracked in #2710.

Schema authors: WorkflowTemplate manifests in the external project-map/ops
repositories may specify `stall_nudges`; their existing field names and defaults
remain compatible, so no companion manifest edit is required. Convoy status and
Checkout integration status and TerminalSession status are daemon/controller-authored; no external manifest
source authors the new status fields. New stored fields decode with defaults
under ADR 0047; the stored-record corpus must not be regenerated for this change.

Claude tool subscriptions take effect when crew settings are regenerated; already
running crews retain their previous hooks and use attention-only evidence until
restarted. The hook client parses one payload and sends one daemon RPC, waiting
for acknowledgement. It performs no provider refresh, but it is synchronous and
currently has no dedicated RPC timeout. Measure the PreToolUse critical-path
latency and daemon-unavailable behavior during host-direct acceptance testing.

## Declaration convergence and standing-convoy rolls

`flotilla project list` reports a refused ops declaration and flags refusals
older than 24 hours as stale. Project and ConvoyEnsure status retain the entry
path, parser error and first-refused timestamp. Successful refresh clears the
condition and its attention demand.

A running ConvoyEnsure keeps the configuration captured at admission. Changes
to repositories, workflow, placement, agent overrides or presentation raise
configuration-drift attention while its crew continues working. At a suitable
boundary, use `flotilla ensure roll <ensure-name>` to admit the current
configuration. `flotilla ensure roll --drifted` rolls all ensures currently
reported as drifted in the namespace; `--namespace` selects another namespace.
Admission is validated before the previous generation is abandoned. Its record
remains as history, and ordinary lifecycle reconciliation reclaims its backing.
A repeated roll without drift does nothing. Generations admitted before config
tracking report an unknown admission baseline and require an explicit roll to
establish it. The first roll of this daemon generation deliberately raises
attention for every pre-existing standing convoy: schedule a safe boundary and
run `flotilla ensure roll --drifted` to establish their baselines. Retirement and
admission are separate durable writes; a failure or restart after retirement can
leave a gap. The ordinary ensure reconciliation loop recovers admission, using
its backing verification and retry backoff. Driver moves recover on the new
driver rather than immediately on the retiring host.

Before a fleet roll, run the candidate's `resource validate --from-daemon` on
each host. It decodes the live store and parses the registered projects' committed
ops entries with the candidate parser. The resource API exports raw declaration
inputs, so forwarded `--host` validation also uses the candidate parser. An older
daemon without that inventory endpoint requires validation on its own host,
where the candidate reads committed ops sources through its VCS interface.
Missing or ambiguous ops checkouts fail validation.

Candidate validation also prints a `workflow retirement preview: ` JSON report
from the merged durable definitions, using the same builtin catalogue as startup.
It lists builtin-labelled templates outside the candidate set, apply-ready copies
of their definitions, and affected Projects' explicit `default_workflow_ref`
values. References distinguish the two supported retired-name aliases (both map
to `single-agent`), surviving Project/ancestor-scoped definitions, and global
references that will no longer resolve. This report is advisory; schema and ops
validation still determine the gate's exit status. Observed resources and raw
replica provenance do not drive retirement. Existing Convoy frozen snapshots are
unaffected.

For live operator acceptance, run on each host with the candidate binary before
installing it:

```bash
scripts/preview-workflow-retirement.sh /path/to/candidate /path/to/new-backup-directory
```

Review `retirement.json`, verify expected aliases and unsupported references,
and retain the output outside daemon state. Confirm the live definitions remain
unchanged after validation. The script exports `restore-*.json` manifests with
specs, names, namespaces, labels and annotations, without server-owned identities
or statuses. Startup retirement creates causal tombstones: rolling back the
binary alone does not restore these definitions. After rollback, explicitly
restore a reviewed definition with `flotilla resource apply --file /path/to/restore-0.json`
(check `resource apply --help` for the installed binary's syntax). Applying it
while the candidate is still installed will let startup retire it again. Update
unsupported Project defaults or provide a Project-scoped replacement before the
roll. This container has no live fleet; this acceptance is operator-run.


## Git in contained host worktrees

Contained host-worktree vessels receive process-scoped tracking settings for their
provisioned branches: `branch.<branch>.remote=origin`,
`branch.<branch>.merge=refs/heads/<branch>`, and `push.default=current`.
Use plain `git push` to publish the branch, including its first push; later
`git push` and `git pull` follow the same branch on origin. Plain `git push`
reuses the provisioned tracking; `git push -u` requests a write to shared config.
These settings are
scoped to the vessel environment and coexist with the injected global credential
config. The shared clone config and hooks retain their read-only mounts, and the
host config guard continues to reject `extensions.worktreeConfig` and executable
configuration. Branches created manually after provisioning can be published with
an explicit branch.

The shared Git metadata remains writable for objects and refs, while `config`,
`hooks`, and the entire `worktrees` parent are mounted read-only. Writable
submounts expose only the vessel's own administration directories, resolved by
Git through the typed VCS boundary. Siblings registered after container startup
inherit the same read-only parent. Metadata mounts use the same absolute host
paths inside the container, preserving `.git` pointers and host backlinks.

Docker cannot update these bind mounts in place. When checkout membership or
resolved administration paths change, vessel reconciliation holds provisioning
and requests environment recreation before launching crews. A Ready vessel
keeps its phase and existing terminal sessions while showing the recreation
message. Stop existing crews, remove the vessel environment with
`flotilla resource delete Environment <environment-name>`, and let reconciliation
recreate it with the new mount set; it does not automatically interrupt running work. Environments created
before these protection mounts also require recreation.

Managed creation uses `git worktree add --lock --reason` so a later protection
failure cannot leave a newly created registration exposed to prune. Transient
registration-protection failures retry the same Pending Checkout; its bootstrap
ref preserves branch provenance. Failed protection does not delete the target.
Normal teardown preserves dirty reused checkouts and pre-existing branches.

Queued-turn diagnostics use the `crew turn delivery decision`,
`pending crew turn delivery decision`, and `terminal crew turn delivery decision`
events. They record convoy and sender, current attention state, source and
timestamp, the 120-second hook precedence window, and the queue, release,
confirmation, or skip reason without logging message contents. Repeated pending
and skip decisions use debug level; queue, release and confirmation use info.
Debug-level `terminal attention decision` events show accepted observations and
precedence/debounce skips. `Codex turn hook configuration` records the effective
config path, contained detection, and whether notify needs repair. An already
trusted contained workspace still gets
`notify = ["flotilla", "hook", "codex", "notify"]`.
Host-direct launches use an invocation override instead.

Before #2598, Active convoys armed only conflict probes, explaining why the
hookless incident never queued its review wake. Active Working or Interrupted
crews now also arm workflow-declared checks and actionable-review rules; queued
turns still wait for terminal readiness. Debug subscription decisions identify
`skip_ineligible_active_crew_or_subject` when the target or subject is ineligible.
Escalation warnings now distinguish `supervisor_lookup_failed`,
`operator_rung_selected`, and `supervision_policy_exhausted`, and include the policy
cursor and lookup evidence. A live governor can coexist with an exhausted policy;
its existence alone does not mean that an escalation lookup failed.

### Reproducible build experiments

`scripts/build-bench` (Python 3, Linux `taskset`, `cc`, `du`, and Cargo) runs
paired, interleaved baseline/challenger rounds from the **committed HEAD**.
Commit source changes before measuring. Variants live in `scripts/build-bench.json`;
each has optional Cargo `config`, `env`, `rustflags`, and crew gate ordering.
Optional `commands` maps action names (`test`, `no-run`, `clippy`, `check`) to
Cargo argument arrays. For example, `"commands": {"test": ["nextest", "run",
"--workspace", "--locked"]}` supplies a test runner without changing the harness;
include a `no-run` override too when comparing nextest binary selection. Linker
variants can set `rustflags` or Cargo target linker config. These are extension
points for #2750; those tools are not required by the default variant map.

Ambient or config `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` are preserved, with
encoded flags taking Cargo’s usual precedence; declared `rustflags` are appended.
Effective flags are recorded in each run. If no flags are supplied, the harness
leaves both flag environment variables unset so Cargo config flags still apply.

The baseline explicitly uses line tables, dependency `debug=0`, four Cargo jobs,
and incremental off. All variants use the same pinned nightly, including those
without unstable flags, to avoid mixing compiler versions into comparisons.

```bash
scripts/build-bench --output /tmp/build-bench-4cpu --profiles limited contended \
  --cpus 4 --contenders 2 --jobs 4 --reuse-prime --rounds 2
scripts/build-bench --output /tmp/build-bench-quiet --profiles quiet --jobs "$(nproc)" --reuse-prime --rounds 5
python3 -m pip install -r scripts/tests/build-bench-requirements.txt
python3 -m unittest scripts.tests.test_build_bench
```

Use `--jobs N` to enforce a uniform job count over config and variant environment
values. Both the environment variable and final Cargo `build.jobs` config are
set deliberately: the latter also overrides a variant’s `build.jobs` entry.
The four-job default is the crew vessel baseline; full host parallelism
is an operator run. `--reuse-prime` keeps each worker’s private target across the
selected scenarios in a pair and deletes it at the end. Edits accumulate in the
disposable source, and the first crew round then starts primed by prior scenarios;
`source_state` records this distinction. Omit it for independent fresh scenario
primes, including a crew cycle starting from a cold target. A failed prime stops
that worker’s suite because later edit timings would be invalid. Each non-cold
scenario still runs its declared warm-up against the reused target; this is
usually a no-change rebuild, is recorded separately, and is excluded from edit,
primed and test-runtime timings. Crew-cycle includes both rounds in its total.

Use `--variants incremental threads-4` and `--scenarios cold core-edit` for a
smaller experiment. The quiet profile uses the vessel's available affinity;
it cannot remove an inherited CPU quota. Limited runs pin to the first N allowed
CPUs; contended runs launch K independent builds on that same CPU set. Record
both the requested affinity and inherited quota when interpreting results.

Every worker gets a disposable source archive and isolated target, removed after
its scenario or scenario suite even on build failure. Dependency downloads share Cargo's registry;
run `cargo fetch --locked` first to avoid timing downloads. `results.json` records
flags, selected environment, host/toolchain/linker, affinity/quota, phase times,
load samples and final target bytes. CPU quota and memory limits are read from
cgroup v2 files and are null where those files are unavailable; `medians.md` compares each challenger with
its paired baseline. Logs survive beside those files. Choose a new output directory
for each invocation. Git prompts are disabled; SSH defaults to batch mode, strict
host-key checking, and a five-second connection timeout so test fixtures cannot
hang on an interactive prompt. An explicit caller `GIT_SSH_COMMAND` is preserved. Failed runs are shown and excluded from timing medians, and
the command exits unsuccessfully if any build fails.

Cold measures `cargo test --workspace --no-run --locked`. Leaf/core edit probes
prime the target, then append one comment line to the disposable TUI/core source.
Primed measures the no-change rebuild after that same prime. Crew-cycle measures
both a full test/gate round and a second round after a core fix; individual phase
and second-round times remain in JSON. Optimization variants can therefore be
compared for test execution as well as build time. `--scenarios test-runtime`
primes with `--no-run` and times just the subsequent test invocation. The `check-gate` variant is a
cost probe, not a recommendation to drop lint coverage. A primed private target is
only a phase-2 probe, not a shared-cache implementation. Load averages are sampled
once per second; they describe the whole host, rather than only the vessel.
