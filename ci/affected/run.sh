#!/usr/bin/env bash
# Run the selector's arguments without eval, and report each phase separately.
set -euo pipefail
selection=${1:?usage: run.sh 'all | -p package ...'}
if [[ "$selection" == all ]]; then
  packages=(--workspace)
else
  read -r -a packages <<< "$selection"
  if (( ${#packages[@]} == 0 || ${#packages[@]} % 2 != 0 )); then
    echo 'invalid package selection' >&2
    exit 1
  fi
  for ((i=0; i<${#packages[@]}; i+=2)); do
    [[ "${packages[i]}" == -p && "${packages[i+1]}" =~ ^[a-zA-Z0-9_-]+$ ]] || exit 1
  done
fi
cargo_args=(--config 'profile.dev.package."*".debug=0')
TIMEFORMAT='compile_seconds=%R'
time cargo "${cargo_args[@]}" nextest run "${packages[@]}" --locked --no-run
TIMEFORMAT='run_seconds=%R'
time cargo "${cargo_args[@]}" nextest run "${packages[@]}" --locked
TIMEFORMAT='doctest_seconds=%R'
time cargo "${cargo_args[@]}" test --doc "${packages[@]}" --locked
