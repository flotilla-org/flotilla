# Embedded by the composer in the isolated builder stage. Only /out is exported.
set -eu
layer=$1
shift
root=${FLOTILLA_LINK_OUTPUT_ROOT:-/out}
private="/opt/flotilla/layers/$layer/"
test -d "$root${private}"
paths=$(mktemp)
trap 'rm -f "$paths"' EXIT
find "$root" -mindepth 1 -printf '%P\n' > "$paths"
while IFS= read -r relative; do
    path="/$relative"
    allowed=0
    for output do
        case "$output" in
            */) case "$path/" in "$output"*) allowed=1 ;; esac ;;
            *) test "$path" != "$output" || allowed=1 ;;
        esac
        # Necessary parent directories may be shared; they must be directories.
        case "$output" in "$path/"*) test ! -d "$root$path" || allowed=1 ;; esac
    done
    test "$allowed" = 1 || { echo "link output outside allowed paths: $path" >&2; exit 1; }
    if test -L "$root$path"; then
        case "$path" in
            /opt/flotilla/bin/*) target=$(readlink "$root$path"); case "$target" in "$private"*) ;; *) exit 1 ;; esac; resolved=$(realpath -m "$root$target"); case "$resolved" in "$root${private}"*) ;; *) exit 1 ;; esac; test -e "$resolved" ;;
            "$private"*) target=$(realpath -m "$root$path"); case "$target" in "$root${private}"*) ;; *) exit 1 ;; esac ;;
            *) exit 1 ;;
        esac
    else
        case "$path" in /opt/flotilla/bin/*) echo "bin output must be a symlink: $path" >&2; exit 1 ;; esac
    fi
done < "$paths"
for output do
    test -e "$root$output" || test -L "$root$output"
done
