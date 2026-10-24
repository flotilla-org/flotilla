# Contained Codex launch evidence (#3823)

Recorded inside the assigned vessel on 2026-10-24 using the installed
`codex-cli 0.160.0`. The unpatched baseline is commit `2403498b2`.
Both launches used `CliAgentAdapter::prepare_with_environment` and `launch`,
through `crates/flotilla-agent/examples/stage_codex_crew.rs`. The staged roots
were `/tmp/codex-3823-before` and `/tmp/codex-3823-after`.

## Acceptance gap

The original duplicate delivery reproduces, but the reported **Hooks need
review** prompt does not. Both the unrestricted baseline and a separate
baseline with `fulfilment_grants = Some(empty)` start without hook review.
Codex discovery reports 14 enabled, trusted hooks on the unpatched baseline.
The captures are real observations, and do not claim to show the requested
before-fix review prompt. No PR was opened because that explicit prerequisite
has not been met. Reproduction guidance or a revised acceptance decision is
needed to finish delivery.

## Captures and native hook evidence

- [Before](before.txt): rendered `cleat capture codex-3823-before` after one
  completed turn, with the composer ready.
- [After](after.txt): the same capture after the fix and one completed turn.
- [Baseline discovery](before-hooks-list.json): 14 hooks, all trusted.
- [Fixed discovery](../../../crates/flotilla-agent/src/fixtures/codex-3823/hooks-list.json):
  seven hooks, all trusted and enabled. The adjacent `config.toml` and
  `launch.json` are the actual staged inputs, copied without editing.
- [Baseline sink](before-events.jsonl) and [fixed sink](after-events.jsonl):
  native stdin payloads and argv recorded at the `flotilla hook` process
  boundary. No production daemon was contacted.

The baseline completed one `printf hook-probe-3823` turn and quit: SessionStart,
UserPromptSubmit, PreToolUse, PostToolUse, Stop, and SessionEnd each arrived twice.
The fixed session completed that same turn, then started a second `sleep 30`
turn, received Escape, and quit. SessionStart, Stop, Interrupt, and SessionEnd
arrived once; UserPromptSubmit, PreToolUse, and PostToolUse arrived once per turn.
The native `turn_id` and `tool_use_id` fields distinguish the occurrences.

## Reproduce the staged launch

```sh
cargo run -p flotilla-agent --locked --example stage_codex_crew -- \
  /tmp/codex-3823-probe /opt/flotilla/harnesses/codex/0.160.0/bin/codex
```

Add `--restricted` after the binary path to exercise empty fulfilment grants.
Deliver a copy of the current crew's `auth.json` into the fresh home with mode
0600; do not publish it. Launch the exact `command` in `launch.json` with cleat,
using its `cwd` and staged `CODEX_HOME`. Set PATH to put a stand-in `flotilla`
first. The stand-in records `sys.argv[1:]` and parsed stdin as one JSON line to
`HOOK_SINK`; it exits successfully without extra output. All other inherited
model/credential environment stays the same. Use `--size 120x42`,
`--tag project=flotilla`, and `--tag purpose=probe`.

Capture the rendered grid with `cleat capture`. Separately record discovery:

```sh
python3 crates/flotilla-agent/examples/record_codex_hooks.py /tmp/codex-3823-probe
```

That script uses the staged home and all `-c` arguments from the actual launch
plan. Only the TUI-specific flags and prompt are replaced by `app-server`.
It does not replace the real TUI or tool-turn check.

## Regression and mutation checks

The combined-layer test runs real adapter preparation for both fresh and
previous-generation homes, with an unrelated user hook retained. It compares
all seven invocation definitions and trust states with the recorded discovery.

- Original installation (`install = true`): fails with
  `home duplicates flotilla hook codex pre-tool-use`.
- Removing the CLI trust-state override: fails on the absent trust table.
- Both mutations reverted: 114 agent tests pass, one existing ignored test.
- `cargo fmt --check` and affected-package Clippy with `-D warnings` pass.
