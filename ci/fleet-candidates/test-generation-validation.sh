#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixtures="$root/fixtures"
tools=(
  "$root/lab-fleet-promote"
  "$root/lab-fleet-finalize-darwin"
  "$root/lab-darwin-sign"
)
for tool in "${tools[@]}"; do
  python3 "$tool" --validate-fixture "$fixtures/valid.json"
  # A generation predating the CODEX_HOME template stays valid, so a failed
  # health check can still roll back onto one.
  python3 "$tool" --validate-fixture "$fixtures/pre-codex-home.json"
  for fixture in bad-pin bad-skill-path traversing-skill-path unexpected-payload source-set-mismatch v4-bundle-violation codex-credential-payload codex-credential-directory; do
    if python3 "$tool" --validate-fixture "$fixtures/$fixture.json" >/dev/null 2>&1; then
      echo "$(basename "$tool") accepted invalid fixture $fixture" >&2
      exit 1
    fi
  done
done

FLEET_GENERATION_VALIDATOR="$root/generation_validation.py" "$root/../../scripts/fleet-install" __validate_fixture "$fixtures/valid.json"
FLEET_GENERATION_VALIDATOR="$root/generation_validation.py" "$root/../../scripts/fleet-install" __validate_fixture "$fixtures/pre-codex-home.json"
for fixture in bad-pin bad-skill-path traversing-skill-path unexpected-payload source-set-mismatch v4-bundle-violation codex-credential-payload codex-credential-directory; do
  if FLEET_GENERATION_VALIDATOR="$root/generation_validation.py" "$root/../../scripts/fleet-install" __validate_fixture "$fixtures/$fixture.json" >/dev/null 2>&1; then
    echo "fleet-install accepted invalid fixture $fixture" >&2
    exit 1
  fi
done

# The build-time gate on the assembled CODEX_HOME template: the directory is
# copied into every crew's writable CODEX_HOME, so a credential, a missing
# config.toml, or a symlink escaping the generation must never become payload.
codex_home_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-codex-home.XXXXXX")"
install_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-validator-layout.XXXXXX")"
trap 'rm -rf "$codex_home_root" "$install_root"' EXIT
template="$codex_home_root/codex-home"
mkdir -p "$template/prompts"
printf '# template\n' >"$template/config.toml"
printf 'static default\n' >"$template/prompts/review.md"

accept_template() {
  python3 "$root/generation_validation.py" codex-home "$1"
}

reject_template() {
  local candidate="$1"
  local description="$2"
  if python3 "$root/generation_validation.py" codex-home "$candidate" >/dev/null 2>&1; then
    echo "codex home validation accepted $description" >&2
    exit 1
  fi
}

accept_template "$template"
reject_template "$codex_home_root/missing" 'a declared template that is not on disk'
printf '{"tokens":{}}\n' >"$template/auth.json"
reject_template "$template" 'a template carrying auth.json'
rm "$template/auth.json"
printf '{"tokens":{}}\n' >"$template/prompts/auth.json"
reject_template "$template" 'a template carrying a nested auth.json'
rm "$template/prompts/auth.json"
# seed_scratch copies the template wholesale, so a *directory* named auth.json
# lands in the crew's home as one — with or without anything inside it.
mkdir "$template/auth.json"
reject_template "$template" 'a template carrying an empty directory named auth.json'
printf 'token\n' >"$template/auth.json/refresh"
reject_template "$template" 'a template hiding a credential inside a directory named auth.json'
rm -r "$template/auth.json"
ln -s /etc/passwd "$template/escape"
reject_template "$template" 'a template carrying a symlink'
rm "$template/escape"
mv "$template/config.toml" "$template/config.toml.disabled"
reject_template "$template" 'a template without config.toml'
mv "$template/config.toml.disabled" "$template/config.toml"
accept_template "$template"

# The repository's own template is what the candidate build bakes in.
accept_template "$root/../../share/flotilla/codex-home"

cp "$root/../../scripts/fleet-install" "$root/generation_validation.py" "$install_root/"
PATH="$(dirname "$(command -v python3)"):$PATH" "$install_root/fleet-install" __validate_fixture "$fixtures/valid.json"

echo "generation validator parity passed"
