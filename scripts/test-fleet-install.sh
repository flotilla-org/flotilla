#!/usr/bin/env bash
set -euo pipefail
# This suite covers installation; the separate canary suite covers its gate.
export FLEET_INSTALL_SKIP_CANARY=1

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_root/scripts/fleet-install"
bash "$repo_root/scripts/test-fleet-host-guard.sh"
python3 "$repo_root/scripts/test-fleet-preroll-checks.py"
python3 -m unittest discover -s "$repo_root/ci/fleet-candidates" -p "test_bootstrap_*.py"
test_root="$(mktemp -d "${TMPDIR:-/tmp}/fleet-install-test.XXXXXX")"
cleanup() {
  chmod -R u+w "$test_root" 2>/dev/null || true
  rm -rf "$test_root"
}
trap cleanup EXIT

fail() {
  echo "fleet-install contract failed: $*" >&2
  exit 1
}

# An incomplete installer/module deployment refuses before creating fleet state,
# for both download selection and mutations (including rollback).
for operation in latest 20260815T210000Z-r1-f111111111111-caaaaaaaaaaaa rollback --prune; do
  if FLEET_INSTALL_ROOT="$test_root/missing-validator-root" \
    FLEET_GENERATION_VALIDATOR="$test_root/missing-validator.py" \
    bash "$installer" "$operation" >"$test_root/missing-validator.log" 2>&1; then
    fail "missing validator allowed $operation"
  fi
  grep -Fq 'generation validator is missing or unreadable:' "$test_root/missing-validator.log" \
    || fail 'missing validator did not explain the incomplete deployment'
  test ! -e "$test_root/missing-validator-root" || fail 'missing validator created fleet state'
done

file_sha256() {
  python3 "$repo_root/ci/fleet-candidates/generation_validation.py" sha256 -- "$1"
}

file_mode() {
  python3 "$repo_root/ci/fleet-candidates/generation_validation.py" mode -- "$1"
}

link_generation() {
  basename "$(readlink "$1")"
}

add_test_skills() {
  local bundle="$1"
  local rjw_revision="${2:-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb}"
  local mattpocock_repository="${3:-https://github.com/flotilla-org/mattpocock-skills.git}"
  local skills="$bundle/share/flotilla/skills"
  mkdir -p "$skills"
  printf '{"schema_version":5,"sources":[{"name":"mattpocock-skills","repository":"%s","revision":"2222222222222222222222222222222222222222","credential":"github-skills-fork"},{"name":"rjw-skills","repository":"https://github.com/rjwittams/rjw-skills.git","revision":"%s","paths":["plugins/rjw-sdlc/skills"]}]}\n' \
    "$mattpocock_repository" "$rjw_revision" >"$skills/.flotilla-sources.json"
}

add_test_codex_home() {
  local bundle="$1"
  local codex_home="$bundle/share/flotilla/codex-home"
  mkdir -p "$codex_home"
  printf '# test CODEX_HOME template\n' >"$codex_home/config.toml"
}

generation_one="20260815T210000Z-r1-f111111111111-caaaaaaaaaaaa"
generation_two="20260815T220000Z-r2-f222222222222-cbbbbbbbbbbbb"
generation_incomplete="20260815T225000Z-r9-f999999999999-cffffffffffff"
generation_handoff_failure="20260815T224000Z-r8-f888888888888-ceeeeeeeeeeee"
fixture_root="$test_root/packages"
fake_bin="$test_root/fake-bin"
mkdir -p "$test_root/home/.config/flotilla" "$fixture_root" "$fake_bin"
printf 'test-token\n' >"$test_root/home/.config/flotilla/fleet-reader-token"
chmod 0600 "$test_root/home/.config/flotilla/fleet-reader-token"

# Process boundary double: the fixture CLI stands in for the incoming binary's
# post-install subcommand; Rust tests cover cleat inventory and drain behavior.
add_post_install_fixture() {
  cat >>"$1" <<EOF
if [[ "\${1:-}" == --socket && "\${3:-}" == fleet && "\${4:-}" == check ]]; then
  if [[ -n "\${FLEET_CHECK_LOG:-}" ]]; then printf '%s|%s|%s\\n' "\$0" "\$*" "\${FLOTILLA_CONTAINED_HOST_DAEMON:-}" >>"\$FLEET_CHECK_LOG"; fi
  if [[ "\${FLEET_HEALTH_FAIL_FOR:-}" == "$2" ]]; then printf 'fixture readiness failure\\n' >&2; exit 1; fi
  if [[ -n "\${FLEET_HEALTH_GATE:-}" && ! -e "\${FLEET_HEALTH_GATE:-}" ]]; then exit 1; fi
  if [[ -n "\${WINDOW_FD_LOG:-}" ]] && { : >&9; } 2>/dev/null; then echo check >>"\$WINDOW_FD_LOG"; fi
  exit 0
fi
if [[ "\${1:-}" == --socket && "\${3:-}" == fleet && "\${4:-}" == spread ]]; then
  if [[ -n "\${FLEET_SPREAD_LOG:-}" ]]; then printf '%s|%s\\n' "\$0" "\$*" >>"\$FLEET_SPREAD_LOG"; fi
  [[ -n "\${FLEET_SPREAD_TEXT:-}" ]] || exit 1
  printf '%s\\n' "\$FLEET_SPREAD_TEXT"
  exit 0
fi
EOF
  cat >>"$1" <<'SH'
if [[ "${1:-}" == fleet && "${2:-}" == prune ]]; then
  if [[ -n "${PRUNE_LOG:-}" ]]; then printf '%s|%s\n' "$0" "$*" >>"$PRUNE_LOG"; fi
  exit "${PRUNE_EXIT:-0}"
fi
if [[ "${1:-}" == --socket && "${3:-}" == --json && "${4:-}" == fleet && "${5:-}" == post-install ]]; then
  if [[ -n "${POST_INSTALL_LOG:-}" ]]; then printf '%s|%s\n' "$0" "$*" >>"$POST_INSTALL_LOG"; fi
  printf '%s\n' '{"kind":"fleet_post_install","report":"post-install fixture report"}'
  exit "${POST_INSTALL_EXIT:-0}"
fi
SH
}

make_generation() {
  local generation="$1"
  local protocol="$2"
  local platform="${3:-linux-x86_64-gnu2.36}"
  local corrupt_inner="${4:-no}"
  local skill_revision="${5:-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb}"
  local skill_repository="${6:-https://github.com/flotilla-org/mattpocock-skills.git}"
  local installer_behavior="${7:-activate}"
  local directory="$fixture_root/$generation"
  local bundle="$test_root/bundle-$generation/fleet-candidate-linux-x86_64-gnu2.36"
  mkdir -p "$directory" "$bundle/bin" "$bundle/lib"
  for name in flotilla flotillad cleat; do
    printf '#!/usr/bin/env bash\nif [[ "${1:-}" == daemon && "${2:-}" == stop ]]; then echo "daemon stop requested"; exit "${STOP_FAIL:-0}"; fi\n' >"$bundle/bin/$name"
    if [[ "$name" == flotilla ]]; then
      cat >>"$bundle/bin/$name" <<EOF
if [[ "\${1:-}" == --socket && "\${3:-}" == resource && "\${4:-}" == validate && -n "\${FLEET_VALIDATION_LOG:-}" ]]; then
  printf '%s\\n' "\$@" >>"\$FLEET_VALIDATION_LOG"
fi
if [[ "\${1:-}" == --socket && "\${3:-}" == resource && "\${4:-}" == validate && "\${5:-}" == --from-daemon && "\${FLEET_VALIDATE_FAIL_FOR:-}" == "$generation" ]]; then
  exit 1
fi
EOF
    fi
    if [[ "$name" == flotilla ]]; then add_post_install_fixture "$bundle/bin/$name" "$generation"; fi
    printf 'printf "%s from %s\\n"\n' "$name" "$generation" >>"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty\n' >"$bundle/lib/libghostty-vt.so.0"
  if [[ "$installer_behavior" == fail ]]; then
    printf '#!/usr/bin/env bash\nprintf "incoming installer refused activation\\n" >&2\nexit 42\n' >"$bundle/install.sh"
  else
    cp "$installer" "$bundle/install.sh"
  fi
  cp "$repo_root/ci/fleet-candidates/generation_validation.py" "$bundle/generation_validation.py"
  chmod 0755 "$bundle/install.sh" "$bundle/generation_validation.py"
  add_test_skills "$bundle" "$skill_revision" "$skill_repository"
  add_test_codex_home "$bundle"
TEST_BUNDLE="$bundle" TEST_PLATFORM="$platform" TEST_PROTOCOL="$protocol" TEST_CORRUPT_INNER="$corrupt_inner" python3 "$repo_root/scripts/fleet_install_test_support.py" linux-release
  local artifact="fleet-candidate-linux-x86_64-gnu2.36.tar.gz"
  COPYFILE_DISABLE=1 tar -C "$(dirname "$bundle")" -czf "$directory/$artifact" "$(basename "$bundle")"
  local digest
  digest="$(file_sha256 "$directory/$artifact")"
  local size
  size="$(python3 "$repo_root/ci/fleet-candidates/generation_validation.py" size -- "$directory/$artifact")"
  TEST_GENERATION="$generation" TEST_PROTOCOL="$protocol" TEST_PLATFORM="$platform" TEST_ARTIFACT="$artifact" TEST_DIGEST="$digest" TEST_SIZE="$size" TEST_DIRECTORY="$directory" python3 "$repo_root/scripts/fleet_install_test_support.py" linux-generation
}

