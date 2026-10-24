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
before-fix review prompt. The operator authorized opening the PR with this
limitation after one further same-home relaunch check. That check also did not
reproduce hook review before or after the fix. The fresh-crew check on feta
after the roll remains the operator acceptance step, tracked in
[follow-up #3827](https://github.com/flotilla-org/flotilla/issues/3827).

## Exact conditions tried

Every launch used the installed 0.160.0 binary, adapter-generated command,
`--no-daemon`, no explicit model, the same controlled tool brief, and the seven
`-c hooks.<Event>` definitions plus `notify=[]`, `features.hooks=true`, and all
seven enabled/trusted `hooks.state` records keyed to
`/<session-flags>/config.toml:<event>:0:0`. The hook command was unchanged;
PATH selected a recording stand-in for `flotilla`.

| Launch | Home | Command mode | Hook review |
| --- | --- | --- | --- |
| Before, fresh | `/tmp/codex-3823-before/home`; adapter installs seven home hooks and seven matching home trust records | `--dangerously-bypass-approvals-and-sandbox` | Absent |
| Before, fresh restricted | `/tmp/codex-3823-before-restricted/home`; same home hook/trust installation | Empty fulfilment grants; bypass flag absent | Absent (later tool sandbox failed and asked for execution approval) |
| After, fresh | `/tmp/codex-3823-after/home`; adapter writes workspace trust, no home hook definitions | `--dangerously-bypass-approvals-and-sandbox` | Absent |
| Before, exit/relaunch | Reuse `/tmp/codex-3823-before/home` after `/quit`, without restaging or modifying config/trust/auth | Identical saved command and launch environment | Absent |
| After, exit/relaunch | Reuse `/tmp/codex-3823-after/home` after `/quit`, without restaging or modifying config/trust/auth | Identical saved command and launch environment | Absent |

Both relaunches completed the same `printf hook-probe-3823` tool turn and
returned to the composer. The baseline still delivered each event twice;
the fixed relaunch delivered each once. See [before relaunch](before-relaunch.txt),
[after relaunch](after-relaunch.txt), and the adjacent `*-relaunch-events.jsonl`.
The fixed same-home relaunch starts a new conversation; it does not use
`codex resume`, because the requested check was the same launch command.

## Captures and native hook evidence

- [Before](before.txt): rendered `cleat capture codex-3823-before` after one
  completed turn, with the composer ready.
- [After](after.txt): the same capture after the fix and one completed turn.
- [Baseline discovery](before-hooks-list.json): 14 hooks, all trusted.
- [Fixed discovery](../../../crates/flotilla-agent/src/fixtures/codex-3823/hooks-list.json):
  seven hooks, all trusted and enabled. The [staged config](config.toml) and
  [launch plan](launch.json) here are the actual staged inputs, copied without editing.
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
