# Selected platform tests

`selectors.txt` owns the selected Cargo tests. Each row is `scope|arguments`:
`windows`, `macos`, `all` (both desktop OSes), or `tender-ssh` (the isolated
real-SSH proof). `all` rows run only on Windows/macOS; Tender SSH runs only
`tender-ssh` rows and never inherits `all`. Blank lines and full-line comments are ignored. Arguments are
whitespace-separated literal argv, with no shell quoting or expansion. Arguments
cannot contain `|`, which is reserved for the scope delimiter. Duplicate
rows execute twice; write shared coverage once using `all`.

Run `ci/platform-tests/run.sh windows`, `macos`, or `tender-ssh`. The runner
validates the entire file before invoking Cargo and stops at the first failure.
It builds one union of the selected packages, targets and features with
`cargo --config 'profile.dev.package."*".debug=0' test --no-run`. Each subsequent
Cargo test invocation uses that same union. A native target runner dispatches only
the original selector's binaries, so Cargo supplies package working directories,
package variables and dynamic-library paths. The build writes Cargo HTML timings;
the runner logs build and selector durations separately.
No new selected test needs a workflow edit. Build, check and runtime smoke
commands, job isolation, runners, triggers and credentials stay in workflows.

Use Python 3.11+ for the action contracts (`tomllib`), with PyYAML from
`ci/platform-tests/requirements.txt`. The script-contracts job explicitly
sets up Python 3.12. Install the requirements, then run
`ci/platform-tests/test-run.sh` and
`python3 -m unittest discover -s ci/toolchain -p test_action.py` for the runner
and composite-action contracts. Tests fake only the Cargo process boundary;
they do not run platform tests on Linux. Set `SELECTOR_TEST_BASH=/bin/bash`
to exercise the runner with macOS's system Bash 3.2 (the existing macOS job does this).

Rows retain the `scope|whitespace-separated cargo test arguments` format. Supported
build arguments are `-p`/`--package`, `--features`, `--locked`, `--lib`, `--bin`, and
`--test`; test harness arguments follow `--`. Require an explicit target on each
row, and select at most one package per row. Implicit targets also include doctests and potentially examples/benches, which
cannot be dispatched from the no-run artifact list; those rows fail before the
build instead of silently dropping coverage. Every checked-in row is explicit.

Windows Git Bash uses `python`; Unix uses `python3`. Cargo dispatch uses the exact
interpreter already running the selector planner. Keep all Cargo `--config`
arguments before the `test` subcommand, and identical between build and execution.

Features are also unified across rows: `ssh_adapter` is built with
`tender/ssh-cleat-proof` because the adjacent `ssh_cleat` row requests it. Selected
rows specify the job's test coverage; they do not prove each package works with
its row's features in isolation. Add a separate job only when isolated feature
coverage is deliberately required. Package and target unions can compile extra
matching targets in a future selector set; artifact dispatch still limits execution
to each original row. The current root package has no library target.
