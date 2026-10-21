#!/bin/sh
set -eu
dir=$(dirname "$0")
mode=$(cat "$dir/mode")
printf 'spawn\n' >> "$dir/spawns"
printf '%s\n' "$@" >> "$dir/args"
case "$mode" in
    close-first-child-stdin)
        if [ ! -e "$dir/first-child" ]; then
            touch "$dir/first-child"
            IFS= read -r first_line
            exec 0<&-
            touch "$dir/stdin-closed"
            sleep 5
            exit 0
        fi
        ;;
    exit-first-child)
        if [ ! -e "$dir/first-child" ]; then
            touch "$dir/first-child"
            IFS= read -r first_line
            touch "$dir/first-child-exited"
            exit 0
        fi
        ;;
    block-stdin)
        touch "$dir/blocked"
        while [ ! -e "$dir/unblock" ]; do
            sleep 0.01
        done
        ;;
esac
while IFS= read -r line; do
    printf '%s\n' "$line" >> "$dir/lines"
done
