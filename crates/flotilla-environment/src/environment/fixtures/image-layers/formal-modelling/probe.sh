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
    jdk) java -version > java-version.txt 2>&1; grep -F 'Temurin-21.0.12.1+1' java-version.txt ;;
    quint) test "$(quint --version)" = 0.33.0; run; verify ;;
    # Each capability must independently probe its backend, even when the
    # aggregate probe has already exercised it through the Quint capability.
    evaluator) run ;;
    tlc)
        tlc -workers 1 -metadir "$work_dir/states" \
            -config /opt/flotilla/formal-modelling/smoke.cfg \
            /opt/flotilla/formal-modelling/smoke.tla > tlc-output.txt 2>&1 \
            || { status=$?; cat tlc-output.txt; exit "$status"; }
        cat tlc-output.txt
        # TLC release 1.7.4 reports engine version 2.19; update this check with
        # the `tlc` pin in layer.yaml when selecting another release.
        grep -F 'TLC2 Version 2.19' tlc-output.txt
        verify
        ;;
    apalache) apalache-mc version > apalache-version.txt; grep -F '0.62.1' apalache-version.txt ;;
    all) for tool in jdk quint evaluator tlc apalache; do "$0" "$tool"; done ;;
    *) echo "unknown formal-modelling probe: $1" >&2; exit 2 ;;
esac
