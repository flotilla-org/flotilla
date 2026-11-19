#!/usr/bin/env bash
# Dispatch dummy convoys to a running flotillad seeded by dummy-fleet-seed.sh,
# cycling through the four dummy workflow templates, as the dummy fleet load
# test dispatches them:
#
#   flotilla --root ROOT convoy NAME create --template TEMPLATE \
#     --project fleet --ref dummy/NAME --fulfilment dummy-crews
#
# Each convoy belongs to the Fleet project, which lists the dummy repository.
# Prints one line per convoy and exits non-zero if any dispatch was refused.
# The daemon must already be running: the script fails if
# ROOT/config/run/flotilla.sock is absent and never lets the CLI start one.
#
# Usage:
#   dummy-fleet-dispatch.sh --root DIR --count N [--prefix dummy] [--start 1] \
#     [--flotilla PATH]
#
# Convoys are named PREFIX-NNNN from --start, so a second batch on the same
# fleet needs another --prefix or --start.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit "${1:-2}"; }
die() { echo "dummy-fleet-dispatch: $*" >&2; exit 1; }

root="" count="" prefix=dummy start=1 flotilla=flotilla
while (($#)); do
  case "$1" in
    --root) root=${2:?}; shift 2 ;;
    --count) count=${2:?}; shift 2 ;;
    --prefix) prefix=${2:?}; shift 2 ;;
    --start) start=${2:?}; shift 2 ;;
    --flotilla) flotilla=${2:?}; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "dummy-fleet-dispatch: unknown argument $1" >&2; usage ;;
  esac
done

[[ -n $root ]] || die "--root is required: the daemon's root directory"
[[ -d $root ]] || die "--root $root is not a directory"
[[ $count =~ ^[1-9][0-9]*$ ]] || die "--count takes a positive number of convoys"
[[ $start =~ ^[0-9]+$ ]] || die "--start takes a number"
# The name becomes a resource name and a branch name.
[[ $prefix =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ ]] || die "--prefix '$prefix' must be lowercase letters, digits and '-'"
command -v "$flotilla" >/dev/null || die "$flotilla is required"
socket=$root/config/run/flotilla.sock
[[ -S $socket ]] || die "no daemon socket at $socket; start flotillad for this root first"

# The CLI talks only to this root's daemon: no inherited socket, remote daemon
# or root, and the marker that makes it refuse to start a daemon of its own.
cli() {
  env -u FLOTILLA_DAEMON_SOCKET -u FLOTILLA_DAEMON -u FLOTILLA_ROOT FLOTILLA_CONTAINED_HOST_DAEMON=1 \
    "$flotilla" --root "$root" --json "$@" </dev/null
}

templates=(dummy-coder dummy-coder-reviewer dummy-long-runner dummy-coder-supervisor)
failed=0
for ((i = 0; i < count; i++)); do
  name=$(printf '%s-%04d' "$prefix" $((10#$start + i)))
  template=${templates[i % ${#templates[@]}]}
  if output=$(cli convoy "$name" create --template "$template" --project fleet --ref "dummy/$name" --fulfilment dummy-crews 2>&1); then
    echo "dispatched $name $template"
  else
    failed=$((failed + 1))
    echo "refused $name $template: $(tr '\n' ' ' <<<"$output")"
  fi
done
((failed == 0)) || die "$failed of $count convoys were refused"
