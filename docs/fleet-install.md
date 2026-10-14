# Fleet installation and macOS privacy

Run `scripts/fleet-install latest` (or a generation ID) using the configured
fleet reader credential. `scripts/fleet-install rollback` restores the previous
verified generation. `status` reports the selection and wire generations.

## Stable Darwin executables

On macOS, fleet-install copies the selected generation's signed `flotillad`,
`flotilla`, and `cleat` into `~/.local/opt/flotilla-fleet/tcc/bin/`. These are
regular files, not symlinks. The executable-relative libraries are copied into
`tcc/lib/`. Launchd and the managed CLI launchers execute these fixed paths;
activation, reinstall, recovery and rollback all refresh the copies from the
selected verified release. Linux retains its generation symlinks.

Keep `FLEET_INSTALL_ROOT` unchanged between installs: changing it changes the
privacy identity. Signing remains the existing comte signing contract (team
`973L4GV58R` and the existing binary identifiers/designated requirements).
The owner deferred app bundles for #2798; fixed-path copies are the chosen
identity mechanism for this roll.

The first install at these paths needs a one-time privacy grant. If configured
checkouts or charter sources live in Documents, Desktop, Downloads, or on
removable/network volumes, grant the corresponding Files and Folders access.
For access to protected application data, or a daemon that manages repositories
in several protected locations, the operator can instead add
`tcc/bin/flotillad` to System Settings → Privacy & Security → Full Disk Access.
Add `tcc/bin/flotilla` and `tcc/bin/cleat` too if their work requires that access.
Use the file picker’s “Go to Folder” to enter the hidden `.local` path. Restart
the daemon after changing grants. Old generation-specific grants may be removed
once the fixed-path installation has been accepted.

## Host refusal and startup confirmation

Before downloading or stopping a daemon, installation and explicit rollback
refuse when the filesystem containing the daemon state directory has less than
5 GiB available, or memory pressure is critical. The state directory follows
`XDG_STATE_HOME` (default `~/.local/state/flotilla`); a missing directory uses
its nearest existing ancestor. The check runs again before activation, so
staging and canary work cannot consume the headroom unnoticed. Automatic
recovery remains available after a failed activation.

Override the floor with `--min-free-bytes N` (default `5368709120`) or
`FLEET_INSTALL_MIN_FREE_BYTES`. On macOS, the read-only
`kern.memorystatus_vm_pressure_level` sysctl supplies memory pressure (normal=1, warning=2, critical=4); critical
refuses. On Linux, `MemAvailable` at or below 5% of `MemTotal` refuses.
Missing or malformed pressure observations refuse with a diagnostic. Relieve
the reported pressure or free space, then retry the installer.

Normal health confirmation waits 90 seconds, allowing the daemon's first
60-second pending-phase warning to arrive. A structured
`daemon startup phase remains pending` warning written since this activation
extends confirmation up to `--max-wait-seconds N` (default `600`), also settable
as `FLEET_INSTALL_MAX_WAIT_SECONDS`. Historical warnings cannot extend a new
attempt. The maximum caps the entire health wait even if warnings continue or
the health CLI hangs; each probe also has a five-second cap. A permanently
pending startup eventually fails confirmation and follows the existing rollback
path. Recovery gets a separate bounded health wait and ignores candidate logs.
`FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS` still overrides the ordinary window;
setting it below 60 seconds can expire before the first pending warning.
Flags precede `--skip-canary`, `--canary`, or the operation, for example:

```sh
scripts/fleet-install --min-free-bytes 8589934592 --max-wait-seconds 900 latest
```

## Probe audit and configuration

| Probe | Locations and policy |
| --- | --- |
| Free-space admission/heartbeat | Queries only the requested filesystem with `statvfs` on Unix. Does not enumerate other mounts, removable media, or network volumes. Missing checkout paths use the nearest existing ancestor; permission errors do not fall back. |
| Host tool discovery | PATH binaries and known `.claude`/`.codex` dotfiles; version probes run from `/` rather than inheriting a checkout/HOME working directory. No scan of other apps' `~/Library/Application Support` or containers. |
| Repository discovery and Git config inspection | Only configured observation roots, adopted checkouts, and their Git metadata/ancestors. Register a root explicitly to opt into observing a protected checkout; keep roots outside protected folders when that access is unnecessary. |
| Charter and operational-entry reads | Explicit `CharterSource`/manifest and Project ops configuration. Repository charters read committed blobs; local-directory charters reject symlinks. Do not configure HOME or another app's data directory as a charter source. |
| Authentication | `.codex/auth.json`, configured `CODEX_HOME`, and explicitly configured Forge credentials/token paths (or the existing `.config` token discovery). Configuring a protected token location opts into accessing it; ordinary dotfile locations avoid that need. |

