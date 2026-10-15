#!/usr/bin/env bash
# Subprocess boundary fake: validate allocation ownership and teardown without a live host.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
scratch=$(mktemp -d)
cat > "$scratch/provider" <<'PROVIDER'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$1" >> "$DEEP_CONTRACT_LOG"
case "$1" in
  start) exit "${DEEP_CONTRACT_START_STATUS:-0}" ;;
  run) test -s "$3"; touch "${3%/*}/extra-file"; if [ "${DEEP_CONTRACT_HANG:-0}" = 1 ]; then sleep 3; fi; exit "${DEEP_CONTRACT_RUN_STATUS:-0}" ;;
  collect) mkdir -p "$3"; exit "${DEEP_CONTRACT_COLLECT_STATUS:-0}" ;;
  stop) exit "${DEEP_CONTRACT_STOP_STATUS:-0}" ;;
esac
PROVIDER
chmod +x "$scratch/provider"
export DEEP_CONTRACT_LOG="$scratch/calls"
# Collect only once run is reached; always stop, including a partial start.
for phase in success start run collect stop timeout; do
  export DEEP_CONTRACT_START_STATUS=0 DEEP_CONTRACT_RUN_STATUS=0 DEEP_CONTRACT_STOP_STATUS=0 DEEP_CONTRACT_COLLECT_STATUS=0 DEEP_CONTRACT_HANG=0 DEEP_RUN_SECONDS=900
  case "$phase" in
    start) export DEEP_CONTRACT_START_STATUS=23 ;;
    run) export DEEP_CONTRACT_RUN_STATUS=23 ;;
    collect) export DEEP_CONTRACT_COLLECT_STATUS=23 ;;
    timeout) export DEEP_CONTRACT_HANG=1 DEEP_RUN_SECONDS=1 ;;
    stop) export DEEP_CONTRACT_STOP_STATUS=23 ;;
  esac
  : > "$DEEP_CONTRACT_LOG"
  result=0
  "$here/deep.sh" "$scratch/provider" "$scratch/out" > "$scratch/$phase.out" 2>&1 || result=$?
  case "$phase" in success) test "$result" = 0 ;; start|run) test "$result" = 23 ;; collect|stop) test "$result" = 1 ;; timeout) test "$result" = 124 ;; esac
  if [ "$phase" = start ]; then printf 'start\nstop\n' > "$scratch/expected";
  else printf 'start\nrun\ncollect\nstop\n' > "$scratch/expected"; fi
  cmp "$scratch/expected" "$DEEP_CONTRACT_LOG"
done
printf 'deep allocation lifecycle contracts passed\n'