add_darwin_derivative() {
  local generation="$1"
  local source_generation="$2"
  local protocol="$3"
  local directory="$fixture_root/$generation"
  local bundle="$test_root/darwin-$generation/fleet-signed-darwin-aarch64"
  mkdir -p "$bundle/bin" "$bundle/lib"
  for name in flotilla flotillad cleat; do
    printf '#!/usr/bin/env bash\n' >"$bundle/bin/$name"
    if [[ "$name" == flotilla ]]; then
      cat >>"$bundle/bin/$name" <<'SH'
if [[ "${1:-}" == --socket && "${3:-}" == --config-dir ]]; then
  [[ "$2" == "$XDG_CONFIG_HOME/flotilla/run/flotilla.sock" ]] || exit 97
  [[ "$4" == "$XDG_CONFIG_HOME/flotilla" ]] || exit 97
  [[ "$5" == --state-dir && "$6" == "$XDG_STATE_HOME/flotilla" ]] || exit 97
  [[ "${7:-}" == daemon && "${8:-}" == stop ]] || exit 97
  shift 6
fi
if [[ "${1:-}" == daemon && "${2:-}" == stop ]]; then
  if [[ -n "${WINDOW_FD_LOG:-}" ]] && { : >&9; } 2>/dev/null; then echo stop >>"$WINDOW_FD_LOG"; fi
  if [[ "${STOP_DISAPPEARS:-0}" == 1 ]]; then rm -f "$LAUNCHCTL_STATE.running"; exit 1; fi
  [[ "${STOP_FAIL:-0}" == 0 ]] || exit 1
  if [[ -f "$LAUNCHCTL_STATE.starting" ]]; then
    rm -f "$LAUNCHCTL_STATE.starting"
    echo "daemon is still starting; retry shutdown" >&2
    exit 1
  fi
  rm -f "$LAUNCHCTL_STATE.running"
fi
if [[ "${1:-}" == --socket && "${3:-}" == resource && "${4:-}" == validate ]]; then
  [[ "${FLEET_VALIDATE_FAIL_FOR:-}" != "$(basename "$(dirname "$(dirname "$0")")")" ]] || exit 1
  if [[ -n "${WINDOW_STATE_LOG:-}" ]]; then
    {
      [[ ! -e "$XDG_CONFIG_HOME/flotilla/run/install-in-progress" ]] || echo marker
      [[ -f "$LAUNCHCTL_STATE.loaded" ]] || echo unloaded
      cmp -s "$FLEET_INSTALL_ROOT/tcc/bin/flotilla" "$FLEET_INSTALL_ROOT/current/bin/flotilla" || echo stale-tcc
      echo validated
    } >>"$WINDOW_STATE_LOG"
  fi
fi
SH
    fi
    if [[ "$name" == flotilla ]]; then add_post_install_fixture "$bundle/bin/$name" "$generation"; fi
    printf 'printf "%s signed for %s\\n"\n' "$name" "$generation" >>"$bundle/bin/$name"
    chmod 0755 "$bundle/bin/$name"
  done
  printf 'ghostty signed\n' >"$bundle/lib/libghostty-vt.dylib"
  chmod 0755 "$bundle/lib/libghostty-vt.dylib"
  cp "$installer" "$bundle/install.sh"
  cp "$repo_root/ci/fleet-candidates/generation_validation.py" "$bundle/generation_validation.py"
  chmod 0755 "$bundle/install.sh" "$bundle/generation_validation.py"
  add_test_skills "$bundle"
  add_test_codex_home "$bundle"
  TEST_BUNDLE="$bundle" TEST_GENERATION="$generation" TEST_SOURCE_GENERATION="$source_generation" TEST_PROTOCOL="$protocol" python3 "$repo_root/scripts/fleet_install_test_support.py" darwin-release
  local artifact="fleet-signed-darwin-aarch64.tar.gz"
  COPYFILE_DISABLE=1 tar -C "$(dirname "$bundle")" -czf "$directory/$artifact" "$(basename "$bundle")"
  local digest size
  digest="$(file_sha256 "$directory/$artifact")"
  size="$(python3 "$repo_root/ci/fleet-candidates/generation_validation.py" size -- "$directory/$artifact")"
  TEST_GENERATION="$generation" TEST_SOURCE_GENERATION="$source_generation" TEST_ARTIFACT="$artifact" \
    TEST_DIGEST="$digest" TEST_SIZE="$size" TEST_DIRECTORY="$directory" python3 "$repo_root/scripts/fleet_install_test_support.py" darwin-generation
}

make_generation "$generation_one" 20
make_generation "$generation_two" 21
make_generation "$generation_handoff_failure" 21 linux-x86_64-gnu2.36 no \
  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb https://github.com/flotilla-org/mattpocock-skills.git fail
source_generation_two="20260815T215500Z-r2-f222222222222-cbbbbbbbbbbbb"
source_generation_one="20260815T205500Z-r1-f111111111111-caaaaaaaaaaaa"
add_darwin_derivative "$generation_one" "$source_generation_one" 20
add_darwin_derivative "$generation_two" "$source_generation_two" 21

cat >"$fixture_root/packages-page-1.json" <<JSON
[
  {"type":"generic","name":"flotilla-fleet","version":"$generation_one","created_at":"2026-08-15T21:00:00Z"},
  {"type":"generic","name":"another-package","version":"ignored","created_at":"2026-08-15T23:00:00Z"},
  {"type":"generic","name":"flotilla-fleet","version":"$generation_two","created_at":"2026-08-15T22:00:00Z"},
  {"type":"generic","name":"flotilla-fleet","version":"$generation_incomplete","created_at":"2026-08-15T22:50:00Z"}
]
JSON

cat >"$fake_bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
destination=""
url=""
write_out=""
while (($#)); do
  case "$1" in
    --output) destination="$2"; shift 2 ;;
    --header) shift 2 ;;
    --write-out) write_out="$2"; shift 2 ;;
    --fail|--silent|--show-error|--location) shift ;;
    *) url="$1"; shift ;;
  esac
done
[[ -n "$destination" && -n "$url" ]]
if [[ "$url" == *'/api/v1/packages/'* ]]; then
  page="${url##*page=}"
  source="$FIXTURE_ROOT/packages-page-$page.json"
  if [[ ! -f "$source" ]]; then printf '[]\n' >"$destination"; else cp "$source" "$destination"; fi
  exit 0
fi
generation="$(basename "$(dirname "$url")")"
file="$(basename "$url")"
source="$FIXTURE_ROOT/$generation/$file"
if [[ "${FAIL_MANIFEST_FOR:-}" == "$generation" && "$file" == generation.json ]]; then
  [[ -z "$write_out" ]] || printf '503'
  exit 0
fi
if [[ ! -f "$source" ]]; then
  [[ -z "$write_out" ]] || printf '404'
  exit 0
fi
if [[ "${FAIL_ARTIFACT_FOR:-}" == "$generation" && "$file" == *.tar.gz ]]; then
  printf 'interrupted' >"$destination"
  exit 56
fi
cp "$source" "$destination"
[[ -z "$write_out" ]] || printf '200'
SH
chmod 0755 "$fake_bin/curl"

cat >"$fake_bin/pgrep" <<'SH'
#!/usr/bin/env bash
if [[ -n "${LAUNCHCTL_STATE:-}" ]]; then
  [[ -f "$LAUNCHCTL_STATE.running" ]]
else
  [[ "${DAEMON_RUNNING:-0}" == 1 ]]
fi
SH
chmod 0755 "$fake_bin/pgrep"

cat >"$fake_bin/zsh" <<'SH'
#!/bin/sh
set -eu
[ "$#" -eq 3 ] || exit 98
[ "$1" = -l ] && [ "$2" = -c ] || exit 98
case "$3" in
  'command -v flotilla'|'command -v flotillad'|'command -v cleat') ;;
  *) exit 98 ;;
esac
case ":${PATH:-}:" in
  *":$HOME/.local/bin:"*) exit 97 ;;
esac
PATH="$LOGIN_SHELL_PATH"
export PATH
exec /bin/sh -c "$3"
SH
chmod 0755 "$fake_bin/zsh"

cat >"$fake_bin/systemctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$SYSTEMCTL_LOG"
if [[ "$*" == '--user is-enabled flotillad.service' ]]; then
  if [[ "${SYSTEMD_UNIT_DISABLED:-false}" == true ]]; then
    printf 'disabled\n'
    exit 1
  fi
  printf 'enabled\n'
fi
SH
chmod 0755 "$fake_bin/systemctl"

# Process boundary: Darwin memory pressure is supplied by the fixture host.
cat >"$fake_bin/sysctl" <<'SH'
#!/usr/bin/env bash
[[ "$*" == '-n kern.memorystatus_vm_pressure_level' ]] || exit 98
printf '%s\n' "${MEMORY_PRESSURE_LEVEL:-1}"
SH
chmod 0755 "$fake_bin/sysctl"

cat >"$fake_bin/loginctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$LOGINCTL_LOG"
state="$LOGINCTL_LOG.linger"
case "${1:-}" in
  # A redundant re-enable is denied, as polkit does outside a login session.
  enable-linger) if [[ -e "$state" ]]; then exit 1; else touch "$state"; fi ;;
  show-user) if [[ -e "$state" ]]; then printf 'yes\n'; else printf 'no\n'; fi ;;
esac
SH
chmod 0755 "$fake_bin/loginctl"

cat >"$fake_bin/launchctl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
current='<none>'
if [[ -L "$FLEET_INSTALL_ROOT/current" ]]; then
  current="$(readlink "$FLEET_INSTALL_ROOT/current")"
fi
printf '%s|%s\n' "$*" "$current" >>"$LAUNCHCTL_LOG"
# Process boundary double: model launchd registration separately from its process.
case "$1" in
  print) [[ -f "$LAUNCHCTL_STATE.loaded" ]] || exit 1 ;;
  bootout)
    [[ "${BOOTOUT_FAIL:-0}" == 0 ]] || exit 1
    rm -f "$LAUNCHCTL_STATE.loaded"
    [[ "${STOP_FAIL:-0}" == 1 ]] || rm -f "$LAUNCHCTL_STATE.running"
    if [[ "${CLIENT_AFTER_BOOTOUT:-0}" == 1 ]]; then
      # Process boundary double: a client respects the install marker.
      if [[ ! -f "$HOME/.config/flotilla/run/install-in-progress" ]]; then
        printf '%s\n' "$current" >"$LAUNCHCTL_STATE.running"
        touch "$LAUNCHCTL_STATE.starting"
        echo spawned >>"$LAUNCHCTL_STATE.client"
      else
        echo refused >>"$LAUNCHCTL_STATE.client"
      fi
    fi
    # A spawn admitted before the marker may finish after bootout.
    if [[ "${LATE_SPAWN_AFTER_BOOTOUT:-0}" == 1 ]]; then
      printf '%s\n' "$current" >"$LAUNCHCTL_STATE.running"
      touch "$LAUNCHCTL_STATE.starting"
    fi
    if [[ "${ABORT_AFTER_BOOTOUT:-0}" == 1 ]]; then kill -"${ABORT_SIGNAL:-TERM}" "$PPID"; fi
    ;;
  bootstrap)
    [[ "${BOOTSTRAP_FAIL:-0}" == 0 ]] || exit 1
    [[ ! -f "$LAUNCHCTL_STATE.loaded" ]] || exit 1
    touch "$LAUNCHCTL_STATE.loaded"
    printf '%s\n' "$current" >"$LAUNCHCTL_STATE.running"
    ;;
  kickstart)
    [[ -f "$LAUNCHCTL_STATE.loaded" ]] || exit 1
    printf '%s\n' "$current" >"$LAUNCHCTL_STATE.running"
    if [[ "${ABORT_AT_KICKSTART:-0}" == 1 ]]; then kill -KILL "$PPID"; fi
    ;;
