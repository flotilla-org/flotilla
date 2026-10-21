#!/usr/bin/env bash
set -euo pipefail
# This is an agent stand-in, launched through the normal managed agent path.
# Keep evidence outside the checkout so ordinary teardown safety checks apply.
# The optional directory argument is the filesystem seam used by contract tests.
probe_dir="${1:-/tmp}"
python3 - <<'PY' >"$probe_dir/fleet-canary-report.json"
import json
import os
from pathlib import Path
import subprocess

home = Path(os.environ['CLAUDE_CONFIG_DIR'])


def version(tool):
    # A runnable toolchain is the contract; images may place it anywhere on PATH.
    # Failures report why, so the baseline refusal can name the cause.
    try:
        result = subprocess.run([tool, '--version'], capture_output=True, text=True)
    except OSError as error:
        return f'not runnable: {error}'
    if result.returncode != 0:
        return f'exit {result.returncode}: {result.stderr.strip()}'
    return result.stdout.strip()


print(json.dumps({
    'environment': dict(os.environ),
    'git': {key: subprocess.run(['git', 'config', '--get', key], capture_output=True, text=True).stdout.strip()
            for key in ['user.name', 'user.email', 'push.default']} | {
                key: subprocess.run(['git', 'var', key], capture_output=True, text=True).stdout.strip()
                for key in ['GIT_AUTHOR_IDENT', 'GIT_COMMITTER_IDENT']},
    'skills': sorted(str(path.relative_to(home / 'skills'))
                     for path in (home / 'skills').rglob('SKILL.md')),
    'toolchain': {tool: version(tool) for tool in ['cargo', 'rustc']},
}))
PY
# The installer observes Running and verifies the report before releasing settlement.
while [[ ! -e "$probe_dir/fleet-canary-continue" ]]; do sleep 1; done
cat >"$probe_dir/fleet-canary-ledger.md" <<'LEDGER'
## Decision ledger

1. **Brief silence:** How the credential-free probe proves settlement.
- **Choice:** Keep a decision-ledger promise after the installer verifies the baseline report.
- **Alternative:** Retract a crew-sourced promise without exercising artifact submission.
- **If asking were free:** Which forge-free deliverable should prove settlement?
LEDGER
# Declare the baseline evidence before publishing it: ledger observation submits
# and keeps both ledger promises. Publishing last avoids racing automatic Done
# with a second CLI call after the artifact is visible.
flotilla crew promise --promise baseline --kind decision-ledger
flotilla artifact put --kind decision-ledger "$probe_dir/fleet-canary-ledger.md"
# Stay alive until the normal convoy finalizers reap the terminal/container.
while true; do sleep 1; done
