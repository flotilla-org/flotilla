These command outputs were recorded on 2026-10-05 from the actual pinned cleat
CLI, revision `00c072b207dc943f6c93fe3b6b09abaa257695a6`, using private temporary
runtime roots. No host or crew daemon was drained.

For success/unchanged/warning, an HTTP-over-Unix-socket stand-in for the old
generation enforced the pinned daemon contract: `GET /` returns build status,
`GET /sessions` returns the session list, and `POST /drain` returns draining
status (success) or 404 (warning). The success/warning old build reported the
previous revision `fd66a7121149e85eb8fbc57a99a388b0c91dae6c` and protocol 11;
unchanged reported the installed build. Success and warning launched a real
successor daemon from the installed CLI and captured its actual build metadata.
For nonzero, no daemon was listening in the private root.

The recorded invocation was:
`cleat --runtime-root <private-root> --server default server drain --json`.
The files retain stdout, stderr, and successful/unsuccessful exit classification
(the injected CommandRunner's output contract). Report JSON is captured exactly
as emitted, including warnings and all embedded build fields.

`generations.json` was recorded with the installed r531 CLI
(`c1e7a7692bbbb93d7c246e2669bebae922343c88`). Run
`python3 crates/flotilla-core/src/cleat_roll/fixtures/record_generations.py` from
the repository root to re-record it. The script uses a private temporary root,
a contract-enforcing old-daemon stand-in (three sessions, protocol 11), and a
real successor launched by `server drain --json`. It records the successful
26→27 drain, current alias version, daemon listing, missing-daemon failure, and
stale alias version after deliberately pointing the private alias back to 26.
Only the private runtime path is masked as `{root}`. The private successor is
stopped at the end; no host or vessel daemon is changed.

Health must query the current alias, preserve alive draining generations as
information, and never use `cleat list` to count their sessions: that command
may adopt daemons or sweep recordings. Counts come from the captured drain
report and are explicitly labelled as counts at drain time.

The CLI stderr prefix `connect daemon:` is part of the missing-daemon
classification contract. Both `nonzero.json` and `generations.json` record it.
That prefix alone never suppresses an error: `daemons --json` must also confirm
no matching daemon is alive. A changed prefix or unavailable listing keeps the
failure actionable rather than silently accepting it.

Daemon listings are matched to the literal environment-owned runtime root and
logical name used in the command. Flotilla does not canonicalize vessel paths
through the host filesystem. Differently spelled/symlinked roots may omit an
informational draining line; they cannot select the build used for the current
alias comparison. Unknown build evidence stays visible as an informational
condition and never degrades the host.

Each target can issue two alias probes, one version query and one daemon listing,
with a 5-second deadline per command (20 seconds worst case). Targets run
concurrently, so this bound does not multiply by the number of environments.