esac
if [[ "$1" == print-disabled ]]; then
  printf 'disabled services = {\n    "work.flotilla.flotillad" => %s\n}\n' "${LAUNCHD_AGENT_DISABLED:-false}"
fi
SH
chmod 0755 "$fake_bin/launchctl"

cat >"$fake_bin/codesign" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$CODESIGN_LOG"
path="${!#}"
if [[ " $* " == *' --verify '* ]]; then
  [[ -z "${FAIL_CODESIGN_FOR:-}" || "$path" != *"$FAIL_CODESIGN_FOR"* ]] || exit 1
elif [[ " $* " == *' -d '* ]]; then
  printf 'Authority=Apple Development: Robert Wittams (DYYMCPD885)\n' >&2
  printf 'TeamIdentifier=973L4GV58R\n' >&2
  printf 'CDHash=0123456789abcdef\n' >&2
elif [[ " $* " == *' --entitlements '* ]]; then
  printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict/></plist>'
else
  exit 2
fi
SH
chmod 0755 "$fake_bin/codesign"

# Process boundary double for the flotilla history the installer fetches: with
# no FAKE_GIT_ORDER every commit descends from every other; otherwise the file
# lists full commits oldest first.
cat >"$fake_bin/git" <<'SH'
#!/usr/bin/env bash
position() { grep -nFx -- "$1" "$FAKE_GIT_ORDER" | cut -d: -f1; }
case "$1" in
  init|remote) exit 0 ;;
  fetch)
    [[ -z "${FAKE_GIT_ORDER:-}" ]] || position "${*: -1}" >/dev/null || { echo "unknown commit" >&2; exit 128; }
    exit 0 ;;
  merge-base)
    [[ -n "${FAKE_GIT_ORDER:-}" ]] || exit 0
    (( $(position "$3") <= $(position "$4") )) ;;
  rev-list)
    [[ -n "${FAKE_GIT_ORDER:-}" ]] || { echo 1; exit 0; }
    range="${*: -1}"
    echo $(( $(position "${range#*..}") - $(position "${range%%..*}") )) ;;
  *) exit 2 ;;
esac
SH
chmod 0755 "$fake_bin/git"

run_installer() {
  : "${SYSTEMCTL_LOG:=$test_root/systemctl.log}"
  : "${LOGINCTL_LOG:=$test_root/loginctl.log}"
  HOME="$test_root/home" \
    XDG_CONFIG_HOME="$test_root/home/.config" \
    PATH="$test_root/home/.local/bin:$fake_bin:$PATH" \
    SHELL="$fake_bin/zsh" \
    LOGIN_SHELL_PATH="$test_root/home/.local/bin:$fake_bin:/usr/bin:/bin" \
    FIXTURE_ROOT="$fixture_root" \
    SYSTEMCTL_LOG="$SYSTEMCTL_LOG" \
    LOGINCTL_LOG="$LOGINCTL_LOG" \
    FLEET_INSTALL_UNAME_S=Linux \
    FLEET_INSTALL_UNAME_M=x86_64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS="${FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS:-30}" \
    "$installer" "$@"
}

run_darwin_installer() {
  local home="$1"
  shift
  : "${LAUNCHCTL_LOG:=$test_root/launchctl.log}"
  HOME="$home" \
    XDG_CONFIG_HOME="$home/.config" \
    XDG_STATE_HOME="$home/.local/state" \
    PATH="$home/.local/bin:$fake_bin:$PATH" \
    SHELL="$fake_bin/zsh" \
    LOGIN_SHELL_PATH="$home/.local/bin:$fake_bin:/usr/bin:/bin" \
    FIXTURE_ROOT="$fixture_root" \
    CODESIGN_LOG="$test_root/codesign.log" \
    LAUNCHCTL_LOG="$LAUNCHCTL_LOG" \
    LAUNCHCTL_STATE="$home/launchctl-state" \
    LAUNCHD_AGENT_DISABLED="${LAUNCHD_AGENT_DISABLED:-false}" \
    FAIL_CODESIGN_FOR="${FAIL_CODESIGN_FOR:-}" \
    FLEET_INSTALL_ROOT="$home/.local/opt/flotilla-fleet" \
    FLEET_INSTALL_TESTING=1 \
    FLEET_INSTALL_TEST_CODESIGN="$fake_bin/codesign" \
    FLEET_INSTALL_UNAME_S=Darwin \
    FLEET_INSTALL_UNAME_M=arm64 \
    FLEET_INSTALL_API_URL="https://test.invalid/api/v1" \
    FLEET_INSTALL_PACKAGE_URL="https://test.invalid/api/packages" \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS="${FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS:-30}" \
    "$installer" "$@"
}

status_home="$test_root/status-home"
mkdir -p "$status_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$status_home/.config/flotilla/fleet-reader-token"
HOME="$status_home" PATH="$status_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" status >"$test_root/fresh-status.out"
test ! -e "$status_home/.local/opt/flotilla-fleet" || fail 'status mutated the install root'
grep -Fq 'fleet:   unavailable (daemon not running)' "$test_root/fresh-status.out" \
  || fail 'status did not degrade gracefully without a daemon'

login_path_home="$test_root/login-path-home"
mkdir -p "$login_path_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$login_path_home/.config/flotilla/fleet-reader-token"
if HOME="$login_path_home" PATH="$login_path_home/.local/bin:$fake_bin:$PATH" SHELL="$fake_bin/zsh" \
  LOGIN_SHELL_PATH="$fake_bin:/usr/bin:/bin" FIXTURE_ROOT="$fixture_root" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/login-path.out" 2>&1; then
  fail 'inline PATH masked a missing login-shell PATH entry'
fi
grep -Fq 'not reachable through the login shell' "$test_root/login-path.out" \
  || fail 'login-shell PATH failure was unclear'
grep -Fq '~/.zshenv' "$test_root/login-path.out" || fail 'zsh PATH failure omitted ~/.zshenv'

first_install_home="$test_root/first-install-home"
mkdir -p "$first_install_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$first_install_home/.config/flotilla/fleet-reader-token"
HOME="$first_install_home" PATH="$first_install_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  XDG_CONFIG_HOME="$first_install_home/.config" SHELL="$fake_bin/zsh" \
  LOGIN_SHELL_PATH="$first_install_home/.local/bin:$fake_bin:/usr/bin:/bin" \
  SYSTEMCTL_LOG="$test_root/first-install-systemctl.log" LOGINCTL_LOG="$test_root/first-install-loginctl.log" \
  DAEMON_RUNNING=1 STOP_FAIL=1 \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/first-install-failed.out" 2>&1 \
  && fail 'first install ignored a failed staged daemon stop'
first_install_cli="$first_install_home/.local/opt/flotilla-fleet/releases/$generation_one/bin/flotilla"
grep -Fq "run \`$first_install_cli daemon stop\`, then retry" "$test_root/first-install-failed.out" \
  || fail 'first install daemon refusal did not name its staged CLI'
test -x "$first_install_cli" || fail 'first install daemon refusal named a non-executable CLI'
HOME="$first_install_home" PATH="$first_install_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  XDG_CONFIG_HOME="$first_install_home/.config" SHELL="$fake_bin/zsh" \
  LOGIN_SHELL_PATH="$first_install_home/.local/bin:$fake_bin:/usr/bin:/bin" \
  SYSTEMCTL_LOG="$test_root/first-install-systemctl.log" LOGINCTL_LOG="$test_root/first-install-loginctl.log" \
  DAEMON_RUNNING=1 STOP_FAIL=0 \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/first-install-retry.out"
grep -Fq 'daemon stop requested' "$test_root/first-install-retry.out" || fail 'first-install retry did not stop the daemon with its staged CLI'
test "$(link_generation "$first_install_home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'first-install retry did not select the generation'

# Successful health confirmation delegates pruning with default K=3.
prune_install_log="$test_root/prune-install.log"
FLEET_CHECK_LOG="$test_root/check.log" PRUNE_LOG="$prune_install_log" run_installer "$generation_one" >"$test_root/install-one.out"
grep -Fq -- '--keep-others 3' "$prune_install_log" || fail 'healthy install did not delegate default retention'
if grep -Fq 'handing off activation' "$test_root/install-one.out"; then
  fail 'fresh install handed activation to the incoming installer'
fi
grep -Fq "generation $generation_one confirmed healthy" "$test_root/install-one.out" \
  || fail 'healthy Linux install was not confirmed'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'exact generation was not selected'
grep -Fq -- "--socket $test_root/home/.config/flotilla/run/flotilla.sock fleet check|1" "$test_root/check.log" || fail 'confirmation did not delegate exact local readiness'

# Failed health confirmation never prunes even when K=0.
if FLEET_INSTALL_KEEP_OTHERS=0 FLEET_HEALTH_FAIL_FOR="$generation_one" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=0 \
  PRUNE_LOG="$test_root/unhealthy-prune.log" run_installer "$generation_one" >"$test_root/no-previous.out" 2>&1; then
  fail 'unhealthy reinstall without a previous generation was accepted'
fi
grep -Fq "generation $generation_one failed health confirmation and no healthy previous generation could be restored" \
  "$test_root/no-previous.out" || fail 'unhealthy reinstall without a rollback target was not reported clearly'
test ! -L "$test_root/home/.local/opt/flotilla-fleet/previous" \
  || fail 'unhealthy reinstall created a self-referential previous generation'
test ! -e "$test_root/unhealthy-prune.log" || fail 'unhealthy install delegated pruning'
# A pruning failure is reported, but the healthy installation still succeeds.
PRUNE_EXIT=1 run_installer "$generation_one" >"$test_root/prune-failure-install.out" 2>&1
grep -Fq 'healthy installation remains active' "$test_root/prune-failure-install.out" || fail 'prune failure was not reported'
run_installer "$generation_one" >/dev/null
test -x "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/bin/flotilla" || fail 'candidate binaries were not staged'
test ! -w "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json" || fail 'selected generation is writable'
test "$(file_mode "$test_root/home/.local/bin")" = 755 || fail 'credential umask leaked into the launcher directory'
for name in flotilla flotillad cleat; do
  grep -Fq '# managed by fleet-install' "$test_root/home/.local/bin/$name" || fail "missing stable $name launcher"
