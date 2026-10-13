#!/usr/bin/env bash
set -euo pipefail
# Host-side, lowest-level proof: real PTYs, daemon sockets, SCM_RIGHTS and
# redirected attachments; no Docker or live fleet. The pinned cleat checkout
# owns this protocol-level scenario, including the deliberate previous SHA.
cleat_source=${1:?usage: test-tools-handover.sh <pinned-cleat-source>}
cargo test --manifest-path "$cleat_source/Cargo.toml" -p cleat --locked \
  --no-default-features --test transfer \
  handover_
