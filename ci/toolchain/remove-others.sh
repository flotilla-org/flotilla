#!/usr/bin/env bash
# Uninstall every rustup toolchain except the active pin. rust-cache hashes all
# installed toolchains into its keys, so the toolchains a runner image happens
# to preinstall would otherwise split one cache by image version (#3750).
set -euo pipefail
active=$(rustup show active-toolchain)
active=${active%$'\r'}
active=${active%% *}
rustup toolchain list --quiet | while IFS= read -r toolchain; do
  toolchain=${toolchain%$'\r'}
  toolchain=${toolchain%% *}
  if [[ -n "$toolchain" && "$toolchain" != "$active" ]]; then
    rustup toolchain uninstall "$toolchain"
  fi
done