done
unit="$test_root/home/.config/systemd/user/flotillad.service"
test -f "$unit" || fail 'systemd user unit was not installed'
grep -Fxq 'ExecStart="%h/.local/opt/flotilla-fleet/current/bin/flotillad" --timeout 0 --config-dir="%h/.config/flotilla" --state-dir="%h/.local/state/flotilla" --socket="%h/.config/flotilla/run/flotilla.sock"' "$unit" \
  || fail 'systemd user unit does not run the declared daemon identity without an idle timeout'
grep -Fxq 'Environment="PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin"' "$unit" \
  || fail 'systemd user unit does not expose the fleet binary PATH'
grep -Fxq 'Environment="FLOTILLA_SKILLS_DIR=%h/.local/opt/flotilla-fleet/current/share/flotilla/skills"' "$unit" \
  || fail 'systemd user unit does not select the generation-pinned skills'
grep -Fxq 'Environment="FLOTILLA_CODEX_HOME_TEMPLATE=%h/.local/opt/flotilla-fleet/current/share/flotilla/codex-home"' "$unit" \
  || fail 'systemd user unit does not select the generation-pinned Codex home template'
grep -Fxq 'Restart=always' "$unit" || fail 'systemd user unit does not always restart'
grep -Fxq 'RestartSec=5' "$unit" || fail 'systemd user unit does not delay restarts'
grep -Fxq -- '--user daemon-reload' "$test_root/systemctl.log" || fail 'systemd user manager was not reloaded'
grep -Fxq -- '--user enable flotillad.service' "$test_root/systemctl.log" || fail 'systemd user unit was not enabled'
grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log" || fail 'systemd user unit was not started'
grep -Eq '^enable-linger .+$' "$test_root/loginctl.log" || fail 'systemd lingering was not enabled'

: >"$test_root/systemctl.log"
SYSTEMD_UNIT_DISABLED=true run_installer "$generation_one" >"$test_root/linux-dev-mode-install.out"
grep -Fq 'preserving flotillad dev mode' "$test_root/linux-dev-mode-install.out" \
  || fail 'Linux install did not report preserved dev mode'
if grep -Eq -- '^--user (enable|restart) flotillad\.service$' "$test_root/systemctl.log"; then
  fail 'Linux install enabled or restarted the fleet unit while dev mode was active'
fi

status="$(run_installer status 2>"$test_root/status.err")"
grep -Fq "current: $generation_one (peer protocol 20)" <<<"$status" || fail 'status omitted current manifest protocol'
grep -Fq "latest:  $generation_two (peer protocol 21)" <<<"$status" || fail 'status selected the wrong promoted generation'
grep -Fq 'fleet:   unavailable (daemon not running)' <<<"$status" || fail 'status did not report unavailable fleet spread'
grep -Fq 'warning: peer protocol changes from 20 to 21' <<<"$status" || fail 'status omitted wire-bump warning'
grep -Fq "skipping incomplete linux-x86_64-gnu2.36 generation $generation_incomplete" "$test_root/status.err" \
  || fail 'latest did not explain why it skipped an incomplete generation'

# The binary owns grouping and response validation; the shell only delegates.
fleet_spread=$'fleet wire generations:\n  generation-fixture: host-fixture (local)'
fleet_status="$(DAEMON_RUNNING=1 FLEET_SPREAD_TEXT="$fleet_spread" FLEET_SPREAD_LOG="$test_root/spread.log" run_installer status 2>"$test_root/fleet-status.err")"
grep -Fq "$fleet_spread" <<<"$fleet_status" || fail 'status omitted the binary generation report'
grep -Fq -- "--socket $test_root/home/.config/flotilla/run/flotilla.sock fleet spread" "$test_root/spread.log" || fail 'status did not delegate spread to the local socket'
unreachable_status="$(DAEMON_RUNNING=1 run_installer status 2>"$test_root/unreachable-status.err")"
grep -Fq 'fleet:   unavailable (daemon query failed)' <<<"$unreachable_status" || fail 'status did not degrade gracefully when the binary query failed'

if run_installer "$generation_incomplete" >"$test_root/incomplete-exact.out" 2>&1; then
  fail 'an explicitly requested incomplete generation was accepted'
fi
grep -Fq "generation $generation_incomplete is not complete for linux-x86_64-gnu2.36" \
  "$test_root/incomplete-exact.out" || fail 'incomplete exact-generation error was unclear'

if FAIL_MANIFEST_FOR="$generation_two" run_installer status >"$test_root/latest-fetch.out" 2>&1; then
  fail 'latest silently downgraded after a promoted-generation fetch failure'
fi
grep -Fq "generation manifest request returned HTTP 503" "$test_root/latest-fetch.out" \
  || fail 'latest hid the promoted-generation fetch failure'
if grep -Fq "could not verify advertised generation $generation_two" "$test_root/latest-fetch.out"; then
  fail 'fatal generation fetch was incorrectly treated as a skippable generation'
fi

before_manifest="$(file_sha256 "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
printf '# managed by fleet-install\nstale unit\n' >"$unit"
: >"$test_root/systemctl.log"
run_installer "$generation_one" >/dev/null
after_manifest="$(file_sha256 "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/manifest.json")"
[[ "$before_manifest" == "$after_manifest" ]] || fail 'exact-generation reinstall mutated the release'
grep -Fxq 'ExecStart="%h/.local/opt/flotilla-fleet/current/bin/flotillad" --timeout 0 --config-dir="%h/.config/flotilla" --state-dir="%h/.local/state/flotilla" --socket="%h/.config/flotilla/run/flotilla.sock"' "$unit" \
  || fail 'exact-generation reinstall did not refresh the systemd user unit'
test "$(grep -Fxc -- '--user daemon-reload' "$test_root/systemctl.log")" = 1 \
  || fail 'unit refresh did not reload the systemd user manager exactly once'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 1 \
  || fail 'unit refresh did not restart flotillad exactly once'
test "$(grep -Ec '^enable-linger ' "$test_root/loginctl.log")" = 1 \
  || fail 'reinstall with linger already enabled must not re-attempt enable-linger'

ln -s "$$-active-test-owner" "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
if run_installer "$generation_one" >"$test_root/locked.out" 2>&1; then
  fail 'concurrent mutation lock was ignored'
fi
rm "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
grep -Fq 'another fleet-install mutation is already running' "$test_root/locked.out" || fail 'concurrent mutation error was unclear'

ln -s '999999999-stale-test-owner' "$test_root/home/.local/opt/flotilla-fleet/.install.lock"
run_installer "$generation_one" >"$test_root/stale-lock.out"
test ! -L "$test_root/home/.local/opt/flotilla-fleet/.install.lock" || fail 'stale install lock was not reclaimed'

if FAIL_ARTIFACT_FOR="$generation_two" run_installer "$generation_two" >"$test_root/interrupted.out" 2>&1; then
  fail 'interrupted download was accepted'
fi
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'interrupted download switched current'
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_two" || fail 'interrupted download published a release'

if run_installer "$generation_handoff_failure" >"$test_root/handoff-failure.out" 2>&1; then
  fail 'incoming installer failure was accepted'
fi
grep -Fq "handing off activation to generation $generation_handoff_failure installer" "$test_root/handoff-failure.out" \
  || fail 'upgrade did not execute the incoming generation installer'
grep -Fq 'incoming installer refused activation' "$test_root/handoff-failure.out" \
  || fail 'incoming installer failure was not reported'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'handoff failure switched current'
"$test_root/home/.local/opt/flotilla-fleet/current/bin/flotilla" --socket "$test_root/home/.config/flotilla/run/flotilla.sock" fleet check >/dev/null \
  || fail 'handoff failure did not leave the old generation restartable'

if FLEET_VALIDATION_LOG="$test_root/frozen-arguments.log" FLEET_INSTALL_SKILL_PROBE_TOKENS="$test_root/probe-tokens.json" FLEET_VALIDATE_FAIL_FOR="$generation_two" run_installer "$generation_two" >"$test_root/validation.out" 2>&1; then
  fail 'candidate with incompatible stored records was installed'
fi
# The activation gate receives the candidate supply, never current-generation
# skills, and forwards the operator's explicit credential probe map.
for expected in --skill-sources --skill-catalog --skill-probe-tokens "$test_root/probe-tokens.json"; do
  grep -Fxq -- "$expected" "$test_root/frozen-arguments.log" || fail "missing frozen-reference argument: $expected"
done
grep -Fxq "$test_root/home/.local/opt/flotilla-fleet/releases/$generation_two/share/flotilla/skills" "$test_root/frozen-arguments.log" \
  || fail 'frozen-reference gate did not use candidate skill supply'
grep -Fq 'failed live-store or frozen-reference validation' "$test_root/validation.out" \
  || fail 'candidate validation failure was not reported'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'candidate validation failure switched current'

if DAEMON_RUNNING=1 STOP_FAIL=1 run_installer "$generation_two" >"$test_root/daemon.out" 2>&1; then
  fail 'daemon stop refusal was ignored'
fi
grep -Fq 'flotilla daemon stop' "$test_root/daemon.out" || fail 'daemon refusal omitted recovery instruction'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'failed daemon preflight switched current'

: >"$test_root/systemctl.log"
if POST_INSTALL_LOG="$test_root/unhealthy-post-install.log" FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=0 \
  run_installer "$generation_two" >"$test_root/health-rollback.out" 2>&1; then
  fail 'unhealthy Linux generation was accepted'
fi
grep -Fq 'fixture readiness failure' "$test_root/health-rollback.out" || fail 'health refusal diagnostic was not retained'
test ! -e "$test_root/unhealthy-post-install.log" || fail 'post-install ran before health confirmation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'unhealthy Linux generation did not roll current back'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_two" \
  || fail 'unhealthy Linux generation was not retained as previous'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 2 \
  || fail 'Linux health failure did not restart the candidate and restored daemon'
grep -Fq "generation $generation_two failed health confirmation; rolled back to $generation_one" \
  "$test_root/health-rollback.out" || fail 'Linux automatic rollback was not reported loudly'

: >"$test_root/systemctl.log"
FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=2 \
  run_installer "$generation_two" >"$test_root/interrupted-confirmation.out" 2>&1 &
interrupted_installer_pid=$!
for _ in {1..50}; do
  if [[ "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" == "$generation_two" ]] \
    && grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log"; then
    break
  fi
  sleep 0.1
