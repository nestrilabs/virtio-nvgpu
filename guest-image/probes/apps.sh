#!/bin/bash
# Real applications through the Wayland proxy, one at a time, each for a slot
# the host side (scripts/rig-app-check.sh) watches: it waits for the line
#   APP_START <name>
# on the console, finds the app's window in the host compositor, captures it,
# and for the input apps types and clicks into it. What only the guest can
# see (the text that arrived, the events a client got) is checked here after
# the slot and reported as PASS/FAIL.
#
#   nvgpu_apps=a,b,c   which apps (default: the first set below; the rest
#                      are named one by one, TESTING-RIG.md has batches)
#   nvgpu_slot=S       seconds per app (default 25; a slow starter gets more)
#   nvgpu_live=1       the host is the live desktop (rig-app-check.sh --live):
#                      nothing will be typed, so no slot waits for input
#
# A slot is announced as "APP_START <name> [wait=S] [settle=S]": wait is how
# long the host looks for the window, settle a pause before an extra
# capture. A step with no window of its own (an encode, a CUDA sample) is
# "APP_START <name> nowin": the host only records it.
. /opt/nvgpu/probe-common.sh
probe_init apps 0

SLOT=$(arg slot 25)
ALL=typing,pointer,clipboard,glxgears,xterm,gamescope,gtk,qt,firefox,mpv,glmark2,vkmark
APPS=$(arg apps "$ALL")

load_module || { fail "module did not load"; finish; }

# nvgpu_user=1: the apps run as an unprivileged user, as a sandbox would run
# them -- uid 1000 in video and render, the render node group-accessible, the
# Wayland daemon that user's own (it owns /dev/nvgpu-wl, as the udev rule's
# nvgpu-wl group gives a session), and the browsers with their own sandboxes
# on. Default: everything as root, as the image first did.
USERMODE=$(arg user 0)
AS=()
if [ "$USERMODE" = 1 ]; then
    grep -q '^app:' /etc/passwd || {
        echo 'app:x:1000:1000:app:/home/app:/bin/sh' >> /etc/passwd
        echo 'app:x:1000:' >> /etc/group
        # Into the image's own groups (video, render, nvgpu-wl exist there).
        # The apps are NOT in nvgpu-wl: only the daemon may open
        # /dev/nvgpu-wl (CONNECT_FOR names the client process it proxies),
        # as a setgid daemon would have it; below it gets the group alone.
        for g in video render; do
            sed -i -E "s/^($g:x:[0-9]+:)(.*)\$/\1\2,app/; s/^($g:x:[0-9]+:),app\$/\1app/" /etc/group
        done
    }
    mkdir -p /home/app /run/user/1000
    chown 1000:1000 /home/app /run/user/1000
    chmod 0700 /run/user/1000
    for n in /dev/dri/renderD*; do chgrp render "$n"; chmod 0660 "$n"; done
    for n in /dev/dri/card*; do chgrp video "$n"; chmod 0660 "$n"; done
    [ -e /dev/nvgpu-wl ] && { chgrp nvgpu-wl /dev/nvgpu-wl; chmod 0660 /dev/nvgpu-wl; }
    AS=(setpriv --reuid=1000 --regid=1000 --init-groups env HOME=/home/app USER=app XDG_RUNTIME_DIR=/run/user/1000)
    export HOME=/home/app XDG_RUNTIME_DIR=/run/user/1000
    say "apps run as $(id -un 1000 2>/dev/null || echo 1000): groups video render (not nvgpu-wl: only the daemon), browsers sandboxed"
    nvgid=$(awk -F: '$1 == "nvgpu-wl" {print $3}' /etc/group)
    bg setpriv --reuid=1000 --regid=1000 --groups="$(id -G 1000 | tr ' ' ','),$nvgid" \
        env HOME=/home/app USER=app XDG_RUNTIME_DIR=/run/user/1000 nvgpu-wl-guest --socket wayland-0
    WL_PID=${BG_PIDS[-1]}
    for _ in $(seq 1 100); do [ -S /run/user/1000/wayland-0 ] && break; sleep 0.1; done
    if [ -S /run/user/1000/wayland-0 ]; then pass "nvgpu-wl-guest (as app) serving /run/user/1000/wayland-0"
    else fail "nvgpu-wl-guest (as app) never listened"; finish; fi
