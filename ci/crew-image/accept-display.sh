#!/usr/bin/env bash
# Operator acceptance on a Docker host, from the branch checkout.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
acceptance_dir="$(mktemp -d)"
trap 'rm -rf "$acceptance_dir"' EXIT
image="${FLOTILLA_ACCEPTANCE_IMAGE:-flotilla-crew-display:acceptance}"
python3 "$repo_root/ci/crew-image/compose.py" --display > "$acceptance_dir/Dockerfile"
docker buildx build --builder default --load --progress=plain \
    --file "$acceptance_dir/Dockerfile" --tag "$image" "$repo_root"
# Identical sourced prelude contract to the daemon's build probe wrapper.
docker run --rm --network=none --pull=never --user "$(id -u):$(id -g)" \
    --entrypoint sh "$image" -c "$(cat "$repo_root/ci/crew-image/prelude.sh")
xdpyinfo && glxinfo -B && test \"\$LIBGL_ALWAYS_SOFTWARE\" = 1"