There is no default Documents/Desktop/Downloads/Library/Volumes crawl. The
existing explicit root, charter, and credential configuration is the access
gate for those locations; no extra blanket protected-directory probe is added.

Filesystem probes run in blocking tasks with a five-second caller deadline.
At most eight such probes can remain outstanding, including timed-out OS calls;
a pending TCC prompt cannot exhaust the async workers or accumulate an unlimited
retry queue. This capacity is shared by filesystem callers; saturation makes
new probes fail immediately with a distinct capacity-exhausted error, including
git guards and path resolution, until an outstanding OS call completes. Discovery bounds each detector/factory independently and retains
other capabilities. Subprocess discovery deadlines kill their process groups.
A timed-out OS filesystem call itself cannot be cancelled: answering the prompt
releases its blocking task and capacity. A failed space probe publishes unknown
space and a degraded host condition while preserving the heartbeat and alarms.

## Controller recovery

An exhausted controller publishes a `Controller/<kind>` host condition with
reason `RestartBudgetExhausted`, visible in `flotilla fleet`. It retries with a
fresh budget after five minutes. The alarm remains during backoff and clears
only after a restarted run survives sixty seconds (or shuts down cleanly).
The daemon need not be restarted to resume provisioning.

## Pre-activation canary on feta

After finalization, every normal `fleet-install latest` or explicit-generation
install gates activation on a canary on feta. Before any launch service or
`current` link changes, the incoming installer stages that generation on feta
and runs its own unpacked `flotillad`, `flotilla`, and `cleat`. Consumers on other
hosts send the incoming installer and validator over authenticated, batch-mode
SSH; they do not rely on feta's installed bootstrap knowing the new command.
Feta needs the existing package reader token, Docker setup (including registry
authentication), and enough memory for the regular contained-crew policy. Each
consumer runs the gate; parallel installs may refuse on feta's mutation lock
and should be retried after the other installation finishes.

The second daemon has private config, state, HOME, socket, and
`CLEAT_RUNTIME_DIR` under a short `/tmp/fleet-canary.*` path. No fleet config is
copied and it has no peers. Its scratch Git repository needs no forge
credential. The scratch checkout has a private local bare repository as a
`file://` remote, so convoy admission has a clonable transport; the host
worktree checkout mode keeps that path outside the container. The generation
no longer packages a crew-image baseline. The canary reads the newest Ready
local Docker Environment's `local_image_id` through the installed host client
(with auto-spawn forbidden), then freezes that concrete image in a no-layer
composition for the isolated daemon. It does not build or discover image tags.
An operator can select another built local ID or pinned registry reference with
`FLEET_CANARY_IMAGE`; missing/malformed evidence fails the gate rather than
selecting an older image. The canary uses `if_not_present` acquisition with
`docker_per_vessel` and the shared crew memory-policy default
(currently 80% of host RAM divided among four crews, with zero swap). The
`fleet-canary` adapter is available only when `FLOTILLA_FLEET_CANARY=1` is
discovered. It uses normal agent
provisioning and skill staging, with a private config home and no model/login.

The stub dumps its launch environment, effective Git configuration, and staged
`testing` skill. The installer observes a Ready vessel and Running terminal,
checks `RUSTUP_HOME`, effective author and committer identity from `git var`
(including environment overrides), `push.default`, skills, and absence of
model/forge token variables, then releases the stub's completion claim. Real
Cleat rejects its managed session coordinates supplied via `--env`; a Running
session proves that launch boundary accepted the declared environment. The
gate waits for Landed and the terminal/environment/vessel finalizers, and checks
that its Docker container is gone. Container deletion also reaps its contained
Cleat endpoint and sessions. It never prunes or kills fleet containers/sessions.

