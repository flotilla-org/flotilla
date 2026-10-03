# Development

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

## Cargo target cache policy

A checkout's `target/` is managed cache state. The fleet uses two complementary controls: a daily, per-host mtime-based sweep for old Cargo artifact families and a per-checkout size-cap backstop for unusually large targets.

### Daily mtime-based sweep

Install and immediately verify the schedule on each fleet host from a Flotilla checkout:

```bash
scripts/install-cargo-sweep-schedule.sh
```

The installer pins `cargo-sweep` 0.8.0 when the command is absent, copies the runner to `~/.local/libexec/flotilla/`, installs a systemd user timer on Linux or the checked-in launchd agent on macOS, enables the daily schedule, and starts one observed run. The installed policy runs `cargo-sweep --time 3` once a day. This is an mtime-based three-day retention policy.

Each run sweeps:

- every immediate directory under `~/dev/` that has its own `target/`; and
- every checkout with a `target/` beneath `~/dev/flotilla-repos`, covering that convoy root until lifecycle teardown owns checkout removal under #1113.

The runner records reclaimed bytes for every root and the whole run in:

```text
~/.local/state/flotilla/cargo-sweep-mtime.log
```

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

### Crew and CI debuginfo

Contained `rust-build-limits` tools export `CARGO_PROFILE_DEV_DEBUG=line-tables-only` and prepend a staged Cargo shim to `PATH`. The shim supplies `--config 'profile.dev.package."*".debug=0'`, so workspace crates retain file and line information in backtraces while third-party dependencies emit no debuginfo. CI supplies the same Cargo dependency override and workspace environment setting. Test builds inherit the dev profile. Release and bench profiles retain their existing Cargo settings.

For a build requiring workspace variables and types in a debugger, use `CARGO_PROFILE_DEV_DEBUG=full cargo test --workspace --locked`. This explicit override wins over the tool environment's default; dependency debuginfo remains disabled. Desk builds without the contained tool environment keep Cargo's full workspace debuginfo. The shim forwards `+toolchain` selectors and other arguments to the next Cargo executable on `PATH`, preserving repository Cargo config and Rust flags.

### Size-cap backstop

The daily host sweep owns age-based removal. `scripts/prune-target.sh` only caps a single checkout when its target grows unusually large: it removes the oldest incremental generations until their total is at most 10 GiB, then asks `cargo-sweep` to reduce the complete target to at most 20 GiB.

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

With Cargo's default configuration the script acts only on that checkout's `target/`. When `CARGO_TARGET_DIR` is set, the command intentionally honors it. Relative values are anchored to this checkout's root, so use an absolute value if Cargo is normally invoked elsewhere.

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
identified by its maker and leaves. Working attention clears the visible stall
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
