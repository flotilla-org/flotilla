#!/usr/bin/env bash
# Operator-only live acceptance. Run on each host before fleet-install activation.
# The script reads the daemon and writes evidence; it never rolls the fleet.
set -euo pipefail
if [[ $# -lt 3 || $# -gt 4 ]]; then
  echo "usage: $0 /absolute/candidate-release /absolute/evidence-dir /absolute/probe-token-map.json [host]" >&2
  exit 2
fi
release=$1
evidence=$2
tokens=$3
host=${4:-}
mkdir "$evidence"
args=(resource validate --from-daemon --skill-sources "$release/share/flotilla/skills"
      --skill-catalog "$release/share/flotilla/skills/.flotilla-skill-catalog.json" --skill-probe-tokens "$tokens")
if [[ -n "$host" ]]; then args+=(--host "$host"); fi
status=0
"$release/bin/flotilla" "${args[@]}" > "$evidence/validation.txt" 2> "$evidence/refusals.txt" || status=$?
export_status=0
python3 - "$evidence" <<'PY' || export_status=$?
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
prefix = 'frozen-reference satisfiability: '
reports = [json.loads(line[len(prefix):]) for line in (root / 'validation.txt').read_text().splitlines() if line.startswith(prefix)]
if len(reports) != 1:
    raise SystemExit('no complete frozen-reference report; inspect validation.txt and refusals.txt')
(root / 'frozen-references.json').write_text(json.dumps(reports[0], indent=2) + '\n')
print(json.dumps(reports[0], indent=2))
if reports[0].get('inventory_complete') is not True:
    raise SystemExit('frozen-reference inventory is incomplete; activation is refused')
PY
if [[ "$status" -ne 0 ]]; then exit "$status"; fi
exit "$export_status"
