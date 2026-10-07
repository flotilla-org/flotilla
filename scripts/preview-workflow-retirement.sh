#!/usr/bin/env bash
# Run on each fleet host before installing the candidate. No daemon writes.
set -euo pipefail
if [[ $# -ne 2 ]]; then
    echo "usage: $0 /absolute/path/to/candidate /absolute/path/to/new-output-directory" >&2
    exit 2
fi
candidate=$1
output=$2
mkdir "$output"
validation_status=0
"$candidate" resource validate --from-daemon > "$output/validation.txt" || validation_status=$?
export_status=0
python3 - "$output" <<'PY' || export_status=$?
import json
import pathlib
import sys

output = pathlib.Path(sys.argv[1])
prefix = 'workflow retirement preview: '
reports = [json.loads(line[len(prefix):]) for line in (output / 'validation.txt').read_text().splitlines() if line.startswith(prefix)]
if len(reports) != 1:
    raise SystemExit(f'expected exactly one retirement preview; inspect {output / "validation.txt"}')
report = reports[0]
(output / 'retirement.json').write_text(json.dumps(report, indent=2) + '\n')
for index, definition in enumerate(report['definitions']):
    (output / f'restore-{index}.json').write_text(json.dumps(definition, indent=2) + '\n')
print(f"Exported {len(report['definitions'])} retiring definitions; review {output / 'retirement.json'}")
PY
if [[ "$validation_status" -ne 0 ]]; then
    exit "$validation_status"
fi
exit "$export_status"