All runs stop the canary daemon and reap any matching host Cleat daemon started
by discovery. Host Cleat cleanup checks both its executable and private runtime
environment, using Linux pidfds (Linux 5.3+ and Python 3.9+) to avoid PID reuse;
stale or inaccessible PID
files never authorize signalling another process. Successful runs remove scratch
state. Failures retain state, logs, and any surviving canary container for
inspection. The diagnostic names the failed assertion and log directory, and failed
commands include both stdout (including CLI JSON errors) and stderr;
no active generation is switched. Inspect `daemon.log`, `commands.log`, and
`crew-report.json` when present. After inspection, remove only the container ID
recorded by that canary and its `/tmp/fleet-canary.*` directory.

`fleet-install --canary <generation>` stages and probes without activation and
is accepted only on feta. For an explicit emergency bypass, use
`fleet-install --skip-canary <generation>` (or `--skip-canary latest`); every
activation prints a warning identifying `FLEET_INSTALL_SKIP_CANARY=1`, which can
also be set explicitly in the environment. Rollback retains the previous-generation path and
does not run a new canary. The real-Codex optional probe is not implemented.

For the first generation carrying the canary payload, sync the reviewed
`fleet-install` and `generation_validation.py` bootstrap pair before downloading
it, as for previous payload-boundary crossings. The new validator accepts old
generations for rollback. No workflow changes are needed.

`scripts/test-fleet-install.sh` includes `scripts/test-fleet-canary.sh`, which
covers gate ordering, refusal, visible bypass, stage-only execution, and the
lifecycle scenarios through injected subprocess seams. It also builds the CLI
and daemon and runs the production setup through convoy admission against
them, then stops without waiting for Docker. Real feta/Docker runs
are the operator's acceptance after merge. This gate replaces the manual
post-roll "does a fresh crew launch" check.

## Bootstrap helpers and synchronization

The bootstrap must work before a healthy, trusted generation exists and when
rolling back to an older generation without new installer subcommands. Its
lock-owner release, secure nonce, token-mode inspection, launcher realpath
check, atomic selection-link replacement, service-file rendering, Darwin
fixed-path copying and confirmation disarming are commands in the existing
`generation_validation.py` module. They must remain available independently
of candidate daemon health and of the previous generation's CLI interface.
Sync **both** `fleet-install` and `generation_validation.py` beside one another;
do not deploy either alone. The incoming installer sends this same pair to
feta for the canary gate. There is no additional bootstrap module or required
generation payload path.

Before crossing a payload boundary, stage the reviewed bootstrap/validator pair
on every consumer, including feta. Keep the validator compatible with the
retained rollback generations: rollback re-verifies the previous release using
the installed validator, so rolling the binaries back does not repair an
incompatible validator. Pruning does not synchronize or roll back that module.
The old-generation fixtures in `ci/fleet-candidates/test-generation-validation.sh`
exercise this rule.

This extraction removes all twelve remaining Python heredocs from
`fleet-install`: lock cleanup and stale-owner release, two nonces, token mode,
two realpath checks, atomic link replacement, systemd path escaping, launchd
plist rendering, Darwin fixed-path copying and confirmation disarming.
The shell test's Python fixtures and assertions are ordinary functions in
`scripts/fleet_install_test_support.py`; it no longer extracts and executes
source text from the installer. Pruning, fleet diagnostics, health checks and
post-install Cleat turnover already delegate to the selected generation's CLI.
The shell retains download, verification, selection, service control and handoff.

Run `ci/fleet-candidates/test-generation-validation.sh` and
`scripts/test-fleet-install.sh` for local acceptance. The helper unit tests
inject filesystem and entropy boundaries; the shell suite exercises bootstrap
and handoff with fixture commands, and includes the canary contracts. For live
operator acceptance on feta after merge, stage the reviewed pair beside one another, then run:

```bash
scripts/fleet-bootstrap-acceptance.sh /path/to/fleet-install <generation>
```

This operator script selects the adjacent validator and clears any inherited
emergency canary bypass before invoking `--canary`. It runs the finalized
generation's Docker canary without changing `current`; inspect retained
diagnostics on failure before proceeding with a normal install. No Docker or
live fleet is needed for the local tests.

