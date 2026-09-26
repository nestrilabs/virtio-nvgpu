#!/usr/bin/env bash
# Run the guest's application pass (guest-image/probes/apps.sh) against the
# rig's headless sway, and do the host's half of it: find each app's window
# in sway, capture it, and type, click and copy into it.
#
# Usage: scripts/rig-app-check.sh [app,app,...] [tag]
#   apps  default: every app apps.sh knows
#   tag   names the run's logs and captures (default apps-HHMMSS)
#
# The launcher's environment reaches it as is: NVGPU_VMM_KIND=crosvm runs the
# pass under crosvm instead of nesbox. NVGPU_APPS_BACKEND_ARGS is split into
# words and handed to the backend (run-guest.sh's "-- backend-args").
#
# Needs scripts/rig-headless-sway.sh running. Everything here talks to that
# compositor only: the input it injects (virtual keyboard and pointer) and the
# captures (screencopy) go to the headless sway, never to a desktop session.
# Captures land in .rig/logs/<tag>/<app>.png.
set -uo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
APPS=${1:-typing,pointer,clipboard,glxgears,xterm,gamescope,gtk,qt,firefox,mpv,glmark2,vkmark}
TAG=${2:-apps-$(date +%H%M%S)}
SLOT=${NVGPU_SLOT:-25}
OUT=$RIG/logs/$TAG
mkdir -p "$OUT"

SOCK=$(cat "$RIG/run/headless-sway.socket" 2>/dev/null) || SOCK=
[ -S "$SOCK" ] || { echo "no headless sway; start scripts/rig-headless-sway.sh" >&2; exit 1; }
RT=$(dirname "$SOCK")
export WAYLAND_DISPLAY=$SOCK XDG_RUNTIME_DIR=$RT
SWAYSOCK=$(ls "$RT"/sway-ipc.*.sock 2>/dev/null | head -n 1)
export SWAYSOCK

# The host tools, from nix, once.
if [ -z "${RIG_APP_TOOLS:-}" ]; then
    export NIX_CONFIG="experimental-features = nix-command flakes"
    export RIG_APP_TOOLS=1
    exec nix shell nixpkgs#grim nixpkgs#wtype nixpkgs#wl-clipboard \
        nixpkgs#sway nixpkgs#jq -c "$0" "$APPS" "$TAG"
fi

say() { printf '[host] %s\n' "$*"; }

# The newest view's geometry, as grim wants it, once it has one.
views() {
    local n
    n=$(swaymsg -t get_tree 2>>"$OUT/swaymsg.err" |
        jq '[recurse(.nodes[]?, .floating_nodes[]?) | select(.pid? != null)] | length' 2>>"$OUT/swaymsg.err")
    echo "${n:-0}"
}
view_geom() {
    swaymsg -t get_tree 2>/dev/null |
        jq -r '[recurse(.nodes[]?, .floating_nodes[]?) | select(.pid? != null)] | last |
               select(.) | "\(.rect.x),\(.rect.y) \(.rect.width)x\(.rect.height) \(.app_id // .window_properties.class // "?") \(.name // "")"'
}

capture() { # capture NAME: the whole output, and the newest window alone
    grim "$OUT/$1.png" 2>/dev/null && say "$1: captured $OUT/$1.png"
    local g
    g=$(view_geom | cut -d' ' -f1-2)
    [ -n "$g" ] && grim -g "$g" "$OUT/$1.window.png" 2>/dev/null
}

# The ids of every view sway has now.
view_ids() {
    swaymsg -t get_tree 2>>"$OUT/swaymsg.err" |
        jq -r '[recurse(.nodes[]?, .floating_nodes[]?) | select(.pid? != null) | .id] | .[]' 2>>"$OUT/swaymsg.err"
}

declare -A seen_view
wait_view() { # wait_view NAME: up to 20 s for a view sway has not shown before
    local id
    for _ in $(seq 1 100); do
        for id in $(view_ids); do
            if [ -z "${seen_view[$id]:-}" ]; then
                seen_view[$id]=1
                say "$1: window $(view_geom)"
                return 0
            fi
        done
        sleep 0.2
    done
    say "$1: no new window appeared in sway"
    return 1
}

