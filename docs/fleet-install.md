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
retry queue. Discovery bounds each detector/factory independently and retains
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
