#!/usr/bin/env bash
# Seed a running flotillad that has `[dummy_fleet]` in its daemon.toml with the
# resources a dummy convoy needs before it can be admitted: the Fleet project
# and its designation, the dummy forge and repository, the `dummy-crews`
# fulfilment kind on this host, and the four dummy workflow templates.
#
# The documents come from `dummy-fleet-catalog` (crate flotilla-dummy-fleet),
# the catalog the test harnesses apply. They are applied in order with
# `flotilla resource apply --file`, then the script waits until the Repository
# has a default branch and the Host reports the dummy harness for
# `dummy-crews`; admission needs both.
#
# Run on the daemon's host as the daemon's user. The daemon must already be
# running: the script fails if ROOT/config/run/flotilla.sock is absent and
# never lets the CLI start a daemon. Re-running applies the same documents.
#
# Usage:
#   dummy-fleet-seed.sh --root DIR [--flotilla PATH] [--emitter PATH] \
#     [--host-ref NAME] [--long-runner-turns N] [--forge-service-url URL] \
#     [--out-dir DIR] [--wait SECONDS]
#
# --root is the daemon's root, as in `flotilla --root DIR`. --flotilla and
# --emitter default to `flotilla` and `dummy-fleet-catalog` on PATH.
# --host-ref defaults to the daemon's own Host resource. Pass
# --long-runner-turns and --forge-service-url when daemon.toml sets
# `[dummy_fleet.agent] long_runner_turns` or `[dummy_fleet] forge_service_url`;
# they default to the daemon's defaults. --out-dir keeps the documents
# (default: a temporary directory). --wait bounds the readiness wait (120).
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit "${1:-2}"; }
die() { echo "dummy-fleet-seed: $*" >&2; exit 1; }

root="" flotilla=flotilla emitter=dummy-fleet-catalog host_ref="" turns="" forge_url="" out_dir="" wait_secs=120
while (($#)); do
  case "$1" in
    --root) root=${2:?}; shift 2 ;;
    --flotilla) flotilla=${2:?}; shift 2 ;;
    --emitter) emitter=${2:?}; shift 2 ;;
    --host-ref) host_ref=${2:?}; shift 2 ;;
    --long-runner-turns) turns=${2:?}; shift 2 ;;
    --forge-service-url) forge_url=${2:?}; shift 2 ;;
    --out-dir) out_dir=${2:?}; shift 2 ;;
    --wait) wait_secs=${2:?}; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "dummy-fleet-seed: unknown argument $1" >&2; usage ;;
  esac
done

[[ -n $root ]] || die "--root is required: the daemon's root directory"
[[ -d $root ]] || die "--root $root is not a directory"
[[ $wait_secs =~ ^[1-9][0-9]*$ ]] || die "--wait takes a positive number of seconds"
[[ -z $turns || $turns =~ ^[0-9]+$ ]] || die "--long-runner-turns takes a number"
for tool in "$flotilla" "$emitter" python3; do command -v "$tool" >/dev/null || die "$tool is required"; done
socket=$root/config/run/flotilla.sock
[[ -S $socket ]] || die "no daemon socket at $socket; start flotillad for this root first"

# The CLI talks only to this root's daemon: no inherited socket, remote daemon
# or root, and the marker that makes it refuse to start a daemon of its own.
cli() {
  env -u FLOTILLA_DAEMON_SOCKET -u FLOTILLA_DAEMON -u FLOTILLA_ROOT FLOTILLA_CONTAINED_HOST_DAEMON=1 \
    "$flotilla" --root "$root" --json "$@" </dev/null
}

if [[ -z $host_ref ]]; then
  hosts=$(cli resource list Host --local-only) || die "could not list this daemon's Host resource"
  host_ref=$(python3 -c '
import json, sys
names = [r["object"]["metadata"]["name"] for r in json.load(sys.stdin)["records"] if r.get("object")]
if len(names) != 1:
    sys.exit("expected one local Host, found %d: %s" % (len(names), ", ".join(names)))
print(names[0])' <<<"$hosts") || die "could not tell the local Host's name; pass --host-ref NAME"
fi

if [[ -z $out_dir ]]; then out_dir=$(mktemp -d "${TMPDIR:-/tmp}/dummy-fleet-catalog.XXXXXX"); fi
emit=("$emitter" --host-ref "$host_ref" --out-dir "$out_dir")
if [[ -n $turns ]]; then emit+=(--long-runner-turns "$turns"); fi
if [[ -n $forge_url ]]; then emit+=(--forge-service-url "$forge_url"); fi
documents=()
while IFS= read -r document; do documents+=("$document"); done < <("${emit[@]}")
((${#documents[@]})) || die "$emitter wrote no documents"

repository=""
for document in "${documents[@]}"; do
  read -r kind name < <(python3 -c '
import json, sys
document = json.load(open(sys.argv[1]))
print(document["kind"], document["metadata"]["name"])' "$document")
  cli resource apply --file "$document" >/dev/null || die "applying $kind $name ($document) failed"
  echo "applied $kind $name"
  if [[ $kind == Repository ]]; then repository=$name; fi
done
[[ -n $repository ]] || die "the catalog holds no Repository"

# Prints what is still missing; nothing when the host can admit dummy convoys.
missing() {
  local repo host
  repo=$(cli resource get Repository "$repository" 2>/dev/null) || repo='{"records":[]}'
  host=$(cli resource get Host "$host_ref" 2>/dev/null) || host='{"records":[]}'
  python3 -c '
import json, sys
def status(text):
    for record in json.loads(text).get("records", []):
        if record.get("object"):
            return record["object"].get("status") or {}
    return {}
missing = []
if not status(sys.argv[1]).get("default_branch"):
    missing.append("Repository %s has no status.default_branch" % sys.argv[3])
facts = (status(sys.argv[2]).get("fulfilment_facts") or {}).get("dummy-crews") or {}
if "dummy" not in (facts.get("harnesses") or {}):
    missing.append("Host %s does not report the dummy harness for dummy-crews" % sys.argv[4])
print("; ".join(missing))' "$repo" "$host" "$repository" "$host_ref"
}

deadline=$((SECONDS + wait_secs))
while :; do
  still=$(missing)
  [[ -n $still ]] || break
  ((SECONDS < deadline)) || die "not ready after ${wait_secs}s: $still. Check that daemon.toml has [dummy_fleet] and see the daemon log."
  sleep 1
done
echo "Host $host_ref is seeded for dummy convoys (documents in $out_dir)."
