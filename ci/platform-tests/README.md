# Selected platform tests

`selectors.txt` owns the selected Cargo tests. Each row is `scope|arguments`:
`windows`, `macos`, `all` (both desktop OSes), or `tender-ssh` (the isolated
real-SSH proof). Blank lines and full-line comments are ignored. Arguments are
whitespace-separated literal argv, with no shell quoting or expansion. Duplicate
rows execute twice; write shared coverage once using `all`.

Run `ci/platform-tests/run.sh windows`, `macos`, or `tender-ssh`. The runner
validates the entire file before invoking Cargo and stops at the first failure.
It adds `cargo --config 'profile.dev.package."*".debug=0' test` to each row.
No new selected test needs a workflow edit. Build, check and runtime smoke
commands, job isolation, runners, triggers and credentials stay in workflows.

Install `ci/platform-tests/requirements.txt`, then run
`ci/platform-tests/test-run.sh` and
`python3 -m unittest discover -s ci/toolchain -p test_action.py` for the runner
and composite-action contracts. Tests fake only the Cargo process boundary;
they do not run platform tests on Linux. Set `SELECTOR_TEST_BASH=/bin/bash`
to exercise the runner with macOS's system Bash 3.2 (the operator workflow
diff does this in the existing macOS job).
