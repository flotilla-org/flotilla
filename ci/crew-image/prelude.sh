# Sourced in the launch shell, so exports reach the agent/probe. No image
# entrypoint or installed Flotilla executable is required for arbitrary images.
flotilla_prelude_dir=${FLOTILLA_PRELUDE_DIR:-/etc/flotilla/prelude.d}
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