## Automatic pre-roll refusals

Before any consumer install, use a checkout of the **exact generation source
revision**, after finalization. Run the operator gate from that checkout:

```bash
scripts/fleet-preroll-checks.sh <generation> --consumer feta --consumer comte --consumer <other-consumer>
```

Repeat `--consumer` for every installation consumer in the current fleet,
including the desk and feta. The list is explicit so a changing fleet cannot
silently inherit a stale hard-coded inventory. A successful gate is required
before proceeding to the existing candidate record/ops validation, feta canary
and install/health-check steps. Run it again if tools or bootstrap files change.
The script never installs or changes the active generation.

The read-only gate hashes raclette's `/usr/local/sbin/lab-fleet-promote`,
`lab-fleet-finalize-darwin`, and `generation_validation.py` via
`ssh silo 'qm guest exec 106 ...'`, and comte's
`~/.local/libexec/lab-darwin-sign` and `generation_validation.py` via `ssh comte`.
Each must match `ci/fleet-candidates/` in the generation source checkout.
A refusal names the remote file and installed and generation SHA256 values.
Missing files and remote failures refuse too. SSH uses a ten-second connect
timeout, with a 120-second total command deadline. The gate collects lab and
consumer failures in one refusal list; any lab failure disables all repairs. Lab copies are operator-owned:
this gate never pushes or repairs them, even when bootstrap repair is requested.
Sync lab tools through the existing operator deployment procedure and rerun.

The gate then checks **both** `~/.local/bin/fleet-install` and its adjacent
`generation_validation.py` on every consumer against `scripts/fleet-install`
and `ci/fleet-candidates/generation_validation.py`. A mismatch refuses before
installation and prints this exact paired sync command for the affected host:

```bash
scripts/fleet-preroll-checks.sh <generation> --source-root <generation-checkout> --consumer <host> --sync-bootstrap
```

This explicit repair saves both installed files as `.pre-<generation>` before
writing either generation file, preserves their modes, and rechecks both hashes.
An existing backup, missing installed file, or symlink refuses repair. A write
failure restores both old contents with atomic file replacement. Backup,
replacement and restore file contents are fsynced before proceeding. Run without repair against the **full**
consumer list again before installing. Do not manually copy just one member.
Repair assumes the operator has stopped concurrent bootstrap installs; it does
not acquire the installer's mutation lock. Retain the backups for rollback
inspection; bootstrap compatibility with old generations remains required.

After a failed repair, stop any still-running repair process and inspect both
installed files against the retained `.pre-<generation>` backups. The gate
keeps those backups as recovery evidence and removes only temporary files it
created. A pre-existing `.new-<generation>` collision is left untouched. After
confirming the original pair is restored (or restoring both from the backups),
remove both `.pre-<generation>` backups and any inspected stale
`.new-<generation>` files together before retrying the paired sync command.
Never remove these files while an install or repair is active.

For live operator acceptance, first run the read-only full-list command above.
On a lab tool mismatch, confirm the diagnostic names both hashes and that no
consumer files changed; sync the operator-owned tool and rerun. For a bootstrap
mismatch, confirm refusal, run the explicit paired command, inspect both
`.pre-<generation>` backups, and rerun the full-list gate. Then run
`scripts/fleet-bootstrap-acceptance.sh` on feta before normal installation.
No live acceptance was run in a crew container without SSH or Docker.

Local coverage is `python3 scripts/test-fleet-preroll-checks.py`, also included
in `scripts/test-fleet-install.sh`. `--host-command /path/to/executable` injects
the process boundary: it receives `HOST COMMAND` as two arguments, a Python
program on stdin, and returns a JSON array of hashes on stdout. Production
uses batch-mode SSH and Proxmox guest-exec; tests use no SSH or live fleet.
Every target needs `python3` on its non-interactive SSH/guest-exec PATH,
including comte on macOS; an interactive shell-only PATH setup is insufficient.
A failed rollback names the original failure, each unrestored file, and both
retained backups for operator recovery.

## Declared-query SQLite indexes and rollback (#3057)

The declared-query migrations preserve resource bodies and support ordinary
fleet-install rollback without index cleanup. Persisted tables and indexes do
not reference custom functions. Native writers maintain canonical field values
with connection-local TEMP triggers, like the digest index.