# Keyboards. A headless sway has no input devices, and a seat with no
# keyboard gives no client keyboard focus -- so no selection, and no
# clipboard, reaches anyone. wtype's keyboard exists while it runs, so each
# one here types and then stays: when the keyboard that last typed goes away,
# sway's seat has no active keyboard, and a client that asks for one then gets
# enter with no keymap (natively too; wev crashes on it).
KBS=()
keys() { wtype -s 400 "$@" -s 86400000 >/dev/null 2>&1 </dev/null & KBS+=($!); }
keys -s 1
# And a pointer, the same way, driven through a fifo (scripts/rig-tools/vptr.c;
# scripts/rig-tools/build.sh builds it). wlrctl's pointers live for one
# command, which no client ever sees move.
[ -x "$RIG/bin/vptr" ] || "$REPO/scripts/rig-tools/build.sh" "$RIG/bin" >/dev/null
PTR=$OUT/pointer.fifo
rm -f "$PTR" && mkfifo "$PTR"
"$RIG/bin/vptr" < "$PTR" 2>>"$OUT/vptr.log" &
VPTR=$!
exec 7>"$PTR"
ptr() { printf '%s\n' "$@" >&7; }
trap 'kill "${KBS[@]}" $VPTR 2>/dev/null' EXIT

# Run the guest in the background; its console is our cue.
CONSOLE=$RIG/logs/$TAG.console.log
N=$(tr ',' '\n' <<<"$APPS" | wc -l)
say "guest: apps=$APPS slot=${SLOT}s"
NVGPU_TIMEOUT=$((N * (SLOT + 10) + 90)) \
    NVGPU_CMDLINE_EXTRA="nvgpu_apps=$APPS nvgpu_slot=$SLOT ${NVGPU_APPS_EXTRA:-}" \
    "$REPO/scripts/run-guest.sh" --wayland-socket "$SOCK" apps "$TAG" \
    -- ${NVGPU_APPS_BACKEND_ARGS:-} > "$OUT/run.log" 2>&1 &
RUN=$!

declare -A host_result
seen_end=0
while kill -0 "$RUN" 2>/dev/null; do
    line=$(tr -d '\r' < "$CONSOLE" 2>/dev/null | grep -a '^APP_START ' | sed -n "$((seen_end + 1))p")
    if [ -z "$line" ]; then sleep 0.3; continue; fi
    seen_end=$((seen_end + 1))
    app=${line#APP_START }
    case $app in
        clipboard) wl-copy clip-from-host >/dev/null 2>&1 </dev/null && say "clipboard: host selection set" ;;
    esac
    if ! wait_view "$app"; then
        host_result[$app]="no window"
        continue
    fi
    sleep 3
    case $app in
        typing)
            # One wtype, after a pause: each wtype is a new virtual keyboard,
            # and a key sent before the client has taken its keymap is lost
            # (natively too, with no proxy in between).
            keys "hello from the host" -k Return && say "typing: typed a line"
            ;;
        pointer)
            ptr 'abs 900 500' 'sleep 300' 'move 40 25' 'click' 'wheel 2'
            keys k
            say "pointer: moved, clicked, scrolled, and pressed k"
            ;;
        chromentp)
            for i in 1 2 3 4; do capture "$app-$i"; sleep 6; done
            ;;
        chromeanim)
            for i in 1 2 3 4 5 6; do
                capture "$app-$i"
                say "$app: title $(view_geom | cut -d' ' -f4-)"
                # Between the second and third captures, type into the
                # address bar (focus it with ctrl+l first).
                [ "$i" = 2 ] && keys -M ctrl l -m ctrl -s 300 "abc" && say "$app: typed into the address bar"
                sleep 5
            done
            ;;
        ffsupport | chromegpu)
            # Chromium ignores a chrome:// URL on its command line: type it
            # into the address bar, which a new window focuses.
            [ "$app" = chromegpu ] && sleep 4 && keys "chrome://gpu" -k Return
            # Long pages: let them fill in, then a capture per screenful.
            sleep 20
            for i in 1 2 3 4 5; do
                capture "$app-$i"
                keys -k Next
                sleep 1.5
            done
            ;;
        clipboard)
            sleep 4
            got=$(timeout 5 wl-paste -n 2>/dev/null)
            say "clipboard guest->host: \"$got\""
            host_result[clipboard]=$([ "$got" = clip-from-guest ] && echo PASS || echo "FAIL ($got)")
            ;;
    esac
    sleep 2
    capture "$app"
    host_result[$app]=${host_result[$app]:-captured}
done
wait "$RUN"
rc=$?
pkill -u "$(id -u)" -f "^$(command -v wl-copy)" 2>/dev/null

say "guest verdicts:"
tr -d '\r' < "$CONSOLE" | grep -aE '\[apps\] (PASS|FAIL|SKIP)|NVGPU_PROBE_DONE' | sed 's/^/    /'
say "host side:"
for a in "${!host_result[@]}"; do printf '    %-10s %s\n' "$a" "${host_result[$a]}"; done
say "captures: $OUT"
exit "$rc"
