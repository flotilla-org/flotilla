#!/usr/bin/env bash
# Local acceptance only: build through the existing composition harness.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
acceptance_dir="$(mktemp -d)"
trap 'rm -rf "$acceptance_dir"' EXIT
image="${FLOTILLA_ACCEPTANCE_IMAGE:-flotilla-crew-formal-modelling:acceptance}"
python3 "$repo_root/ci/crew-image/compose.py" --formal-modelling > "$acceptance_dir/Dockerfile"
docker buildx build --builder default --load --progress=plain \
    --file "$acceptance_dir/Dockerfile" --tag "$image" "$repo_root"
# An arbitrary UID and unwritable home expose accidental ~/.quint downloads.
# spine-base installs Node under /usr/local, so npm's global quint executable
# is /usr/local/bin/quint. Keep this cleared PATH aligned with that prefix.
docker run --rm --network=none --pull=never --user 12345:12345 \
    --env HOME=/nonexistent --env QUINT_HOME=/nonexistent --env JAVA_HOME=/nonexistent \
    --env PATH=/usr/local/bin:/usr/bin:/bin --entrypoint sh "$image" -c "$(cat "$repo_root/ci/crew-image/prelude.sh")
flotilla_run_with_preludes /opt/flotilla/formal-modelling/probe.sh all"
