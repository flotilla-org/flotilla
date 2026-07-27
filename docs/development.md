# Development

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
