#!/usr/bin/env bash
# Live acceptance for #4300, run by an operator: a cloud vessel on its composed
# image (Daytona or Namespace) is provisioned through a throwaway local
# flotillad, the daemon is restarted with the crew running, and the restarted
# daemon must adopt the vessel: no adoption failure, the crew reaches the daemon
# through its crew socket again (its hooks' path), its screen is read again,
# credentials are delivered and attach works. Everything is torn down after.
#
#   scripts/accept-cloud-restart-adoption.sh --provider-config drydock.toml \
#       --image-cache image-cache.toml --forge forge.toml [--provider NAME]
#
# The files are those scripts/trial-fleet documents (`--help`). No live cloud
# acceptance runs in CI.
set -euo pipefail
if [[ " $* " != *" --provider-config "* || " $* " != *" --image-cache "* || " $* " != *" --forge "* ]]; then
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
fi
exec "$(dirname "$0")/trial-fleet" run --until attached --restart-daemon "$@"
