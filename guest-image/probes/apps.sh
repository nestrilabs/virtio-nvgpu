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
        ffsupport)
            # Firefox's own account of its graphics (about:support, Graphics):
            # the compositor, WebGL renderer, and what it blocklisted.
            slot ffsupport dbus env MOZ_ENABLE_WAYLAND=1 firefox --no-remote --profile /tmp/ffprofile --new-instance about:support
            ;;
        chromeanim)
            # Steady rendering and nothing else: does Chromium keep presenting?
            cat > /tmp/cpage.html <<'HTML'
<body style="font:28px sans-serif"><pre id=o></pre><canvas id=c width=640 height=200></canvas><script>
const g = document.getElementById('c').getContext('webgl2');
const d = g && g.getExtension('WEBGL_debug_renderer_info');
document.getElementById('o').textContent = 'WebGL2: ' + (g ? 'on' : 'off') + '\nvendor: ' +
  (d ? g.getParameter(d.UNMASKED_VENDOR_WEBGL) : '?') + '\nrenderer: ' + (d ? g.getParameter(d.UNMASKED_RENDERER_WEBGL) : '?') +
  '\nWebGPU: ' + ('gpu' in navigator ? 'present' : 'absent');
let t = 0;
(function f() { if (g) { g.clearColor((t % 120) / 120, .3, .6, 1); g.clear(g.COLOR_BUFFER_BIT); }
  t++; document.title = 'frame ' + t; requestAnimationFrame(f); })();
</script>
HTML
            slot chromeanim dbus chromium --no-sandbox --user-data-dir=/tmp/chromium2 --ozone-platform=wayland \
                --no-first-run --no-default-browser-check --enable-logging=stderr file:///tmp/cpage.html
            say "chromium GL errors: $(grep -ac 'incomplete: 0x00000000\|eglCreateSync failed' /tmp/apps/chromeanim.log)"
            ;;
        chromentp)
            slot chromentp dbus chromium --no-sandbox --user-data-dir=/tmp/chromium3 --ozone-platform=wayland \
                --no-first-run --no-default-browser-check --enable-logging=stderr
            say "chromium GL errors: $(grep -ac 'incomplete: 0x00000000\|eglCreateSync failed' /tmp/apps/chromentp.log)"
            grep -aE 'ERROR' /tmp/apps/chromentp.log | grep -avE 'dbus|crashpad' | head -n 3 | cut -c1-200 | sed 's/^/    /'
            ;;
        chromegpu)
            # Chromium's (chrome://gpu): feature status, GL/ANGLE renderer.
            # As root it needs --no-sandbox; Wayland by ozone.
            tr=()
            [ "$(arg trace 0)" = 1 ] && tr=(strace -f -qq -e trace=ioctl,mmap,openat -o /tmp/crst)
            [ "$(arg trace 0)" = 2 ] && tr=(strace -f -qq -k -e trace=mmap -e signal=none -o /tmp/crst)
            # What fills the low 2 GiB of the GPU process (MAP_32BIT's window).
            ( sleep 30; p=$(pgrep -f 'type=gpu-process' | head -n 1)
              [ -n "$p" ] && awk '{split($1,a,"-"); if (strtonum("0x" a[1]) < 0x80000000) print}' "/proc/$p/maps" > /tmp/gpumaps
              [ -n "$p" ] && head -c 4000 "/proc/$p/maps" > /tmp/gpumaps.head ) &
            slot chromegpu dbus "${tr[@]}" chromium --no-sandbox --user-data-dir=/tmp/chromium --ozone-platform=wayland \
                --no-first-run --no-default-browser-check --enable-logging=stderr chrome://gpu
            grep -aiE 'dawn|vulkan|webgpu|adapter|compatible|BackendType' /tmp/apps/chromegpu.log | cut -c1-220 | head -n 20 | sed 's/^/    /'
            grep -avE '^ ' /tmp/apps/chromegpu.log | grep -aE 'ERROR|FATAL' | grep -avE 'dbus|Fontconfig|sandbox|vaapi|crashpad' |
                cut -c1-220 | head -n 6 | sed 's/^/    /'
            if [ -s /tmp/gpumaps ]; then
                say "gpu-process mappings below 2 GiB: $(wc -l < /tmp/gpumaps)"
                head -n 5 /tmp/gpumaps | sed 's/^/    /'
                awk '{split($1,a,"-"); sz=strtonum("0x" a[2])-strtonum("0x" a[1]); n[$6]+=sz; c[$6]++} END {for (k in n) printf "%8.1f MiB %5d  %s\n", n[k]/1048576, c[k], k}' /tmp/gpumaps | sort -rn | head -n 12 | sed 's/^/    /'
            fi
            if [ "$(arg trace 0)" = 2 ] && [ -s /tmp/crst ]; then
                say "the low reservation, with its stack:"
                grep -anE 'mmap\((0x10000|NULL), 4[0-9]{9}' /tmp/crst | head -n 3 | cut -c1-200 | sed 's/^/    /'
                l=$(grep -anE 'mmap\((0x10000|NULL), 4[0-9]{9}' /tmp/crst | head -n 1 | cut -d: -f1)
                [ -n "$l" ] && sed -n "$((l+1)),$((l+25))p" /tmp/crst | cut -c1-200 | sed 's/^/    /'
            elif [ -s /tmp/crst ]; then
                say "failed calls (strace):"
                grep -aE '= -1 E' /tmp/crst | grep -avE 'ENOENT|EAGAIN|ENOTTY|EACCES' |
                    sed -E 's/^[0-9]+ +//; s/0x[0-9a-f]{6,}/ADDR/g' | cut -c1-160 | sort | uniq -c | sort -rn | head -n 20 | sed 's/^/    /'
            fi
            ;;
        mpv) slot mpv mpv --no-config --vo=gpu-next --gpu-api=vulkan --loop 'av://lavfi:testsrc2=size=1280x720:rate=60' ;;
        glmark2) slot glmark2 glmark2-wayland --run-forever -b build:use-vbo=true ;;
        vkmark) slot vkmark vkmark --winsys wayland --run-forever -b vertex ;;
        *) skip "unknown app $a" ;;
    esac
done

wl_daemon_alive && pass "nvgpu-wl-guest still running" || fail "nvgpu-wl-guest died"
finish
