#!/usr/bin/env bash
# Run the selector's arguments without eval, and report each phase separately.
set -euo pipefail
selection=${1:?usage: run.sh 'all | none | -p package ...'}
if [[ "$selection" == none ]]; then
  # The selector found no Rust test input among the changed files.
  echo 'No Rust test inputs changed; skipping compile, tests and doctests.'
  exit 0
fi
if [[ "$selection" == all ]]; then
  packages=(--workspace)
else
  read -r -a packages <<< "$selection"
  if (( ${#packages[@]} == 0 || ${#packages[@]} % 2 != 0 )); then
    echo 'invalid package selection' >&2
    exit 1
  fi
  for ((i=0; i<${#packages[@]}; i+=2)); do
    if [[ "${packages[i]}" != -p || ! "${packages[i+1]}" =~ ^[a-zA-Z0-9_-]+$ ]]; then
      echo 'invalid package selection: expected -p followed by a package name' >&2
      exit 1
    fi
  done
fi
# Cargo does not forward a --config given before an external subcommand, so
# nextest takes it after `nextest run`; both builds then share dependencies.
cargo_args=(--config 'profile.dev.package."*".debug=0')
TIMEFORMAT='compile_seconds=%R'
time cargo nextest run "${cargo_args[@]}" "${packages[@]}" --locked --no-run
TIMEFORMAT='run_seconds=%R'
status=0
time cargo nextest run "${cargo_args[@]}" "${packages[@]}" --locked || status=$?
# Cargo rejects explicit bin-only packages with --doc, even in a mixed set.
# Keep full-workspace semantics; filter only explicit affected selections.
doc_packages=("${packages[@]}")
if [[ "$selection" != all ]]; then
  names=()
  for ((i=1; i<${#packages[@]}; i+=2)); do names+=("${packages[i]}"); done
  script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  doc_selection=$(python3 "$script_dir/doctests.py" "${names[@]}")
  read -r -a doc_packages <<< "$doc_selection"
fi
if (( ${#doc_packages[@]} == 0 )); then
  echo 'No selected packages have enabled library doctests.'
  echo 'doctest_seconds=0.000' >&2
  exit "$status"
fi
TIMEFORMAT='doctest_seconds=%R'
time cargo "${cargo_args[@]}" test --doc "${doc_packages[@]}" --locked || status=$?
exit "$status"