else
    start_wl_daemon wayland-0 || finish
    export HOME=/root XDG_RUNTIME_DIR=/run/user/0
fi
export WAYLAND_DISPLAY=wayland-0
# Qt logs to journald when it finds its socket path, and there is no journal
# here: to stderr, so the slots' logs have it.
export QT_FORCE_STDERR_LOGGING=1
mkdir -p "$HOME" /tmp/apps "$HOME/ffprofile"
[ "$USERMODE" = 1 ] && chown -R 1000:1000 "$HOME"
NOSANDBOX=--no-sandbox
[ "$USERMODE" = 1 ] && NOSANDBOX=''
# The apps write their results under /tmp (the typed line, the paste, X sockets).
chmod 1777 /tmp /tmp/apps
# Toolkits want a session bus; one per slot, gone with it.
# The image has no /etc/dbus-1; the package's own session.conf is enough.
dbus() {
    local conf
    conf=$(dirname "$(readlink -f "$(command -v dbus-daemon)")")/../share/dbus-1/session.conf
    if [ -r "$conf" ]; then dbus-run-session --config-file="$conf" -- "$@"; else "$@"; fi
}

# slot NAME CMD...: run CMD for $SLOT seconds, announced to the host.
# SLOTLEN=S before it gives this slot S seconds instead (never fewer than
# $SLOT); OPTS adds to the announcement (wait=S, settle=S). KNOWN="why", when
# set, makes an early exit a SKIP: a failure that is the same natively
# (TESTING-RIG.md, "Application pass", says how each was shown).
slot() {
    local name=$1 len=${SLOTLEN:-$SLOT}
    shift
    [ "$len" -lt "$SLOT" ] && len=$SLOT
    section "app $name"
    say "\$ $*"
    printf '\nAPP_START %s %s\n' "$name" "${OPTS:-}" >/dev/console
    # Through bash, so the helpers below (xwayland, dbus) run like commands.
    timeout -s TERM -k 5 "$len" "${AS[@]}" bash -c "$(declare -f xwayland dbus); \"\$@\"" _ "$@" \
        > "/tmp/apps/$name.log" 2>&1
    local rc=$?
    SLOT_RC=$rc
    printf 'APP_END %s %s\n' "$name" "$rc" >/dev/console
    # A failure shows more of its log; every log is also kept in
    # /var/log/nvgpu/apps (NVGPU_KEEP_ROOTFS=1 keeps the disk).
    if [ "$rc" = 124 ] || [ "$rc" = 0 ]; then n=15; else n=60; fi
    tail -n "$n" "/tmp/apps/$name.log" | cut -c1-300 | sed 's/^/    /'
    if [ "$rc" = 124 ] || [ "$rc" = 0 ]; then
        pass "$name ran its slot"
    elif [ -n "${KNOWN:-}" ]; then
        skip "$name exited early (status $rc): ${KNOWN}; it does the same natively"
    else
        fail "$name exited early (status $rc)"
    fi
}

# task NAME SECS CMD...: a step with no window (an encode, a CUDA sample):
# PASS on exit 0 within SECS. Its output is /tmp/apps/NAME.log.
task() {
    local name=$1 secs=$2
    shift 2
    section "task $name"
    say "\$ $*"
    printf '\nAPP_START %s nowin\n' "$name" >/dev/console
    timeout -s TERM -k 5 "$secs" "${AS[@]}" bash -c "$(declare -f xwayland dbus); \"\$@\"" _ "$@" \
        > "/tmp/apps/$name.log" 2>&1
    local rc=$?
    SLOT_RC=$rc
    printf 'APP_END %s %s\n' "$name" "$rc" >/dev/console
    if [ "$rc" = 0 ]; then n=12; else n=50; fi
    tail -n "$n" "/tmp/apps/$name.log" | cut -c1-240 | sed 's/^/    /'
    if [ "$rc" = 0 ]; then
        pass "$name"
    elif [ -n "${KNOWN:-}" ]; then
        skip "$name (status $rc): ${KNOWN}; it does the same natively"
    else
        fail "$name (status $rc)"
    fi
    return "$rc"
}