Older binaries and plain SQLite connections can write without registering query
functions. Such writes can leave the new derived values stale; each native open
repairs them from authoritative local and replica bodies in one transaction,
including changed values and deleted records. Resource bodies are not rewritten.
Existing native readers are not refreshed by concurrent unconfigured writes;
reopening the native store performs the repair. No writer inventory, function
registration, or derived-schema cleanup is required before rollback.

The new field-value tables and lookup indexes are additive named migrations.
Superseded built-in indexes may be retired after the rollback retention window;
that housekeeping is separate from rollback itself. Native open also removes
function-dependent objects from pre-release #3057 schemas automatically.

### Crew-image cut-over rollback boundary

The candidate validator accepts the retired `crew-image-baseline.yaml` payload
through the compatibility roll so previous generation **bundles** remain valid.
This does not guarantee that the previous binary can decode resources written
by the candidate. Its `ImageComposition` decoder denies unknown fields and
rejects new `literal_base` snapshots. ADR 0047 guarantees N→N+1 decoding only.
After new stored writes, rollback needs a decoder-compatible build or explicit
stored-data recovery. The pre-activation canary uses a private temporary store
and does not cross that boundary in the live fleet store.

## Contained tools on a roll

The candidate packages cleat's prepared `.tools/ghostty-install/share/terminfo/`
verbatim, including its license. A missing `x/xterm-ghostty` entry refuses the
candidate. This depends on cleat #337; the fleet cleat source pin must select
that change. No image builds Ghostty or supplies Flotilla binaries.

Docker vessel provisioning requires a complete Linux tools install, including
cleat's prepared terminfo database next to the resolved library prefix. Desk
builds must run cleat prepare and supply that tree, or install a fleet candidate;
missing assets refuse provisioning with the expected path and build guidance.
There is no per-file fallback: it would silently create vessels that cannot
upgrade in place. The delivered library uses the Linux SONAME
`libghostty-vt.so.0`, including when its source resolves to a versioned filename.
A native macOS dylib is not a Linux Docker vessel runtime asset.

Local Docker binds the provider-owned `environment-tools/fleet` directories
`tools`, `bin`, `lib`, and `share/terminfo` to matching `/opt/flotilla` children,
read-only to the vessel. This preserves the image's sibling harness paths. The
provider publishes immutable `tools/<content-generation>/` trees containing
`bin`, `lib`, and `share/terminfo`. Relative entries in the stable real delivery
directories follow one atomically replaced `tools/current` link. CLI invocations see the new generation immediately. Cleat's candidate RPATH
(`$ORIGIN/../lib`) selects its own generation's library even during a link flip;
tool delivery does not override that with a current-link library search path. A per-root filesystem
lock serializes publication and pruning. Mounting a resolved child directory
would pin Docker to that inode and is forbidden by the tools contract.

`fleet post-install` publishes the complete incoming tools before invoking
`cleat server handover --json` through every registered local environment runner.
It never requests incompatible-client drops or kills sessions. Failed handovers
leave the old daemon and its sessions running, preserve diagnostics, and report
advisory `CleatBuildSkew` until retry or environment relaunch. Vessels created
before this tools contract retain their old mounts and may refuse handover;
relaunch those vessels to acquire the generation parent mount.

After each post-install, local pruning checks host kernel file identities across
process executables, mapped libraries, and open descriptors for the host UID
that Docker uses for vessel tools. Unrelated system UIDs are outside that
provider contract. It preserves the
current generation and any generation still in use, including old cleat child
reapers. Incomplete process visibility defers pruning; a later roll retries.
Other providers must supply their own unused-generation evidence. k8s can fill a
shared volume from an init container; sandboxes can upload the complete tree
before replacing its link. Those adapters are outside this change.

The candidate-build host gate runs
`ci/fleet-candidates/test-tools-handover.sh <pinned-cleat-checkout>` before
packaging. Cleat's real three-session handover scenario transfers PTY descriptors
and redirects an attached watcher, verifies unchanged child PIDs, and sends input
before and after the roll. It also proves the old daemon continues reaping those
children. The passthrough engine isolates descriptor ownership from VT rendering;
there is no Docker or live fleet in this gate.
