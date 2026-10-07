# Crews in one vessel share one display, including agent relaunches. The
# container owns the server lifetime; no host X server or TCP socket is used.
DISPLAY=:99
export DISPLAY
if ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
    flotilla_x11_state=$(mktemp -d)
    Xvfb "$DISPLAY" -screen 0 1280x1024x24 -nolisten tcp -auth /dev/null \
        >"$flotilla_x11_state/log" 2>&1 &
    flotilla_x11_pid=$!
    flotilla_x11_attempt=0
    # Concurrent launches may race to start the server. Opening the display
    # determines readiness even if another launch won that race.
    while ! xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; do
        if [ "$flotilla_x11_attempt" -ge 100 ]; then
            cat "$flotilla_x11_state/log" >&2
            kill "$flotilla_x11_pid" 2>/dev/null || true
            rm -rf "$flotilla_x11_state"
            echo 'Xvfb display did not become ready' >&2
            return 1
        fi
        sleep 0.1
        flotilla_x11_attempt=$((flotilla_x11_attempt + 1))
    done
    rm -rf "$flotilla_x11_state"
fi