# check NAME DESC PATTERN [FILE...]: PASS when PATTERN (an ERE) is in the
# slot's log (or FILEs), and shows the first matching line.
check() {
    local name=$1 desc=$2 pat=$3 m
    shift 3
    [ $# -gt 0 ] || set -- "/tmp/apps/$name.log"
    m=$(cat "$@" 2>/dev/null | tr -d '\r' | grep -aE -m 1 -- "$pat")
    if [ -n "$m" ]; then
        pass "$name: $desc: $(printf '%s' "$m" | cut -c1-200)"
    else
        fail "$name: $desc (no /$pat/ in $*)"
    fi
}

# The last "fps=" and "speed=" of an ffmpeg run's progress (a run under a
# second has no fps: ffmpeg prints 0.0).
ff_fps() { tr '\r' '\n' < "$1" | grep -aoE 'fps= *[0-9.]+' | tail -n 1 | tr -d ' '; }
ff_speed() { tr '\r' '\n' < "$1" | grep -aoE 'speed= *[0-9.]+x' | tail -n 1 | tr -d ' '; }

# Firefox profiles for the page slots: console.log to stdout, no first-run
# pages, autoplay; VAAPI=1 also turns FFmpeg's VA-API decoding on.
ffprofile() {
    local dir=$1 vaapi=${2:-0}
    mkdir -p "$dir"
    cat > "$dir/user.js" <<'PREFS'
user_pref("devtools.console.stdout.content", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("media.autoplay.default", 0);
PREFS
    [ "$vaapi" = 1 ] && cat >> "$dir/user.js" <<'PREFS'
user_pref("media.ffmpeg.vaapi.enabled", true);
user_pref("media.hardware-video-decoding.force-enabled", true);
user_pref("media.rdd-ffmpeg.enabled", true);
user_pref("widget.dmabuf.force-enabled", true);
PREFS
    [ "$USERMODE" = 1 ] && chown -R 1000:1000 "$dir"
}

A=/opt/nvgpu/apps
PAGE=file://$A/www/gl.html
LIVE=$(arg live 0)
COMPUTE=$(arg compute 0)

# An X server of its own for the X11 clients: rootful, so it is one ordinary
# xdg_toplevel to the compositor and needs no window manager on the host.
xwayland() {
    Xwayland :1 -geometry 1280x720 -noreset > "$HOME/xwayland.log" 2>&1 &
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
            slot firefox dbus env MOZ_ENABLE_WAYLAND=1 firefox --no-remote --profile "$HOME/ffprofile" --new-instance file:///tmp/page.html
            ;;
        ffsupport)
            # Firefox's own account of its graphics (about:support, Graphics):
            # the compositor, WebGL renderer, and what it blocklisted.
            slot ffsupport dbus env MOZ_ENABLE_WAYLAND=1 firefox --no-remote --profile "$HOME/ffprofile" --new-instance about:support
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
            slot chromeanim dbus chromium $NOSANDBOX --user-data-dir="$HOME/chromium2" --ozone-platform=wayland \
                --no-first-run --no-default-browser-check --enable-logging=stderr file:///tmp/cpage.html
            say "chromium GL errors: $(grep -ac 'incomplete: 0x00000000\|eglCreateSync failed' /tmp/apps/chromeanim.log)"
            ;;
        chromentp)
            slot chromentp dbus chromium $NOSANDBOX --user-data-dir="$HOME/chromium3" --ozone-platform=wayland \
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
            slot chromegpu dbus "${tr[@]}" chromium $NOSANDBOX --user-data-dir="$HOME/chromium" --ozone-platform=wayland \
                --no-first-run --no-default-browser-check --enable-logging=stderr \
                --vmodule='*/gpu/*=1,*/ui/gl/*=1,*dawn*=2,*webgpu*=2' $(arg chromeflags "" | tr ',' ' ') chrome://gpu
            l=$(grep -anE 'incomplete: 0x00000000|MakeFromBackendTexture|eglCreateSync failed' /tmp/apps/chromegpu.log | head -n 1 | cut -d: -f1)
            say "chromium log before its first GL error (line $l):"
            [ -n "$l" ] && sed -n "$((l > 60 ? l - 60 : 1)),$((l + 2))p" /tmp/apps/chromegpu.log | grep -avE '^ |^$|wayland_object|Binding to' |
                cut -c1-230 | sed 's/^/    /'
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
        # ---- Games and engines --------------------------------------------
        stk)
            # SuperTuxKart (GL 3.3+/4.x through SDL2, native Wayland): a
            # self-driving profile race, which prints its frame rate.
            SLOTLEN=50 OPTS="wait=40 settle=15" slot stk supertuxkart --no-start-screen --track=lighthouse \
                --numkarts=4 --laps=1 --profile-time=35 --windowed
            check stk "GL renderer" 'NVIDIA|GeForce'
            check stk "profile result" '[Ff]ps|frames'
            ;;
        stkgs)
            # The same game nested in gamescope: an X11 client of gamescope's
            # own Xwayland, composited by gamescope with Vulkan. The race
            # outlasts the slot: gamescope 3.16 segfaults as it tears down
            # after its child exits (TESTING-RIG.md), which is not this test.
            SLOTLEN=55 OPTS="wait=40 settle=15" slot stkgs gamescope -W 1280 -H 720 -- supertuxkart \
                --no-start-screen --track=lighthouse --numkarts=4 --laps=1 --profile-time=90 --windowed
            check stkgs "GL renderer" 'NVIDIA|GeForce'
            ;;
        neverball)
            # SDL2 + GL (GLX) through a rootful Xwayland, with the driver's
            # own on-screen API/fps overlay; glxinfo first, in the same server.
            # (SDL prefers Wayland whenever WAYLAND_DISPLAY is set: told X11.)
            SLOTLEN=30 slot neverball xwayland env -u WAYLAND_DISPLAY SDL_VIDEODRIVER=x11 SDL_VIDEO_DRIVER=x11 \
                sh -c 'glxinfo -B > /tmp/apps/neverball.glx 2>&1; __GL_SHOW_GRAPHICS_OSD=1 neverball'
            check neverball "GLX renderer (Xwayland)" 'OpenGL renderer string: .*NVIDIA' /tmp/apps/neverball.glx
            ;;
        godot | godotgl)
            # Godot 4 on native Wayland: Vulkan (Forward+), or GL
            # (compatibility). 400 lit, shadowed cubes with SSAO and glow.
            rm -rf /tmp/godot && cp -r "$A/godot" /tmp/godot && chmod -R a+rwX /tmp/godot
            drv=(--rendering-driver vulkan --rendering-method forward_plus)
            [ "$a" = godotgl ] && drv=(--rendering-driver opengl3 --rendering-method gl_compatibility)
            # Forward+ enables every ray tracing extension the device lists,
            # and NVIDIA's driver lists them without /dev/nvidia-uvm but
            # cannot create a device with them (VK_ERROR_INITIALIZATION_FAILED):
            # natively the same with the node hidden (TESTING-RIG.md).
            kn=
            [ "$a" = godot ] && [ "$COMPUTE" != 1 ] &&
                kn="no UVM (graphics-only guest): NVIDIA's vkCreateDevice fails with the ray tracing extensions it lists"
            KNOWN=$kn SLOTLEN=35 OPTS="wait=30" slot "$a" godot --path /tmp/godot --display-driver wayland "${drv[@]}" --verbose
            if [ -n "$kn" ] && [ "$SLOT_RC" != 124 ] && grep -q 'Couldn.t create Vulkan device (VkResult error -3)' "/tmp/apps/$a.log"; then
                skip "$a: renderer and frame rate (no device, as above)"
            else
                check "$a" "renderer" 'NVGPU_GODOT driver=.*NVIDIA'
                check "$a" "frame rate" 'NVGPU_GODOT fps='
            fi
            ;;
        # ---- Creative -------------------------------------------------------
        blender | blendervk)
            # Blender's UI (GL, or its Vulkan backend) with the default scene
            # in the viewport, and one EEVEE frame rendered from it.
            be=()
            [ "$a" = blendervk ] && be=(--gpu-backend vulkan)
            SLOTLEN=60 OPTS="wait=45 settle=20" slot "$a" blender --factory-startup "${be[@]}" --python "$A/blender/gl.py"
            check "$a" "GPU backend" 'NVGPU_BLENDER backend=.*NVIDIA'
            check "$a" "EEVEE frame" 'NVGPU_BLENDER eevee frame rendered'
            ls -l /tmp/apps/blender-eevee-*.png 2>/dev/null | sed 's/^/    /'
            ;;
        cycles)
            # Cycles on the GPU, in the background: CUDA, then OptiX. Needs
            # --allow-compute (CUDA is UVM).
            for dev in CUDA OPTIX; do
                task "cycles-$dev" 240 blender -b --factory-startup --python "$A/blender/cycles.py" -- "$dev"
                check "cycles-$dev" "device" "NVGPU_CYCLES devices.*'$dev'"
                check "cycles-$dev" "frame" "NVGPU_CYCLES $dev frame rendered"
            done
            ;;
        gimp) SLOTLEN=50 OPTS="wait=45 settle=10" slot gimp dbus gimp -n -s "$A/frame.png" ;;
        inkscape) SLOTLEN=45 OPTS="wait=40 settle=10" slot inkscape dbus inkscape "$A/drawing.svg" ;;
        krita)
            # Qt6 with Krita's OpenGL canvas. Krita picks Qt's xcb platform
            # itself, Wayland session or not: through Xwayland, as on a
            # Wayland desktop.
            SLOTLEN=60 OPTS="wait=55 settle=15" slot krita xwayland dbus krita --nosplash "$A/frame.png"
            grep -ahiE 'opengl|renderer|canvas' /tmp/apps/krita.log "$HOME"/.local/share/krita.log 2>/dev/null | head -n 6 | cut -c1-200 | sed 's/^/    /'
            check krita "OpenGL canvas" 'NVIDIA' /tmp/apps/krita.log "$HOME"/.local/share/krita.log "$HOME"/.local/share/krita-sysinfo.log
            ;;
        # ---- Office and toolkits --------------------------------------------
        lo)
            # LibreOffice Writer on its GTK3 VCL, native Wayland (cairo).
            SLOTLEN=45 OPTS="wait=40 settle=10" slot lo dbus soffice --norestore --nologo --writer
            ;;
        loskia)
            # LibreOffice on the X11 VCL through Xwayland, where it draws with
            # Skia on Vulkan; its skia.log says what it got.
            # nixpkgs' LibreOffice does not have the Vulkan loader on its
            # path (Skia dlopen()s libvulkan.so.1): the probe gives it one.
            rm -rf "$HOME/.config/libreoffice"
            vkl=$(dirname "$(ls /nix/store/*-vulkan-loader-*/lib/libvulkan.so.1 2>/dev/null | head -n 1)")
            SLOTLEN=45 OPTS="wait=40 settle=10" slot loskia xwayland env SAL_USE_VCLPLUGIN=gen SAL_FORCESKIA=1 SAL_SKIA=vulkan \
                LD_LIBRARY_PATH="$vkl" soffice --norestore --nologo --writer
            sed 's/^/    /' "$HOME"/.config/libreoffice/4/cache/skia.log 2>/dev/null | head -n 12
            check loskia "Skia on Vulkan" 'RenderMethod: vulkan' "$HOME"/.config/libreoffice/4/cache/skia.log
            check loskia "Skia device" 'Vendor: 0x10de|NVIDIA' "$HOME"/.config/libreoffice/4/cache/skia.log
            ;;
        gte | gtevk)
            # A GTK4 app on GSK's GL renderer (ngl), then its Vulkan one.
            r=ngl
            [ "$a" = gtevk ] && r=vulkan
            OPTS="wait=25" slot "$a" dbus env GSK_RENDERER=$r GSK_DEBUG=renderer GDK_DEBUG=opengl,vulkan \
                gnome-text-editor --standalone "$A/qml/scene.qml"
            check "$a" "GSK renderer $r" "$([ "$r" = ngl ] && echo 'GskGLRenderer|using.*GL|OpenGL.*NVIDIA' || echo 'GskVulkanRenderer|Vulkan.*NVIDIA')"
            ;;
        qml | qmlvk)
            # Qt Quick (the qml runtime): its scene graph on GL, then Vulkan.
            rhi=opengl
            [ "$a" = qmlvk ] && rhi=vulkan
            OPTS="wait=25" slot "$a" env QSG_INFO=1 QSG_RHI_BACKEND=$rhi QT_QPA_PLATFORM=wayland nvgpu-qml "$A/qml/scene.qml"
            check "$a" "scene graph on $rhi" "NVGPU_QML api=$rhi"
            check "$a" "device" 'NVIDIA'
            grep -a 'NVGPU_QML' "/tmp/apps/$a.log" | tail -n 2 | sed 's/^/    /'
            ;;
        # ---- Electron --------------------------------------------------------
        electron)
            # Electron with the WebGL and video page, and its GPU feature
            # status from the main process.
            OPTS="wait=30 settle=10" SLOTLEN=35 slot electron dbus electron --ozone-platform=wayland \
                --enable-logging=stderr "$A/electron"
            grep -a 'NVGPU_ELECTRON' /tmp/apps/electron.log | cut -c1-700 | sed 's/^/    /'
            check electron "GPU compositing" '"gpu_compositing":"enabled'
            # (Electron 43 lists no webgl2 feature: WebGL2 is the page's check.)
            check electron "WebGL" '"webgl":"enabled'
            check electron "GL renderer" 'NVGPU_ELECTRON gpu .*(NVIDIA|"vendorId":4318)'
            check electron "page" 'NVGPU_PAGE webgl1=on webgl2=on nvidia=yes'
            ;;
        element)
            # Element: it stops at a "System unsupported" dialog (no keyring:
            # the guest runs no secret service), natively the same; the
            # dialog is its GPU-composited window all the same.
            SLOTLEN=35 OPTS="wait=30 settle=10" slot element dbus element-desktop --ozone-platform=wayland \
                --password-store=basic --enable-logging=stderr
            grep -aiE 'unsupported|safeStorage|keytar|seshat|password' /tmp/apps/element.log | head -n 5 | cut -c1-200 | sed 's/^/    /'
            ;;
        # ---- Browsers: WebGL 1/2 and video ------------------------------------
        ffgl | ffvaapi)
            # Firefox (Wayland, content sandbox on as uid 1000) with the WebGL
            # and video page; ffvaapi turns VA-API decoding on (NVDEC through
            # nvidia-vaapi-driver: CUDA, so --allow-compute).
            ffprofile "$HOME/ff-$a" "$([ "$a" = ffvaapi ] && echo 1 || echo 0)"
            env=(MOZ_ENABLE_WAYLAND=1 MOZ_LOG=PlatformDecoderModule:4,Dmabuf:4)
            [ "$a" = ffvaapi ] && env+=(LIBVA_DRIVER_NAME=nvidia NVD_BACKEND=direct NVD_LOG=1)
            SLOTLEN=35 OPTS="wait=30 settle=10" slot "$a" dbus env "${env[@]}" firefox --name nvgpu-firefox --no-remote \
                --profile "$HOME/ff-$a" --new-instance "$PAGE?v=h264.mp4"
            check "$a" "WebGL 1 and 2 on NVIDIA" 'NVGPU_PAGE webgl1=on webgl2=on nvidia=yes'
            check "$a" "video plays" 'NVGPU_PAGE .*video=playing'
            grep -aE 'NVGPU_PAGE' "/tmp/apps/$a.log" | tail -n 1 | cut -c1-300 | sed 's/^/    /'
            grep -aiE 'vaapi|va-api|hardware|FFmpegVideoDecoder|decoder' "/tmp/apps/$a.log" | sort | uniq -c | sort -rn | head -n 8 | cut -c1-200 | sed 's/^/    /'
            # Firefox's RDD sandbox (on: the apps run sandboxed) keeps
            # nvidia-vaapi-driver from its device nodes, and vaInitialize
            # fails in the RDD process: natively the same; it works with
            # MOZ_DISABLE_RDD_SANDBOX=1, which this pass does not set.
            if [ "$a" = ffvaapi ]; then
                if grep -aq 'vaInitialize failed' "/tmp/apps/$a.log"; then
                    skip "$a: VA-API in Firefox's RDD process: vaInitialize fails under its sandbox, natively the same (software decode)"
                else
                    check "$a" "VA-API decoder" 'VA-API|VAAPI.*(created|success|using)'
                fi
            fi
            ;;
        crgl | crvaapi)
            # Chromium (Wayland, sandbox on as uid 1000) with the same page;
            # crvaapi asks for VA-API decode on NVIDIA.
            fl=()
            env=()
            [ "$a" = crvaapi ] && {
                fl=(--enable-features=VaapiVideoDecoder,VaapiOnNvidiaGPUs,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL
                    --vmodule='*/media/gpu/*=2,*vaapi*=2')
                env=(LIBVA_DRIVER_NAME=nvidia NVD_BACKEND=direct)
            }
            SLOTLEN=35 OPTS="wait=30 settle=10" slot "$a" dbus env "${env[@]}" chromium $NOSANDBOX --user-data-dir="$HOME/cr-$a" \
                --class=nvgpu-chromium --ozone-platform=wayland --no-first-run --no-default-browser-check \
                --autoplay-policy=no-user-gesture-required --enable-logging=stderr "${fl[@]}" "$PAGE?v=h264.mp4"
            check "$a" "WebGL 1 and 2 on NVIDIA" 'NVGPU_PAGE webgl1=on webgl2=on nvidia=yes'
            check "$a" "video plays" 'NVGPU_PAGE .*video=playing'
            grep -aE 'NVGPU_PAGE' "/tmp/apps/$a.log" | tail -n 1 | cut -c1-300 | sed 's/^/    /'
            [ "$a" = crvaapi ] && grep -aiE 'vaapi|VideoDecoder|hardware' "/tmp/apps/$a.log" | cut -c1-200 | sort | uniq -c | sort -rn | head -n 8 | sed 's/^/    /'
            [ "$a" = crvaapi ] && check "$a" "VA-API decoder" 'VaapiVideoDecoder.*(Initialize|created)|vaapi.*(success|decod)'
            ;;
        # ---- Video ------------------------------------------------------------
        mpvvk | mpvnvdec | mpvvaapi)
            # mpv on its Vulkan renderer, decoding H.264, HEVC and AV1 in
            # hardware: Vulkan Video (no CUDA), NVDEC (CUDA), or VA-API
            # (nvidia-vaapi-driver, on CUDA). Its log says what it used.
            hw=${a#mpv}
            env=()
            [ "$hw" = vk ] && hw=vulkan
            [ "$hw" = vaapi ] && env=(LIBVA_DRIVER_NAME=nvidia NVD_BACKEND=direct)
            SLOTLEN=36 OPTS="wait=20 settle=8" slot "$a" env "${env[@]}" mpv --no-config --vo=gpu-next --gpu-api=vulkan \
                --hwdec="$hw" --msg-level=vd=v,vo=v --osd-level=3 --osd-msg3='${hwdec-current} ${video-codec} ${estimated-vf-fps} fps' \
                "$A/www/h264.mp4" "$A/www/hevc.mp4" "$A/www/av1.mp4"
            grep -aE 'Using hardware decoding|Could not create|hwdec.*(fail|not)|Opening done|VO:' "/tmp/apps/$a.log" | cut -c1-160 | sed 's/^/    /' | head -n 12
            n=$(grep -ac "Using hardware decoding ($hw" "/tmp/apps/$a.log")
            if [ "$n" -ge 3 ]; then pass "$a: hardware decoding ($hw) for all $n clips"
            else fail "$a: hardware decoding ($hw) for $n of 3 clips"; fi
            ;;
        ffvkdec | ffnvdec)
            # ffmpeg decoding each clip on the GPU as fast as it can: Vulkan
            # Video, or NVDEC through CUDA (--allow-compute); the frames come
            # back to system memory, as a player that copies back has them.
            for c in h264 hevc av1 vp9; do
                f=$A/www/$c.mp4
                [ "$c" = vp9 ] && f=$A/www/vp9.webm
                if [ "$a" = ffvkdec ]; then
                    hwa=(-init_hw_device vulkan=vk -hwaccel vulkan -hwaccel_device vk)
                else
                    hwa=(-hwaccel cuda)
                fi
                # H.264 on Vulkan Video stops short of the end in 2 of 5 runs
                # natively too (this ffmpeg and driver; TESTING-RIG.md).
                kn=
                [ "$a" = ffvkdec ] && [ "$c" = h264 ] && kn="H.264 Vulkan decode stalls before the last frame in 2 of 5 runs"
                KNOWN=$kn task "$a-$c" 90 ffmpeg -hide_banner -v verbose "${hwa[@]}" -i "$f" -f null -
                say "$a-$c: $(ff_speed "/tmp/apps/$a-$c.log"), $(tr '\r' '\n' < "/tmp/apps/$a-$c.log" | grep -aoE '^frame= *[0-9]+' | tail -n 1 | tr -d ' ')"
                if grep -aqE 'Failed setup for format|hwaccel initialisation returned error|Falling back|No device available|Device creation failed' "/tmp/apps/$a-$c.log"; then
                    fail "$a-$c: decoded in software, not on the GPU"
                    grep -aE 'Failed setup|hwaccel init|Falling back|No device|Device creation' "/tmp/apps/$a-$c.log" | head -n 3 | sed 's/^/    /'
                fi
            done
            # Frames kept on the GPU (-hwaccel_output_format vulkan): H.264
            # stops after 252 of 300 frames and ignores SIGTERM, natively
            # the same with this ffmpeg and driver (TESTING-RIG.md).
            [ "$a" = ffvkdec ] && KNOWN="H.264 Vulkan decode into GPU frames stalls at frame 252" \
                task ffvkdec-h264-gpu 30 ffmpeg -hide_banner -init_hw_device vulkan=vk -hwaccel vulkan -hwaccel_device vk \
                -hwaccel_output_format vulkan -i "$A/www/h264.mp4" -f null -
            ;;
        ffvkenc | nvenc)
            # Encode 10 s of 1080p60 on the GPU: Vulkan Video (h264_vulkan,
            # hevc_vulkan, av1_vulkan), or NVENC (--allow-compute: ffmpeg
            # drives NVENC through a CUDA context). A valid file, and its fps.
            if [ "$a" = nvenc ]; then encs="h264_nvenc hevc_nvenc av1_nvenc"; else encs="h264_vulkan hevc_vulkan av1_vulkan"; fi
            for e in $encs; do
                out=/tmp/apps/$e.mp4
                if [ "$a" = nvenc ]; then
                    enc=(-f lavfi -i testsrc2=size=1920x1080:rate=60 -t 10 -c:v "$e" -preset p4 -b:v 12M)
                else
                    enc=(-init_hw_device vulkan=vk -filter_hw_device vk -f lavfi -i testsrc2=size=1920x1080:rate=60 -t 10
                         -vf format=nv12,hwupload -c:v "$e")
                fi
                # hevc_vulkan hangs finishing the stream, every run natively.
                kn=
                [ "$e" = hevc_vulkan ] && kn="hevc_vulkan hangs finishing the stream (5 of 5 runs)"
                KNOWN=$kn task "$a-$e" 120 ffmpeg -hide_banner -y "${enc[@]}" "$out" && {
                    fr=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=codec_name,nb_read_frames,width,height -of csv=p=0 "$out" 2>&1)
                    case $fr in
                        *,1920,1080,600 | *,1920,1080,600,*) pass "$a-$e: valid output ($fr) at $(ff_fps "/tmp/apps/$a-$e.log") $(ff_speed "/tmp/apps/$a-$e.log")" ;;
                        *) fail "$a-$e: output not as expected: $fr" ;;
                    esac
                }
            done
            ;;
        vainfo)
            # VA-API on NVDEC (nvidia-vaapi-driver, CUDA): its profiles.
            task vainfo 60 env LIBVA_DRIVER_NAME=nvidia NVD_BACKEND=direct NVD_LOG=1 vainfo --display drm --device /dev/dri/renderD128
            check vainfo "decode profiles" 'VAProfile(H264|HEVC|AV1).*VAEntrypointVLD'
            ;;
        # ---- Compute (--allow-compute) ----------------------------------------
        cuda)
            # A CUDA runtime program (apps/cuda/nvgpu-nbody.cu): a checked
            # vector add, copy bandwidth, and an n-body kernel's GFLOP/s.
            task cuda 120 nvgpu-nbody
            check cuda "device" 'device 0 of [0-9]+: NVIDIA'
            check cuda "n-body" 'nbody .*check PASS'
            check cuda "result" 'RESULT PASS'
            ;;
        opencl)
            task clinfo 60 clinfo
            check clinfo "NVIDIA platform and device" 'Device Name +NVIDIA|Platform Name +NVIDIA CUDA'
            task clpeak 240 clpeak --opencl --fp-compute --bandwidth --max-time 300
            check clpeak "a measurement" 'GFLOPS|GBPS'
            ;;
        cmd)
            # Any command line, base64 in nvgpu_cmd, as a slot (for chasing a
            # failure with the host side watching): its whole log is shown.
            c=$(arg cmd | base64 -d 2>/dev/null)
            SLOTLEN=$(arg cmdslot "$SLOT") OPTS="wait=$(arg cmdwait 20)" slot cmd bash -c "$c"
            cut -c1-300 /tmp/apps/cmd.log | tail -n 300 | sed 's/^/    /'
            ;;
        *) skip "unknown app $a" ;;
    esac
done

wl_daemon_alive && pass "nvgpu-wl-guest still running" || fail "nvgpu-wl-guest died"
cp -r /tmp/apps /var/log/nvgpu/ 2>/dev/null
finish
