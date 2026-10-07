#!/usr/bin/env bash
# Live acceptance after rolling matching binaries and applying fleet subscriptions.
set -euo pipefail
if (( $# < 2 || $# > 3 )); then
    echo "usage: $0 CREW_ID EXPECTED_FIRST_SUPERVISOR [--stall]" >&2
    exit 2
fi
crew_id=$1
expected=$2
contacts_file=$(mktemp)
trap 'rm -f "$contacts_file"' EXIT
flotilla --json message contacts --crew-id "$crew_id" > "$contacts_file"
python3 - "$contacts_file" "$expected" <<'PY'
import json, sys
value = json.load(open(sys.argv[1]))
book = value.get("book", value)
assert book["supervision"][0]["address"] == sys.argv[2], book
print(json.dumps(book, indent=2))
PY
if [[ ${3:-} == --stall ]]; then
    flotilla crew stall --crew-id "$crew_id" --reason decision --message 'Disposable convoy: supervision routing acceptance'
    flotilla --json resource list Message
fi