done
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" \
  || fail 'interrupted-confirmation test never selected the candidate'
grep -Fxq -- '--user restart flotillad.service' "$test_root/systemctl.log" \
  || fail 'interrupted-confirmation test did not start the candidate daemon'
confirmation_token="$(awk -F= '$1 == "token" { print $2 }' "$test_root/home/.local/opt/flotilla-fleet/.pending-confirmation")"
confirmation_installer_pid="${confirmation_token%%-*}"
[[ "$confirmation_installer_pid" =~ ^[0-9]+$ ]] || fail 'interrupted-confirmation test could not identify the installer process'
kill "$confirmation_installer_pid"
wait "$interrupted_installer_pid" 2>/dev/null || true
for _ in {1..50}; do
  if [[ "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" == "$generation_one" ]] \
    && [[ "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log" || true)" == 2 ]]; then
    break
  fi
  sleep 0.1
done
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'detached health watchdog did not roll back after the installer exited'
test "$(grep -Fxc -- '--user restart flotillad.service' "$test_root/systemctl.log")" = 2 \
  || fail 'detached Linux watchdog did not restart the candidate and restored daemon'
orphan_confirmation_log=""
for _ in {1..50}; do
  for candidate_log in "$test_root/home/.local/opt/flotilla-fleet"/.confirmation.*.log; do
    [[ -f "$candidate_log.done" ]] || continue
    if grep -Fq "generation $generation_two failed health confirmation; rolled back to $generation_one" "$candidate_log"; then
      orphan_confirmation_log="$candidate_log"
      break 2
    fi
  done
  sleep 0.1
done
[[ -n "$orphan_confirmation_log" ]] || fail 'detached watchdog did not retain a completed rollback audit log'

POST_INSTALL_LOG="$test_root/post-install.log" DAEMON_RUNNING=1 STOP_FAIL=0 run_installer latest >"$test_root/latest.out"
grep -Fq "handing off activation to generation $generation_two installer" "$test_root/latest.out" \
  || fail 'upgrade did not adopt the incoming generation installer'
grep -Fq "generation $generation_two confirmed healthy" "$test_root/latest.out" \
  || fail 'healthy Linux upgrade was not confirmed'
compgen -G "$test_root/home/.local/opt/flotilla-fleet/.confirmation.*.log" >/dev/null \
  && fail 'completed health watchdog left a confirmation log behind'
grep -Fq "$generation_one -> $generation_two" "$test_root/latest.out" || fail 'latest did not print the exact transition'
grep -Fq 'daemon stop requested' "$test_root/latest.out" || fail 'running daemon was not stopped before switching'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" || fail 'latest did not select newest promoted generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_one" || fail 'switch did not record previous generation'
if leftover_work_dirs="$(compgen -G "$test_root/home/.local/opt/flotilla-fleet/.fleet-install.*")"; then
  fail "successful upgrade left fleet-install work directories behind: $leftover_work_dirs"
fi

# The roll calls the incoming CLI after confirmation and surfaces its report.
expected_post_install="$test_root/home/.local/opt/flotilla-fleet/releases/$generation_two/bin/flotilla"
grep -Fq "$expected_post_install|--socket $test_root/home/.config/flotilla/run/flotilla.sock --json fleet post-install --cleat-bin $test_root/home/.local/opt/flotilla-fleet/releases/$generation_two/bin/cleat --generation $generation_two --diagnostics-dir $test_root/home/.local/opt/flotilla-fleet/diagnostics" "$test_root/post-install.log" \
  || fail 'roll did not invoke the incoming post-install binary with its cleat and diagnostics paths'
grep -Fq 'post-install fixture report' "$test_root/latest.out" || fail 'post-install report was swallowed'
# Post-install failure cannot undo the confirmed incoming generation.
if POST_INSTALL_EXIT=7 run_installer "$generation_two" >"$test_root/post-install-failed.out" 2>&1; then
  fail 'post-install failure was swallowed'
fi
grep -Fq 'post-install fixture report' "$test_root/post-install-failed.out" || fail 'failed post-install report was swallowed'
grep -Fq 'remains active' "$test_root/post-install-failed.out" || fail 'post-install failure omitted recovery state'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_two" || fail 'post-install failure rolled back'

run_installer rollback >"$test_root/rollback.out"
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'rollback did not restore previous generation'
test "$(link_generation "$test_root/home/.local/opt/flotilla-fleet/previous")" = "$generation_two" || fail 'rollback did not retain displaced generation'

roll_log="$test_root/home/.config/flotilla/fleet-rolls.log"
current_link="$test_root/home/.local/opt/flotilla-fleet/current"
commit_one="$(printf '1%.0s' $(seq 40))"
commit_two="222222222222$(printf '1%.0s' $(seq 28))"
last_roll() { tail -n 1 "$roll_log"; }
roll_field() { last_roll | cut -f"$1"; }
test "$(roll_field 2)" = "$generation_one" || fail 'rollback command was not recorded'
test "$(roll_field 3)" = "$generation_two" || fail 'rollback record omitted the replaced generation'

printf '%s\n%s\n' "$commit_one" "$commit_two" >"$test_root/order-forward"
FAKE_GIT_ORDER="$test_root/order-forward" run_installer "$generation_two" >"$test_root/roll-forward.out" 2>&1 \
  || { cat "$test_root/roll-forward.out" >&2; fail 'a descendant install needed a flag'; }
head -n 1 "$test_root/roll-forward.out" | grep -Fxq 'flotilla 11111111 -> 22222222 (1 commit ahead)' \
  || fail 'install did not lead with the flotilla comparison'
test "$(link_generation "$current_link")" = "$generation_two" || fail 'descendant install did not activate'
test "$(roll_field 2)" = "$generation_two" && test "$(roll_field 3)" = "$generation_one" \
  || fail 'install did not record the generation it replaced'
test "$(roll_field 4)" = "$commit_two" || fail 'install did not record the flotilla commit'
test -n "$(roll_field 5)" || fail 'install did not record the cleat commit'
test "$(roll_field 6)" = "$(id -un)" || fail 'install did not record who installed it'
test -z "$(roll_field 7)" || fail 'a forward install recorded a rollback reason'

rolls_before="$(wc -l <"$roll_log")"
if FAKE_GIT_ORDER="$test_root/order-forward" run_installer "$generation_one" >"$test_root/roll-refused.out" 2>&1; then
  fail 'a non-descendant install was accepted'
fi
head -n 1 "$test_root/roll-refused.out" | grep -Fxq 'flotilla 22222222 -> 11111111 (ROLLBACK: 1 commit behind live r2)' \
  || fail 'refusal did not lead with the ROLLBACK comparison'
grep -Fq 'refused: pass --rollback --reason "<why>" to install an older generation' "$test_root/roll-refused.out" \
  || fail 'refusal did not say how to roll back deliberately'
grep -Fq "$generation_one" "$test_root/roll-refused.out" && grep -Fq "$generation_two" "$test_root/roll-refused.out" \
  && grep -Fq "$commit_one" "$test_root/roll-refused.out" && grep -Fq "$commit_two" "$test_root/roll-refused.out" \
  || fail 'refusal did not name both generations and commits'
test "$(link_generation "$current_link")" = "$generation_two" || fail 'a refused install changed the live generation'
test "$(wc -l <"$roll_log")" = "$rolls_before" || fail 'a refused install was recorded'

if FAKE_GIT_ORDER="$test_root/order-forward" run_installer --rollback "$generation_one" >/dev/null 2>&1; then
  fail '--rollback without a reason was accepted'
fi
FAKE_GIT_ORDER="$test_root/order-forward" run_installer --rollback --reason $'tab\tbroke it' "$generation_one" >"$test_root/roll-accepted.out" 2>&1 \
  || { cat "$test_root/roll-accepted.out" >&2; fail '--rollback with a reason was refused'; }
test "$(link_generation "$current_link")" = "$generation_one" || fail 'accepted rollback did not activate'
test "$(roll_field 7)" = 'tab broke it' || fail 'rollback reason was not recorded'
test "$(roll_field 3)" = "$generation_two" || fail 'rollback record omitted the replaced generation'
test "$(last_roll | awk -F'\t' '{print NF}')" = 7 || fail 'roll record is not seven fields'

if FAKE_GIT_ORDER="$test_root/order-forward" run_installer --rollback --reason x rollback >/dev/null 2>&1; then
  fail '--rollback was accepted with the rollback subcommand'
fi

live_metadata="$test_root/home/.local/opt/flotilla-fleet/releases/$generation_one/.generation.json"
cp "$live_metadata" "$test_root/live-metadata.json"
chmod u+w "$live_metadata" "$(dirname "$live_metadata")"
python3 - "$live_metadata" <<'PY'
import json, sys
path = sys.argv[1]
document = json.load(open(path))
del document["sources"]["flotilla"]
json.dump(document, open(path, "w"))
PY
if FAKE_GIT_ORDER="$test_root/order-forward" run_installer "$generation_two" >"$test_root/roll-unreadable.out" 2>&1; then
  fail 'an install was accepted without the live flotilla commit'
fi
grep -Fq 'cannot compare with live r1' "$test_root/roll-unreadable.out" || fail 'unreadable live commit did not refuse as a rollback'
grep -Fq 'unbound variable' "$test_root/roll-unreadable.out" && fail 'unreadable live commit crashed the guard'
cp "$test_root/live-metadata.json" "$live_metadata"
chmod a-w "$live_metadata" "$(dirname "$live_metadata")"

custom_root="$test_root/custom-root"
custom_bin="$test_root/custom-bin"
HOME="$test_root/home" XDG_CONFIG_HOME="$test_root/home/.config" PATH="$custom_bin:$fake_bin:$PATH" \
  SHELL="$fake_bin/zsh" LOGIN_SHELL_PATH="$custom_bin:$fake_bin:/usr/bin:/bin" \
  FIXTURE_ROOT="$fixture_root" SYSTEMCTL_LOG="$test_root/systemctl.log" LOGINCTL_LOG="$test_root/loginctl.log" \
  FLEET_INSTALL_ROOT="$custom_root" FLEET_INSTALL_BIN_DIR="$custom_bin" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/custom-paths.out"
grep -Fxq "ExecStart=\"$custom_root/current/bin/flotillad\" --timeout 0 --config-dir=\"%h/.config/flotilla\" --state-dir=\"%h/.local/state/flotilla\" --socket=\"%h/.config/flotilla/run/flotilla.sock\"" "$unit" \
  || fail 'systemd user unit ignored the configured fleet root'
grep -Fxq "Environment=\"PATH=$custom_bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin\"" "$unit" \
  || fail 'systemd user unit ignored the configured fleet binary directory'
grep -Fxq "Environment=\"FLOTILLA_SKILLS_DIR=$custom_root/current/share/flotilla/skills\"" "$unit" \
  || fail 'systemd user unit ignored the configured generation skills'
grep -Fxq "Environment=\"FLOTILLA_CODEX_HOME_TEMPLATE=$custom_root/current/share/flotilla/codex-home\"" "$unit" \
  || fail 'systemd user unit ignored the configured generation Codex home template'

darwin_home="$test_root/darwin-home"
mkdir -p "$darwin_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_home/.config/flotilla/fleet-reader-token"
# A stable TCC client must be a regular copy at the same real path across
# installs, automatic rollback, reinstall and explicit rollback. Compare exact
# signed payload bytes, including the executable-relative dynamic library.
assert_darwin_tcc_payload() {
  local generation="$1"
  local stable="$darwin_home/.local/opt/flotilla-fleet/tcc"
  local release="$darwin_home/.local/opt/flotilla-fleet/releases/$generation"
  local relative
  for relative in bin/flotilla bin/flotillad bin/cleat lib/libghostty-vt.dylib; do
    test -f "$stable/$relative" && test ! -L "$stable/$relative" || fail "TCC payload is not a regular copy: $relative"
    cmp "$stable/$relative" "$release/$relative" || fail "TCC payload differs from selected generation: $relative"
  done
  for relative in flotilla flotillad cleat; do
    test -x "$stable/bin/$relative" || fail "TCC binary lost executable mode: $relative"
    grep -Fq "$stable/bin/$relative" "$darwin_home/.local/bin/$relative" || fail "launcher bypasses stable TCC path: $relative"
  done
}

run_darwin_installer "$darwin_home" "$generation_one" >"$test_root/darwin-install-one.out"
assert_darwin_tcc_payload "$generation_one"
# Critical memory pressure refuses before unloading the working daemon or
# changing either generation link. Native sysctl is the process boundary double.
: >"$test_root/launchctl.log"
if MEMORY_PRESSURE_LEVEL=4 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/pressure-refusal.out" 2>&1; then
  fail 'critical memory pressure allowed a roll'
fi
grep -Fq 'critical memory pressure' "$test_root/pressure-refusal.out" || fail 'pressure refusal was unclear'
test ! -s "$test_root/launchctl.log" || fail 'pressure refusal touched launchd'
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'pressure refusal changed current'
test ! -e "$darwin_home/.local/opt/flotilla-fleet/previous" || fail 'pressure refusal changed previous'
test -f "$darwin_home/launchctl-state.running" || fail 'pressure refusal stopped daemon'
# A client arriving between bootout and preflight must not resurrect the old
# daemon. A spawn already admitted must survive a transient startup stop error.
WINDOW_FD_LOG="$test_root/window-fd.log" CLIENT_AFTER_BOOTOUT=1 LATE_SPAWN_AFTER_BOOTOUT=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-install-window.out" 2>&1 \
  || fail 'daemon arriving after bootout required a second install attempt'
test ! -e "$test_root/window-fd.log" || fail "CLI children inherited the install-window lock: $(cat "$test_root/window-fd.log")"
grep -Fxq refused "$darwin_home/launchctl-state.client" || fail 'install window permitted a client spawn'
test ! -e "$darwin_home/.config/flotilla/run/install-in-progress" || fail 'successful install leaked marker'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_two" || fail 'install window ran old generation'
# Reinstalling the selected generation must also stop a late bare daemon.
CLIENT_AFTER_BOOTOUT=1 LATE_SPAWN_AFTER_BOOTOUT=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-reinstall-window.out" 2>&1 \
  || fail 'late daemon prevented exact-generation reinstall'
test ! -e "$darwin_home/launchctl-state.starting" || fail 'reinstall skipped daemon preflight'
run_darwin_installer "$darwin_home" rollback >/dev/null
# Refusing validation leaves the old service untouched and available.
if FLEET_VALIDATE_FAIL_FOR="$generation_two" run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-validation.out" 2>&1; then
  fail 'Darwin validation refusal was accepted'
fi
test -f "$darwin_home/launchctl-state.loaded" || fail 'validation refusal unloaded agent'
test -f "$darwin_home/launchctl-state.running" || fail 'validation refusal stopped daemon'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'recovery ran the wrong generation'

# A failed stop must preserve the selected generation and restore a loaded,
# running launch agent, even though bootout succeeded before the CLI failed.
if STOP_FAIL=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-stop-failed.out" 2>&1; then
  fail 'Darwin failed stop was accepted'
fi
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'failed stop switched generation'
test ! -e "$darwin_home/.config/flotilla/run/install-in-progress" || fail 'failed stop leaked marker'
test -f "$darwin_home/launchctl-state.loaded" || fail 'failed stop left launch agent unloaded'
test -f "$darwin_home/launchctl-state.running" || fail 'failed stop left daemon down'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'recovery ran the wrong generation'

grep -Fq 'then retry fleet-install' "$test_root/darwin-stop-failed.out" || fail 'failed stop omitted installer retry hint'
grep -Fq 'generation validation: daemon stop deadline expired' "$test_root/darwin-stop-failed.out" || fail 'stop deadline omitted clean diagnostic'
if grep -Fq Traceback "$test_root/darwin-stop-failed.out"; then fail 'stop deadline leaked Python traceback'; fi
# If launchd refuses bootout, recovery must kickstart the existing registration.
if BOOTOUT_FAIL=1 STOP_FAIL=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-bootout-failed.out" 2>&1; then
  fail 'failed bootout and stop were accepted'
fi
test -f "$darwin_home/launchctl-state.loaded" || fail 'failed bootout lost registration'
test -f "$darwin_home/launchctl-state.running" || fail 'failed bootout lost daemon'
if grep -Fq 'could not restore the previous launch agent' "$test_root/darwin-bootout-failed.out"; then
  fail 'recovery tried to bootstrap an already loaded agent'
fi

# Catchable interruptions in the incoming installer must restore the old daemon.
for abort_signal in TERM INT; do
  if ABORT_AFTER_BOOTOUT=1 ABORT_SIGNAL="$abort_signal" run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-abort-$abort_signal.out" 2>&1; then
    fail "Darwin handoff interrupted by $abort_signal succeeded"
  fi
  test ! -e "$darwin_home/.config/flotilla/run/install-in-progress" || fail 'interrupted install leaked marker'
  test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'aborted handoff switched generation'
  test -f "$darwin_home/launchctl-state.loaded" || fail 'aborted handoff left agent unloaded'
  test -f "$darwin_home/launchctl-state.running" || fail 'aborted handoff left daemon down'
  test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'recovery ran the wrong generation'
done

darwin_root="$darwin_home/.local/opt/flotilla-fleet"
darwin_marker="$darwin_home/.config/flotilla/run/install-in-progress"
darwin_window_lock="$darwin_home/.config/flotilla/run/install-window.lock"
if ABORT_AFTER_BOOTOUT=1 ABORT_SIGNAL=KILL run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-killed.out" 2>&1; then
  fail 'killed Darwin install succeeded'
fi
test -f "$darwin_marker" || fail 'killed installer left no marker to reclaim'
test ! -f "$darwin_home/launchctl-state.loaded" || fail 'killed installer fixture left the agent loaded'
chmod u+w "$darwin_root/tcc/bin/flotilla"
printf 'partial payload\n' >"$darwin_root/tcc/bin/flotilla"

printf 'fleet_root=%s\n' "$test_root/other-fleet-root" >"$darwin_marker"
: >"$test_root/launchctl.log"
if run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-foreign-window.out" 2>&1; then
  fail 'retry reclaimed a window abandoned by another fleet root'
fi
grep -Fq "rerunning fleet-install with FLEET_INSTALL_ROOT=$test_root/other-fleet-root" "$test_root/darwin-foreign-window.out" \
  || fail 'foreign abandoned window did not name its fleet root'
test -f "$darwin_marker" || fail 'foreign abandoned window lost its marker'
printf 'fleet_root=%s\n' "$darwin_root" >"$darwin_marker"

exec 7>>"$darwin_window_lock"
python3 "$repo_root/ci/fleet-candidates/generation_validation.py" lock-window -- 7
if run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-held-window.out" 2>&1 7>&-; then
  fail 'retry reopened an install window another installer holds'
fi
exec 7>&-
grep -Fq 'holds the daemon install window' "$test_root/darwin-held-window.out" || fail 'held window refusal was unclear'
test -f "$darwin_marker" || fail 'held window lost its marker'
if grep -Ev '^print-disabled ' "$test_root/launchctl.log" | grep -q .; then fail 'retry touched launchd while the window was held'; fi

: >"$test_root/window-state.log"
WINDOW_STATE_LOG="$test_root/window-state.log" run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-reclaimed.out" 2>&1 \
  || fail 'abandoned install window needed manual recovery'
grep -Fq 'recovering an interrupted daemon install window' "$test_root/darwin-reclaimed.out" || fail 'reclaim was not reported'
test "$(cat "$test_root/window-state.log")" = validated \
  || fail "live-store validation ran before the abandoned window recovered: $(cat "$test_root/window-state.log")"
test ! -e "$darwin_marker" || fail 'reclaimed install leaked marker'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_two" || fail 'reclaimed install ran the wrong generation'
assert_darwin_tcc_payload "$generation_two"

health_gate="$test_root/darwin-health-gate"
if FLEET_HEALTH_GATE="$health_gate" ABORT_AT_KICKSTART=1 run_darwin_installer "$darwin_home" "$generation_one" >"$test_root/darwin-orphaned-watchdog.out" 2>&1; then
  fail 'installer killed during restart succeeded'
fi
test -f "$darwin_marker" || fail 'installer killed during restart left no marker'
: >"$test_root/launchctl.log"
if run_darwin_installer "$darwin_home" "$generation_one" >"$test_root/darwin-watchdog-window.out" 2>&1; then
  fail 'retry reopened the window an orphaned health watchdog holds'
fi
grep -Fq 'holds the daemon install window' "$test_root/darwin-watchdog-window.out" || fail 'watchdog-held refusal was unclear'
test -f "$darwin_marker" || fail 'watchdog-held window lost its marker'
if grep -Ev '^print-disabled ' "$test_root/launchctl.log" | grep -q .; then fail 'retry touched launchd while the watchdog held the window'; fi
touch "$health_gate"
python3 - "$darwin_window_lock" <<'PY' || fail 'orphaned watchdog did not release the install window'
import fcntl, sys, time
deadline = time.monotonic() + 60
with open(sys.argv[1], "a") as lock:
    while True:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            if time.monotonic() >= deadline:
                sys.exit(1)
            time.sleep(0.1)
PY
grep -Fq "generation $generation_one confirmed healthy" "$darwin_root"/.confirmation.*.log || fail 'orphaned watchdog did not confirm'
run_darwin_installer "$darwin_home" "$generation_one" >"$test_root/darwin-after-watchdog.out" 2>&1 \
  || fail 'window released by the watchdog was not reclaimed'
grep -Fq 'recovering an interrupted daemon install window' "$test_root/darwin-after-watchdog.out" || fail 'post-watchdog reclaim was not reported'
test ! -e "$darwin_marker" || fail 'post-watchdog reclaim leaked marker'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'post-watchdog reclaim ran the wrong generation'
assert_darwin_tcc_payload "$generation_one"

# A launchd recovery error must be reported, preserve the selection, and fail
# installation. Retrying the current generation after launchd recovers is safe.
if STOP_FAIL=1 BOOTSTRAP_FAIL=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-recovery-failed.out" 2>&1; then
  fail 'failed launch agent restoration returned success'
fi
grep -Fq 'could not restore the previous launch agent' "$test_root/darwin-recovery-failed.out" || fail 'recovery failure omitted manual recovery hint'
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" || fail 'failed recovery switched generation'
run_darwin_installer "$darwin_home" "$generation_one" >/dev/null
test -f "$darwin_home/launchctl-state.loaded" || fail 'retry after recovery failure left agent unloaded'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'retry after recovery failure ran wrong generation'

# An error from the stop CLI with no remaining process is safe to continue.
if ! STOP_FAIL=1 STOP_DISAPPEARS=1 run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-disappeared.out" 2>&1; then
  fail 'already exited daemon was treated as a failed stop'
fi
grep -Fq 'daemon is no longer running' "$test_root/darwin-disappeared.out" || fail 'missing already stopped diagnosis'
run_darwin_installer "$darwin_home" rollback >/dev/null

: >"$test_root/launchctl.log"
if FLEET_HEALTH_FAIL_FOR="$generation_two" FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=0 \
  run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-health-rollback.out" 2>&1; then
  fail 'unhealthy Darwin generation was accepted'
fi
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'unhealthy Darwin generation did not roll current back'
test -f "$darwin_home/launchctl-state.loaded" || fail 'health rollback left agent unloaded'
test -f "$darwin_home/launchctl-state.running" || fail 'health rollback left daemon down'
test "$(cat "$darwin_home/launchctl-state.running")" = "releases/$generation_one" || fail 'recovery ran the wrong generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin health check did not start the candidate generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_one$" "$test_root/launchctl.log" \
  || fail 'Darwin health failure did not restart the restored generation'
grep -Fq "generation $generation_two failed health confirmation; rolled back to $generation_one" \
  "$test_root/darwin-health-rollback.out" || fail 'Darwin automatic rollback was not reported loudly'
assert_darwin_tcc_payload "$generation_one"
: >"$test_root/codesign.log"
: >"$test_root/launchctl.log"
# Refresh must prune obsolete libraries and recursively copy nested payloads.
mkdir -p "$darwin_home/.local/opt/flotilla-fleet/tcc/lib/nested"
printf stale >"$darwin_home/.local/opt/flotilla-fleet/tcc/lib/nested/obsolete.dylib"
run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-install.out"
test ! -e "$darwin_home/.local/opt/flotilla-fleet/tcc/lib/nested/obsolete.dylib" || fail 'obsolete TCC library survived refresh'
assert_darwin_tcc_payload "$generation_two"
test "$(link_generation "$darwin_home/.local/opt/flotilla-fleet/current")" = "$generation_two" \
  || fail 'signed Darwin generation was not selected'
test -f "$darwin_home/.local/opt/flotilla-fleet/releases/$generation_two/lib/libghostty-vt.dylib" \
  || fail 'signed Darwin dynamic library was not installed'
test "$(grep -c -- '--verify' "$test_root/codesign.log")" = 8 \
  || fail 'Darwin upgrade did not verify every Mach-O payload before and after handoff'
test "$(grep -c -- '--entitlements' "$test_root/codesign.log")" = 8 \
  || fail 'Darwin upgrade did not verify every Mach-O entitlement set before and after handoff'
launch_agent="$darwin_home/Library/LaunchAgents/work.flotilla.flotillad.plist"
python3 "$repo_root/scripts/fleet_install_test_support.py" assert-launchd "$launch_agent" "$darwin_home" || fail 'Darwin launchd agent content is incorrect'
test -d "$darwin_home/Library/Logs/flotilla" \
  || fail 'Darwin install did not create the launchd log directory'
grep -Eq "^bootout gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_one$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not unload the old agent before the generation flip'
grep -Eq "^enable gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not enable the agent after selecting the generation'
grep -Eq "^bootstrap gui/[0-9]+ $launch_agent\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not bootstrap the selected generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|releases/$generation_two$" "$test_root/launchctl.log" \
  || fail 'Darwin install did not restart through launchd after the generation flip'

: >"$test_root/launchctl.log"
run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-reinstall.out"
test "$(grep -Ec '^bootout gui/[0-9]+/work\.flotilla\.flotillad\|' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not unload the agent exactly once'
test "$(grep -Ec '^bootstrap gui/[0-9]+ ' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not bootstrap the refreshed agent exactly once'
test "$(grep -Ec '^kickstart -k gui/[0-9]+/work\.flotilla\.flotillad\|' "$test_root/launchctl.log")" = 1 \
  || fail 'Darwin exact-generation reinstall did not restart the agent exactly once'

: >"$test_root/launchctl.log"
LAUNCHD_AGENT_DISABLED=true run_darwin_installer "$darwin_home" "$generation_two" >"$test_root/darwin-dev-mode-install.out"
assert_darwin_tcc_payload "$generation_two"
grep -Fq 'preserving flotillad dev mode' "$test_root/darwin-dev-mode-install.out" \
  || fail 'Darwin install did not report preserved dev mode'
if grep -Eq '^(enable|bootstrap|kickstart) ' "$test_root/launchctl.log"; then
  fail 'Darwin install restarted the fleet agent while dev mode was active'
fi

darwin_previous_target="releases/$generation_one"
: >"$test_root/launchctl.log"
run_darwin_installer "$darwin_home" rollback >"$test_root/darwin-rollback.out"
assert_darwin_tcc_payload "$generation_one"
grep -Eq "^bootstrap gui/[0-9]+ $launch_agent\\|$darwin_previous_target$" "$test_root/launchctl.log" \
  || fail 'Darwin rollback did not bootstrap after selecting the previous generation'
grep -Eq "^kickstart -k gui/[0-9]+/work\\.flotilla\\.flotillad\\|$darwin_previous_target$" "$test_root/launchctl.log" \
  || fail 'Darwin rollback did not restart the selected generation through launchd'

darwin_fail_home="$test_root/darwin-fail-home"
mkdir -p "$darwin_fail_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_fail_home/.config/flotilla/fleet-reader-token"
if FAIL_CODESIGN_FOR=bin/cleat run_darwin_installer "$darwin_fail_home" "$generation_two" \
  >"$test_root/darwin-signature-failure.out" 2>&1; then
  fail 'Darwin signature failure was accepted'
fi
grep -Fq 'strict signature verification failed: bin/cleat' "$test_root/darwin-signature-failure.out" \
  || fail 'Darwin signature failure was unclear'
test ! -L "$darwin_fail_home/.local/opt/flotilla-fleet/current" \
  || fail 'Darwin signature failure selected a generation'
test ! -e "$darwin_fail_home/.local/opt/flotilla-fleet/releases/$generation_two" \
  || fail 'Darwin signature failure published a release'

cp "$fixture_root/$generation_two/generation.json" "$test_root/generation-two-good.json"
python3 "$repo_root/scripts/fleet_install_test_support.py" corrupt-outer-source "$fixture_root/$generation_two/generation.json"
darwin_team_home="$test_root/darwin-team-home"
mkdir -p "$darwin_team_home/.config/flotilla"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$darwin_team_home/.config/flotilla/fleet-reader-token"
if run_darwin_installer "$darwin_team_home" "$generation_two" >"$test_root/darwin-team.out" 2>&1; then
  fail 'Darwin artifact from an untrusted Apple team was accepted'
fi
grep -Fq 'trusted Apple team 973L4GV58R' "$test_root/darwin-team.out" \
  || fail 'Darwin team rejection was unclear'
mv "$test_root/generation-two-good.json" "$fixture_root/$generation_two/generation.json"

bad_digest="20260815T230000Z-r3-f333333333333-ccccccccccccc"
make_generation "$bad_digest" 21
python3 "$repo_root/scripts/fleet_install_test_support.py" corrupt-outer-digest "$fixture_root/$bad_digest/generation.json"
if run_installer "$bad_digest" >"$test_root/digest.out" 2>&1; then
  fail 'outer digest mismatch was accepted'
fi
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_digest" || fail 'digest rejection published a release'

bad_manifest="20260815T231000Z-r4-f444444444444-cdddddddddddd"
make_generation "$bad_manifest" 21 linux-x86_64-gnu2.36 yes \
  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb https://github.com/flotilla-org/mattpocock-skills.git fail
if run_installer "$bad_manifest" >"$test_root/manifest.out" 2>&1; then
  fail 'inner manifest mismatch was accepted'
fi
if grep -Fq 'incoming installer refused activation' "$test_root/manifest.out"; then
  fail 'generation installer ran before its payload passed verification'
fi
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_manifest" || fail 'manifest rejection published a release'

# The staged skill manifest is data, so pin equality against the generation's
# sources map is the only thing tying a bundle's skills to what was promoted.
bad_skill_pin="20260815T233000Z-r6-f666666666666-cfffffffffff0"
make_generation "$bad_skill_pin" 21 linux-x86_64-gnu2.36 no "cccccccccccccccccccccccccccccccccccccccc"
if run_installer "$bad_skill_pin" >"$test_root/skill-pin.out" 2>&1; then
  fail 'skill source pin that disagrees with the generation was accepted'
fi
grep -Fq 'skill bundle source pins do not match the fleet generation' "$test_root/skill-pin.out" \
  || fail 'skill pin mismatch was refused for the wrong reason'
test ! -e "$test_root/home/.local/opt/flotilla-fleet/releases/$bad_skill_pin" || fail 'skill pin rejection published a release'

bad_platform="20260815T232000Z-r5-f555555555555-ceeeeeeeeeeee"
make_generation "$bad_platform" 21 darwin-aarch64
if run_installer "$bad_platform" >"$test_root/platform.out" 2>&1; then
  fail 'wrong-platform generation was accepted'
fi
grep -Fq 'no linux-x86_64-gnu2.36 artifact' "$test_root/platform.out" || fail 'wrong-platform error was unclear'

shadow_home="$test_root/shadow-home"
shadow_bin="$test_root/shadow-bin"
mkdir -p "$shadow_home/.config/flotilla" "$shadow_bin"
cp "$test_root/home/.config/flotilla/fleet-reader-token" "$shadow_home/.config/flotilla/fleet-reader-token"
printf '#!/bin/sh\nexit 0\n' >"$shadow_bin/cleat"
chmod 0755 "$shadow_bin/cleat"
if HOME="$shadow_home" PATH="$shadow_bin:$shadow_home/.local/bin:$fake_bin:$PATH" FIXTURE_ROOT="$fixture_root" \
  SHELL="$fake_bin/zsh" LOGIN_SHELL_PATH="$shadow_bin:$shadow_home/.local/bin:$fake_bin:/usr/bin:/bin" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  "$installer" "$generation_one" >"$test_root/shadow.out" 2>&1; then
  fail 'PATH shadow was accepted'
fi
grep -Fq 'PATH shadows the fleet launcher for cleat' "$test_root/shadow.out" || fail 'cleat PATH shadow error was unclear'
test ! -L "$shadow_home/.local/opt/flotilla-fleet/current" || fail 'PATH shadow switched current'

# Glue: both explicit modes delegate to the active generation with retention
# and dry-run arguments. Rust tests cover process inspection and deletion.
prune_log="$test_root/prune-delegation.log"
PRUNE_LOG="$prune_log" FLEET_INSTALL_KEEP_OTHERS=0 run_installer --prune >/dev/null
PRUNE_LOG="$prune_log" FLEET_INSTALL_KEEP_OTHERS=9 run_installer --prune-dry-run >/dev/null
grep -Fq 'fleet prune --fleet-root ' "$prune_log" || fail 'prune did not delegate'
grep -Fq -- '--keep-others 0' "$prune_log" || fail 'prune lost retention argument'
grep -Fq -- '--keep-others 9 --dry-run' "$prune_log" || fail 'dry-run lost arguments'
grep -Fq '/current/bin/flotilla|' "$prune_log" || fail 'prune did not use active binary'

# --from installs a named generation from a pushed package directory laid out
# like the store. It reads no package token and never calls curl.
pushed_home="$test_root/pushed-home"
pushed_bin="$test_root/pushed-bin"
mkdir -p "$pushed_home/.config" "$pushed_bin"
cat >"$pushed_bin/curl" <<SH
#!/usr/bin/env bash
printf 'curl %s\n' "\$*" >>"$test_root/pushed-curl.log"
exit 7
SH
chmod 0755 "$pushed_bin/curl"
run_pushed_installer() {
  HOME="$pushed_home" XDG_CONFIG_HOME="$pushed_home/.config" \
    PATH="$pushed_home/.local/bin:$pushed_bin:$fake_bin:$PATH" SHELL="$fake_bin/zsh" \
    LOGIN_SHELL_PATH="$pushed_home/.local/bin:$fake_bin:/usr/bin:/bin" \
    SYSTEMCTL_LOG="$test_root/pushed-systemctl.log" LOGINCTL_LOG="$test_root/pushed-loginctl.log" \
    FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
    FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
    FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=30 \
    "$installer" "$@"
}
for operand in latest status rollback --prune; do
  if run_pushed_installer --from "$fixture_root" "$operand" >"$test_root/pushed-refused.out" 2>&1; then
    fail "--from accepted $operand"
  fi
  grep -Fq -- '--from installs a named generation only' "$test_root/pushed-refused.out" \
    || fail "--from refusal of $operand was unclear"
done
if run_pushed_installer --from "$test_root/no-such-dir" "$generation_one" >"$test_root/pushed-missing-dir.out" 2>&1; then
  fail '--from accepted a missing directory'
fi
grep -Fq 'pushed package directory is missing' "$test_root/pushed-missing-dir.out" || fail 'missing --from directory was unclear'
if run_pushed_installer --from "$fixture_root" "$generation_incomplete" >"$test_root/pushed-incomplete.out" 2>&1; then
  fail '--from installed a generation without a pushed manifest'
fi
grep -Fq "generation $generation_incomplete is not complete" "$test_root/pushed-incomplete.out" \
  || fail '--from did not treat a missing manifest as incomplete'
run_pushed_installer --from "$fixture_root" "$generation_one" >"$test_root/pushed-install.out"
test "$(link_generation "$pushed_home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail '--from did not select the pushed generation'
test ! -e "$pushed_home/.config/flotilla/fleet-reader-token" || fail '--from test home unexpectedly had a token'
test ! -e "$test_root/pushed-curl.log" || fail '--from called curl'
# A pushed archive must match the manifest, exactly as a downloaded one.
pushed_corrupt="$test_root/pushed-corrupt"
mkdir -p "$pushed_corrupt"
cp -R "$fixture_root/$generation_two" "$pushed_corrupt/"
for archive in "$pushed_corrupt/$generation_two"/*.tar.gz; do printf 'tampered' >>"$archive"; done
if run_pushed_installer --from "$pushed_corrupt" "$generation_two" >"$test_root/pushed-corrupt.out" 2>&1; then
  fail '--from installed a tampered archive'
fi
grep -Eq "archive (size|digest) mismatch for $generation_two" "$test_root/pushed-corrupt.out" \
  || fail '--from did not verify the pushed archive'
test "$(link_generation "$pushed_home/.local/opt/flotilla-fleet/current")" = "$generation_one" \
  || fail 'a tampered pushed archive switched current'

# fleet-push downloads once with the lab token, then installs each host over
# ssh with --from. The ssh double runs the remote command in a per-host home.
push_bin="$test_root/push-bin"
push_hosts="$test_root/push-hosts"
mkdir -p "$push_bin" "$push_hosts/box-a/.config" "$push_hosts/box-b/.config"
cat >"$push_bin/curl" <<SH
#!/usr/bin/env bash
output='' url='' header=''
while [[ \$# -gt 0 ]]; do
  case "\$1" in
    --output) output="\$2"; shift 2 ;;
    --header) header="\$2"; shift 2 ;;
    -*) shift ;;
    *) url="\$1"; shift ;;
  esac
done
grep -Fqx 'Authorization: token test-token' "\${header#@}" || exit 22
printf '%s\n' "\$url" >>"$test_root/push-curl.log"
source="$fixture_root/\${url#https://test.invalid/api/packages/lab/generic/flotilla-fleet/}"
[[ -f "\$source" ]] || exit 22
cp "\$source" "\$output"
SH
cat >"$push_bin/ssh" <<SH
#!/usr/bin/env bash
while [[ "\$1" == -* ]]; do [[ "\$1" == -o ]] && shift; shift; done
host="\$1"
shift
printf '%s\n' "\$host" >>"$test_root/push-ssh.log"
[[ "\$host" != feta ]] || exit "\${PUSH_FETA_EXIT:-0}"
[[ "\$*" != 'uname -sm' ]] || { echo 'Linux x86_64'; exit; }
home="$push_hosts/\$host"
cd "\$home"
HOME="\$home" XDG_CONFIG_HOME="\$home/.config" \
  PATH="\$home/.local/bin:$pushed_bin:$fake_bin:\$PATH" SHELL="$fake_bin/zsh" \
  LOGIN_SHELL_PATH="\$home/.local/bin:$fake_bin:/usr/bin:/bin" \
  SYSTEMCTL_LOG="$test_root/push-systemctl.log" LOGINCTL_LOG="$test_root/push-loginctl.log" \
  FLEET_INSTALL_UNAME_S=Linux FLEET_INSTALL_UNAME_M=x86_64 \
  FLEET_INSTALL_API_URL=https://test.invalid/api/v1 FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages \
  FLEET_INSTALL_CONFIRM_TIMEOUT_SECONDS=30 \
  bash -c "\$*"
SH
chmod 0755 "$push_bin/curl" "$push_bin/ssh"
run_push() {
  HOME="$test_root/home" PATH="$push_bin:$PATH" \
    FLEET_INSTALL_PACKAGE_URL=https://test.invalid/api/packages "$repo_root/scripts/fleet-push" "$@"
}
if PUSH_FETA_EXIT=1 FLEET_INSTALL_SKIP_CANARY=0 run_push "$generation_one" box-a >"$test_root/push-gate.out" 2>&1; then
  fail 'fleet-push pushed after a failed canary'
fi
grep -Fq 'nothing was pushed' "$test_root/push-gate.out" || fail 'fleet-push canary refusal was unclear'
grep -Fqx feta "$test_root/push-ssh.log" || fail 'fleet-push did not gate on feta'
test ! -e "$push_hosts/box-a/.local" || fail 'fleet-push touched a host after a failed canary'
: >"$test_root/push-curl.log"
run_push --skip-canary "$generation_one" box-a box-b >"$test_root/push.out" 2>&1 \
  || { cat "$test_root/push.out" >&2; fail 'fleet-push failed'; }
for host in box-a box-b; do
  test "$(link_generation "$push_hosts/$host/.local/opt/flotilla-fleet/current")" = "$generation_one" \
    || fail "fleet-push did not install on $host"
  test ! -e "$push_hosts/$host/.config/flotilla/fleet-reader-token" || fail "fleet-push left a token on $host"
  test -z "$(ls -A "$push_hosts/$host/.local/opt/flotilla-fleet/incoming")" \
    || fail "fleet-push left pushed packages on $host"
done
test "$(grep -c 'generation.json$' "$test_root/push-curl.log")" -eq 1 \
  || fail 'fleet-push should download the manifest once per push'
test "$(grep -c '\.tar\.gz$' "$test_root/push-curl.log")" -eq 1 \
  || fail 'fleet-push downloaded the archive more than once for one platform'

# The module unit tests cover Darwin copy policy through its public function.
"$repo_root/scripts/test-fleet-canary.sh"
"$repo_root/scripts/test-fleet-validation-retry.sh"
echo 'fleet-install contract passed'
