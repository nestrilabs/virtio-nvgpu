#!/bin/bash
# Real applications through the Wayland proxy, one at a time, each for a slot
# the host side (scripts/rig-app-check.sh) watches: it waits for the line
#   APP_START <name>
# on the console, finds the app's window in the host compositor, captures it,
# and for the input apps types and clicks into it. What only the guest can
# see (the text that arrived, the events a client got) is checked here after
# the slot and reported as PASS/FAIL.
#
#   nvgpu_apps=a,b,c   which apps (default: all, in the order below)
#   nvgpu_slot=S       seconds per app (default 25)
. /opt/nvgpu/probe-common.sh
probe_init apps 0

SLOT=$(arg slot 25)
ALL=typing,pointer,clipboard,glxgears,xterm,gamescope,gtk,qt,firefox,mpv,glmark2,vkmark
APPS=$(arg apps "$ALL")

load_module || { fail "module did not load"; finish; }
start_wl_daemon wayland-0 || finish
export HOME=/root XDG_RUNTIME_DIR=/run/user/0 WAYLAND_DISPLAY=wayland-0
mkdir -p "$HOME" /tmp/apps /tmp/ffprofile
# Toolkits want a session bus; one per slot, gone with it.
# The image has no /etc/dbus-1; the package's own session.conf is enough.
dbus() {
    local conf
    conf=$(dirname "$(readlink -f "$(command -v dbus-daemon)")")/../share/dbus-1/session.conf
    if [ -r "$conf" ]; then dbus-run-session --config-file="$conf" -- "$@"; else "$@"; fi
}

# slot NAME CMD...: run CMD for $SLOT seconds, announced to the host.
slot() {
    local name=$1
    shift
    section "app $name"
    say "\$ $*"
    printf 'APP_START %s\n' "$name" >/dev/console
    # Through bash, so the helpers below (xwayland, dbus) run like commands.
    timeout -s TERM -k 5 "$SLOT" bash -c "$(declare -f xwayland dbus); \"\$@\"" _ "$@" \
        > "/tmp/apps/$name.log" 2>&1
    local rc=$?
    printf 'APP_END %s %s\n' "$name" "$rc" >/dev/console
    tail -n 15 "/tmp/apps/$name.log" | sed 's/^/    /'
    if [ "$rc" = 124 ] || [ "$rc" = 0 ]; then
        pass "$name ran its slot"
    else
        fail "$name exited early (status $rc)"
    fi
}

# An X server of its own for the X11 clients: rootful, so it is one ordinary
# xdg_toplevel to the compositor and needs no window manager on the host.
xwayland() {
    Xwayland :1 -geometry 1280x720 -noreset > /tmp/apps/xwayland.log 2>&1 &
    local x=$!
    for _ in $(seq 1 50); do [ -S /tmp/.X11-unix/X1 ] && break; sleep 0.1; done
    DISPLAY=:1 "$@"
    kill "$x" 2>/dev/null
}

IFS=, read -r -a want <<<"$APPS"
for a in "${want[@]}"; do
    case $a in
        typing)
            # Keyboard, end to end: the host types a line into a terminal
            # here, and the terminal's shell writes what it read.
            rm -f /tmp/typed
            slot typing foot --override='main.font=DejaVu Sans Mono:size=12' -e sh -c 'IFS= read -r l; printf "%s" "$l" > /tmp/typed; sleep 3600'
            got=$(cat /tmp/typed 2>/dev/null)
            if [ "$got" = "hello from the host" ]; then
                pass "keyboard: the terminal read \"$got\""
            else
                fail "keyboard: the terminal read \"$got\", not \"hello from the host\""
            fi
            ;;
        pointer)
            # Pointer and keys as a client sees them.
            # On a pty, so it writes each line as it goes: the slot ends it
            # with SIGTERM, which would lose whatever stdio still held.
            slot pointer script -qfec wev /dev/null
            m=$(grep -c 'wl_pointer.*motion' /tmp/apps/pointer.log)
            b=$(grep -c 'wl_pointer.*button' /tmp/apps/pointer.log)
            w=$(grep -c 'wl_pointer.*axis' /tmp/apps/pointer.log)
            k=$(grep -c 'wl_keyboard.*key' /tmp/apps/pointer.log)
            say "wev saw $m motion, $b button, $w axis, $k key event(s)"
            [ "$m" -gt 0 ] && [ "$b" -gt 0 ] && pass "pointer: motion and buttons arrived" ||
                fail "pointer: motion $m, buttons $b"
            ;;
        clipboard)
            # Host to guest (the host set the selection before the slot's
            # window took focus), then guest to host (the host reads it).
            slot clipboard foot --override='main.font=DejaVu Sans Mono:size=12' -e sh -c 'sleep 4; WAYLAND_DEBUG=1 wl-paste -n > /tmp/pasted 2>/tmp/paste.err; printf clip-from-guest | wl-copy; sleep 3600'
            got=$(cat /tmp/pasted 2>/dev/null)
            if [ "$got" = "clip-from-host" ]; then
                pass "clipboard host->guest: \"$got\""
            else
                fail "clipboard host->guest: got \"$got\""
                grep -aE 'data_device|data_offer|selection|receive|error' /tmp/paste.err | tail -n 25 | sed 's/^/    /'
            fi
            ;;
        glxgears) slot glxgears xwayland glxgears -info ;;
        xterm) slot xterm xwayland sh -c 'xterm -geometry 80x24+0+0 -e "echo X11 through Xwayland; sleep 3600" & xeyes -geometry 300x200+600+0 & wait' ;;
        gamescope) slot gamescope gamescope -W 1280 -H 720 -- vkcube ;;
        gtk) slot gtk dbus gnome-calculator ;;
        qt) slot qt dbus env QT_QPA_PLATFORM=wayland qalculate-qt ;;
        firefox)
            # A page of its own: WebGL2 animating, and its title saying
            # whether the context came up. (Firefox will not navigate to a
            # data: URL given on the command line.)
            cat > /tmp/page.html <<'HTML'
<!doctype html><title>webgl2 pending</title>
<body style="background:#246;color:white;font:40px sans-serif;margin:40px">
<h1>virtio-nvgpu</h1><canvas id=c width=640 height=360></canvas>
<script>
const g = document.getElementById('c').getContext('webgl2');
document.title = 'webgl2 ' + (g ? 'on: ' + g.getParameter(g.RENDERER) : 'off');
let t = 0;
(function f() { if (g) { g.clearColor((t % 120) / 120, 0.3, 0.6, 1); g.clear(g.COLOR_BUFFER_BIT); }
  t++; requestAnimationFrame(f); })();
</script>
HTML
            slot firefox dbus env MOZ_ENABLE_WAYLAND=1 firefox --no-remote --profile /tmp/ffprofile --new-instance file:///tmp/page.html
            ;;
        mpv) slot mpv mpv --no-config --vo=gpu-next --gpu-api=vulkan --loop 'av://lavfi:testsrc2=size=1280x720:rate=60' ;;
        glmark2) slot glmark2 glmark2-wayland --run-forever -b build:use-vbo=true ;;
        vkmark) slot vkmark vkmark --winsys wayland --run-forever -b vertex ;;
        *) skip "unknown app $a" ;;
    esac
done

wl_daemon_alive && pass "nvgpu-wl-guest still running" || fail "nvgpu-wl-guest died"
finish
