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
print(json.dumps({
    'environment': dict(os.environ),
    'git': {key: subprocess.run(['git', 'config', '--get', key], capture_output=True, text=True).stdout.strip()
            for key in ['user.name', 'user.email', 'push.default']} | {
                key: subprocess.run(['git', 'var', key], check=True, capture_output=True, text=True).stdout.strip()
                for key in ['GIT_AUTHOR_IDENT', 'GIT_COMMITTER_IDENT']},
    'skills': sorted(str(path.relative_to(home / 'skills'))
                     for path in (home / 'skills').rglob('SKILL.md')),
}))
PY
# The installer observes Running before releasing the completion claim.
while [[ ! -e "$probe_dir/fleet-canary-continue" ]]; do sleep 1; done
flotilla crew complete --message 'fleet canary baseline verified'
# Stay alive until the normal convoy finalizers reap the terminal/container.
while true; do sleep 1; done
