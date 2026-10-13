# Run preludes and their command in one subshell of the caller's shell. Exports
# reach the command, while error traps/options never replace caller cleanup.
flotilla_run_with_preludes() (
    # Sourced in the launch shell, so exports reach the agent/probe. No image
    # entrypoint or installed Flotilla executable is required for arbitrary images.
    flotilla_prelude_dir=${FLOTILLA_PRELUDE_DIR:-/etc/flotilla/prelude.d}
    # An unsearchable parent hides the directory from `[ -d ]`; refuse rather
    # than launch without the preludes the image declared.
    flotilla_prelude_parent=${flotilla_prelude_dir%/*}
    if { [ -e "$flotilla_prelude_dir" ] && { [ ! -r "$flotilla_prelude_dir" ] || [ ! -x "$flotilla_prelude_dir" ]; }; } \
        || { [ ! -e "$flotilla_prelude_dir" ] && [ -n "$flotilla_prelude_parent" ] && [ -d "$flotilla_prelude_parent" ] \
            && [ ! -x "$flotilla_prelude_parent" ]; }; then
        printf 'Flotilla prelude directory is not readable: %s\n' "$flotilla_prelude_dir" >&2
        exit 1
    fi
    if [ -d "$flotilla_prelude_dir" ]; then
        flotilla_prelude_restore_nomatch=0
        if [ -n "${ZSH_VERSION-}" ] && [[ -o nomatch ]]; then
            setopt nonomatch
            flotilla_prelude_restore_nomatch=1
        fi
        case $- in *e*) flotilla_prelude_restore_errexit=0 ;; *) flotilla_prelude_restore_errexit=1 ;; esac
        set -e
        trap 'flotilla_prelude_status=$?; if [ "$flotilla_prelude_status" -ne 0 ]; then printf "Flotilla prelude failed: %s (exit %s)\n" "$flotilla_prelude_file" "$flotilla_prelude_status" >&2; fi' 0
        for flotilla_prelude_file in "$flotilla_prelude_dir"/*; do
            [ -f "$flotilla_prelude_file" ] || continue
            . "$flotilla_prelude_file"
        done
        trap - 0
        if [ "$flotilla_prelude_restore_errexit" -eq 1 ]; then set +e; fi
        if [ "$flotilla_prelude_restore_nomatch" -eq 1 ]; then setopt nomatch; fi
    fi
    "$@"
)
