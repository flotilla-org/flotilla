set -eu

helper_name=$1
helper_hash=$2
content=$3

if [ -n "${FLOTILLA_ROOT:-}" ]; then
  state_dir="$FLOTILLA_ROOT/state"
elif [ -n "${XDG_STATE_HOME:-}" ]; then
  state_dir="$XDG_STATE_HOME/flotilla"
elif [ -n "${HOME:-}" ]; then
  state_dir="$HOME/.local/state/flotilla"
else
  echo "cannot resolve flotilla state dir for managed helper install" >&2
  exit 1
fi

helper_dir="$state_dir/helpers/$helper_hash"
helper_path="$helper_dir/$helper_name"

if [ -x "$helper_path" ]; then
  printf '%s\n' "$helper_path"
  exit 0
fi

mkdir -p "$helper_dir"
temp="$helper_dir/.install-script.$$"
trap 'rm -f "$temp"' EXIT
printf '%s' "$content" > "$temp"
chmod +x "$temp"
mv "$temp" "$helper_path"
trap - EXIT
printf '%s\n' "$helper_path"
