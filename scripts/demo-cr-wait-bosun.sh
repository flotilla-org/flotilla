#!/usr/bin/env bash
set -euo pipefail

# Run-3-lite of the #1321 Bosun decision table. The driver blocks only in the
# leaf engine: there is no shell sleep loop and therefore no repeated gh read.
if [[ $# -ne 1 ]]; then
  echo "usage: $0 cr/<service>/<scope>/<number>" >&2
  exit 2
fi

subject=$1
case "$subject" in
  cr/*/*/*) ;;
  *) echo "expected a cr/<service>/<scope>/<number> address" >&2; exit 2 ;;
esac

while true; do
  fired=$(flotilla --json wait \
    --for "$subject .state == merged" \
    --for "$subject .state == closed" \
    --for "$subject .checks == fail" \
    --for "$subject .review.actionable-at-head == true" \
    --for "$subject .mergeable == conflicting")
  path=$(jq -r '.leaf.field_path' <<<"$fired")
  value=$(jq -r '.value' <<<"$fired")

  case "$path:$value" in
    .state:merged)
      echo "DISPOSITION: merged"
      exit 0
      ;;
    .state:closed)
      echo "DISPOSITION: blocked (change request closed without merge)"
      exit 1
      ;;
    .checks:fail)
      echo "WAKE: coder — checks failed at the observed head"
      ;;
    .review.actionable-at-head:true)
      echo "WAKE: coder — actionable review exists at the observed head"
      ;;
    .mergeable:conflicting)
      echo "WAKE: coder — change request conflicts"
      ;;
    *)
      echo "unexpected leaf result: $fired" >&2
      exit 2
      ;;
  esac
done
