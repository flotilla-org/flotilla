#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build
for mutant in expiry_without_watermark release_only_realised duplicate_turn bind_without_drain; do
  case "$mutant" in
    expiry_without_watermark) scenario=message_race; module=message; alias=M; directory=machines ;;
    release_only_realised) scenario=ensure_bind; module=lifecycle; alias=L; directory=machines ;;
    duplicate_turn|bind_without_drain) scenario=inbox_delivery; module=adapter; alias=D; directory=world ;;
  esac
  sed "s#import $module as $alias from \"../$directory/$module\"#import $mutant as $alias from \"../mutants/$mutant\"#" \
    "scenarios/$scenario.qnt" > "build/$mutant.qnt"
  if quint verify "build/$mutant.qnt" --main "$scenario" --backend tlc --invariant safety \
      --tlc-config mutants/tlc.json > "build/$mutant.log" 2>&1; then
    echo "mutant survived: $mutant" >&2
    exit 1
  fi
  grep -q 'Invariant.*violated' "build/$mutant.log" || { cat "build/$mutant.log"; exit 1; }
  echo "caught: $mutant"
done
