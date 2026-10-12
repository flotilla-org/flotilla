#!/bin/sh
# Version checks plus executable contracts; no home-directory installation.
set -eu
# Apalache writes logs relative to cwd; probes must work from read-only roots.
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' 0
trap 'exit 1' HUP INT TERM
cd "$work_dir"
spec=/opt/flotilla/formal-modelling/smoke.qnt
run() {
    quint run "$spec" --backend rust --max-samples 100 --max-steps 10 --seed 1 --invariant bounded
}
verify() {
    quint verify "$spec" --backend tlc --invariant bounded
}
case "${1:-all}" in
    jdk) java -version 2>&1 | grep -F 'Temurin-21.0.12.1+1' ;;
    quint) test "$(quint --version)" = 0.33.0; run; verify ;;
    evaluator) run ;;
    tlc) tlc -help 2>&1 | grep -F 'Version 2.19'; verify ;;
    apalache) apalache-mc version | grep -F '0.62.1' ;;
    all) for tool in jdk quint evaluator tlc apalache; do "$0" "$tool"; done ;;
    *) echo "unknown formal-modelling probe: $1" >&2; exit 2 ;;
esac
