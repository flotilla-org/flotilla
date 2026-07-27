#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
libexec_dir=$HOME/.local/libexec/flotilla

if [[ ! -x $HOME/.cargo/bin/cargo-sweep ]] && ! command -v cargo-sweep >/dev/null 2>&1; then
  cargo install cargo-sweep --version 0.8.0 --locked
fi

install -d "$libexec_dir"
install -m 0755 "$repo_root/scripts/cargo-sweep-mtime.sh" "$libexec_dir/cargo-sweep-mtime.sh"

case $(uname -s) in
  Linux)
    unit_dir=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
    install -d "$unit_dir"
    install -m 0644 "$repo_root/config/systemd/user/flotilla-cargo-sweep-mtime.service" "$unit_dir/"
    install -m 0644 "$repo_root/config/systemd/user/flotilla-cargo-sweep-mtime.timer" "$unit_dir/"
    systemctl --user daemon-reload
    systemctl --user enable --now flotilla-cargo-sweep-mtime.timer
    systemctl --user start flotilla-cargo-sweep-mtime.service
    systemctl --user status --no-pager flotilla-cargo-sweep-mtime.timer
    ;;
  Darwin)
    agent_dir=$HOME/Library/LaunchAgents
    plist=$agent_dir/org.flotilla.cargo-sweep-mtime.plist
    install -d "$agent_dir"
    install -m 0644 "$repo_root/config/launchd/org.flotilla.cargo-sweep-mtime.plist" "$plist"
    launchctl bootout "gui/$UID/org.flotilla.cargo-sweep-mtime" 2>/dev/null || true
    launchctl bootstrap "gui/$UID" "$plist"
    launchctl kickstart -k "gui/$UID/org.flotilla.cargo-sweep-mtime"
    launchctl print "gui/$UID/org.flotilla.cargo-sweep-mtime"
    ;;
  *)
    echo "Unsupported operating system: $(uname -s)" >&2
    exit 1
    ;;
esac

echo "Installed daily mtime-based cargo sweep. Inspect reclaimed bytes in:"
echo "  ${XDG_STATE_HOME:-"$HOME/.local/state"}/flotilla/cargo-sweep-mtime.log"
