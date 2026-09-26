#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run the guest's application pass (guest-image/probes/apps.sh) against a
# host compositor, and do the host's half of it: find each app's window,
# capture it, and (headless sway only) type, click and copy into it.
#
# Usage: scripts/rig-app-check.sh [--live] [app,app,...] [tag]
#   --live  against the live Hyprland session (NVGPU_APPS_LIVE=1 does the
#           same) instead of the rig's headless sway; see "Live" below
#   apps    default: every app apps.sh knows (less, live, the input apps)
#   tag     names the run's logs and captures (default apps-HHMMSS)
#
# The launcher's environment reaches it as is: NVGPU_VMM_KIND=crosvm runs the
# pass under crosvm instead of nesbox, NVGPU_COMPUTE=1 with --allow-compute
# (the compute slots want it). NVGPU_APPS_BACKEND_ARGS is split into words
# and handed to the backend (run-guest.sh's "-- backend-args").
# NVGPU_TIMEOUT, when set, replaces the budget worked out from the slots.
#
# Headless sway (the default). Needs scripts/rig-headless-sway.sh running.
# Everything here talks to that compositor only: the input it injects
# (virtual keyboard and pointer) and the captures (screencopy) go to the
# headless sway, never to a desktop session.
#
# Live (--live). The guest's windows go to the session's own Hyprland
# ($XDG_RUNTIME_DIR/$WAYLAND_DISPLAY; hyprctl must reach it). Nothing is
# typed or clicked there: the input apps (typing, pointer, clipboard) are
# dropped from the list, and the browser slots that navigate by keys take
# their captures without the keys. Windows are found with `hyprctl clients
# -j` (class, title, monitor, geometry) and captured with grim. To keep them
# off the user's work they go to one workspace, silently and without focus:
#   NVGPU_LIVE_WS   the workspace (default: the one DP-3 shows, else 31)
#   NVGPU_LIVE_MON  that workspace's monitor (default DP-3): powered on (DPMS)
#                   only while a slot has a window to show and capture, and
#                   off between slots and at the end (an OLED must not sit on
#                   a static picture); NVGPU_LIVE_DPMS=0 leaves it alone
# by a named window rule on the guest apps' classes, added at the start and
# disabled at the end. A guest window the rule missed (its class is not in
# the list) is moved there after it maps, focus goes back to where it was,
# and the run says so. A guest window is one from the process whose first
# window appeared after an APP_START: all of one VM's clients reach Hyprland
# through its backend, so they share that client pid.
#
# Captures land in .rig/logs/<tag>/<app>.png (the output or monitor) and
# <app>.window.png (the window alone).
set -uo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
LIVE=${NVGPU_APPS_LIVE:-0}
if [ "${1:-}" = --live ]; then LIVE=1; shift; fi
DEFAULT_APPS=typing,pointer,clipboard,glxgears,xterm,gamescope,gtk,qt,firefox,mpv,glmark2,vkmark
APPS=${1:-$DEFAULT_APPS}
TAG=${2:-apps-$(date +%H%M%S)}
SLOT=${NVGPU_SLOT:-25}
OUT=$RIG/logs/$TAG
mkdir -p "$OUT"

if [ "$LIVE" = 1 ]; then
    SOCK=${XDG_RUNTIME_DIR:-}/${WAYLAND_DISPLAY:-}
    [ -S "$SOCK" ] || { echo "--live: no session socket at $SOCK" >&2; exit 1; }
    command -v hyprctl >/dev/null && hyprctl -j version >/dev/null 2>&1 ||
        { echo "--live: hyprctl cannot reach Hyprland (HYPRLAND_INSTANCE_SIGNATURE?)" >&2; exit 1; }
    # No input into the user's session: the input apps are refused here.
    kept=()
    IFS=, read -r -a asked <<<"$APPS"
    for a in "${asked[@]}"; do
        case $a in
            typing | pointer | clipboard) echo "[host] $a: skipped (--live injects no input)" ;;
            *) kept+=("$a") ;;
        esac
    done
    APPS=$(IFS=,; echo "${kept[*]}")
    [ -n "$APPS" ] || { echo "--live: no apps left" >&2; exit 1; }
else
    SOCK=$(cat "$RIG/run/headless-sway.socket" 2>/dev/null) || SOCK=
    [ -S "$SOCK" ] || { echo "no headless sway; start scripts/rig-headless-sway.sh" >&2; exit 1; }
    RT=$(dirname "$SOCK")
    export WAYLAND_DISPLAY=$SOCK XDG_RUNTIME_DIR=$RT
    SWAYSOCK=$(ls "$RT"/sway-ipc.*.sock 2>/dev/null | head -n 1)
    export SWAYSOCK
fi

# The host tools, from nix, once.
if [ -z "${RIG_APP_TOOLS:-}" ]; then
    export NIX_CONFIG="experimental-features = nix-command flakes"
    export RIG_APP_TOOLS=1 NVGPU_APPS_LIVE=$LIVE
    if [ "$LIVE" = 1 ]; then
        exec nix shell nixpkgs#grim nixpkgs#jq -c "$0" "$APPS" "$TAG"
    fi
    exec nix shell nixpkgs#grim nixpkgs#wtype nixpkgs#wl-clipboard \
        nixpkgs#sway nixpkgs#jq -c "$0" "$APPS" "$TAG"
fi

say() { printf '[host] %s\n' "$*"; }

if [ "$LIVE" = 1 ]; then
    # ---- Hyprland ------------------------------------------------------
    hc() { hyprctl -j "$@" 2>>"$OUT/hyprctl.err"; }
    WS=${NVGPU_LIVE_WS:-$(hc monitors | jq -r '.[] | select(.name == "DP-3") | .activeWorkspace.id' 2>/dev/null)}
    WS=${WS:-31}
    # The guest apps' classes (app_id, or WM_CLASS through Xwayland). A class
    # the user's own apps share is not here: the browsers run with classes of
    # their own (apps.sh), and a stray is moved after it maps (below).
    CLASSES='org\.freedesktop\.Xwayland|Xwayland|gamescope|[Ss]uper[Tt]ux[Kk]art|supertuxkart|[Nn]everball'
    CLASSES+='|[Gg]odot.*|org\.godotengine\..*|[Bb]lender|gimp.*|[Gg]imp.*|org\.gimp\..*|org\.inkscape\.Inkscape|[Ii]nkscape'
    CLASSES+='|krita|org\.kde\.krita|libreoffice.*|soffice.*|org\.gnome\.TextEditor|org\.gnome\.Calculator'
    CLASSES+='|io\.github\.Qalculate.*|qalculate.*|org\.qt-project\..*|qml.*|nvgpu-.*|[Ee]lectron|[Ee]lement.*'
    CLASSES+='|mpv|glmark2.*|vkmark|vkcube.*|foot|weston-.*|xterm|XTerm|xeyes|XEyes|eglgears.*|es2gears.*|glxgears'
    CLASSES=${NVGPU_LIVE_CLASSES:-$CLASSES}
    RULE=nvgpu-guest-apps
    lua_rule() { hyprctl eval "$1" >>"$OUT/hyprctl.log" 2>&1; }
    MON=${NVGPU_LIVE_MON:-DP-3}
    dpms() { # dpms on|off: the guest windows' monitor
        [ "${NVGPU_LIVE_DPMS:-1}" = 1 ] || return 0
        lua_rule "hl.dispatch(hl.dsp.dpms({ action = \"$1\", monitor = \"$MON\" }))"
    }
    dpms off
    # (Hyprland 0.56 takes Lua: a long string [[...]] keeps the regex's
    # backslashes as they are.)
    lua_rule "hl.window_rule({ name = \"$RULE\", enabled = true, match = { class = [[^($CLASSES)\$]] }, workspace = \"$WS silent\", no_initial_focus = true })"
    say "live: guest windows go to workspace $WS ($(hc workspaces | jq -r ".[] | select(.id == $WS) | .monitor" 2>/dev/null)), rule $RULE"
    FOCUS0=$(hc activewindow | jq -r '.address // empty')
    MON0=$(hc monitors | jq -r '.[] | select(.focused) | .name')
    live_cleanup() {
        lua_rule "hl.window_rule({ name = \"$RULE\", enabled = false })"
        dpms off
        say "live: rule $RULE disabled, $MON off"
    }
    # The windows there were before the run, and their processes: a guest
    # window is a new window of a process that was not. Windows are told
    # apart by stableId: an address is the window object's, and a new window
    # can be given the address of one that has gone.
    declare -A seen_view old_pid
    while read -r a p; do seen_view[$a]=1; old_pid[$p]=1; done < <(hc clients | jq -r '.[] | "\(.stableId) \(.pid)"')
    GUEST_PID=
    LAST=
    client_json() { hc clients | jq -c --arg a "$1" '.[] | select(.stableId == $a)'; }
    view_geom() { # of the newest guest window: "x,y wxh class title"
        [ -n "$LAST" ] || return
        client_json "$LAST" | jq -r '"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1]) \(.class) \(.title)"'
    }
    describe() {
        local j mon
        j=$(client_json "$1")
        mon=$(hc monitors | jq -r --argjson m "$(jq '.monitor' <<<"$j")" '.[] | select(.id == $m) | .name')
        jq -r --arg mon "$mon" '"class=\(.class) title=\"\(.title)\" monitor=\($mon) workspace=\(.workspace.name) at=\(.at[0]),\(.at[1]) size=\(.size[0])x\(.size[1]) xwayland=\(.xwayland) fullscreen=\(.fullscreen) pid=\(.pid)"' <<<"$j"
    }
    capture() { # capture NAME: the workspace's monitor, and the newest window alone
        local g mon
        mon=$(hc workspaces | jq -r ".[] | select(.id == $WS) | .monitor")
        [ -n "$mon" ] && grim -o "$mon" "$OUT/$1.png" 2>/dev/null && say "$1: captured $OUT/$1.png ($mon)"
        g=$(view_geom | cut -d' ' -f1-2)
        [ -n "$g" ] && grim -g "$g" "$OUT/$1.window.png" 2>/dev/null
    }
    # A new guest window, if one has mapped; keeps it where the rule put it.
    new_guest_view() {
        local a p ws cls addr
        while read -r a addr p ws cls; do
            [ -n "${seen_view[$a]:-}" ] && continue
            if [ -n "$GUEST_PID" ]; then
                [ "$p" = "$GUEST_PID" ] || continue
            else
                [ -n "${old_pid[$p]:-}" ] && continue
                GUEST_PID=$p
                say "live: guest windows come from client pid $p (the backend)"
            fi
            seen_view[$a]=1
            LAST=$a
            if [ "$ws" != "$WS" ]; then
                say "live: WARN $cls mapped on workspace $ws, not $WS (class not in the rule): moving it"
                lua_rule "hl.dispatch(hl.dsp.window.move({ workspace = \"$WS\", follow = false, window = \"address:$addr\" }))"
                [ -n "$FOCUS0" ] && [ "$(hc activewindow | jq -r '.address // empty')" = "$addr" ] &&
                    lua_rule "hl.dispatch(hl.dsp.focus({ window = \"address:$FOCUS0\" }))"
            fi
            return 0
        done < <(hc clients | jq -r '.[] | "\(.stableId) \(.address) \(.pid) \(.workspace.id) \(.class)"')
        return 1
    }
    wait_view() { # wait_view NAME [SECS]: a guest window not shown before
        local secs=${2:-20} i
        for i in $(seq 1 $((secs * 5))); do
            if new_guest_view; then
                say "$1: window $(describe "$LAST")"
                return 0
            fi
            sleep 0.2
        done
        say "$1: no new guest window appeared in Hyprland (${secs}s); clients in $OUT/$1.clients.json"
        hc clients > "$OUT/$1.clients.json"
        return 1
    }
    # More windows of the same slot (a splash, then the main window).
    more_views() {
        local n=0
        while new_guest_view; do n=$((n + 1)); say "$1: another window $(describe "$LAST")"; done
        return 0
    }
    keys() { :; }
    ptr() { :; }
    # The live pass follows the guest instead of leading it: the console's
    # APP_START/APP_END lines say which slot is running, each new guest
    # window belongs to the slot running when it maps, and the captures are
    # timed from that window's first appearance (5 s, then 5 s + settle).
    # A slot that fails fast costs no time here, so the host never falls
    # behind the guest (the headless loop below waits on each slot in turn).
    live_loop() {
        local evs=() i=0 e name opts rc cur= t_win=0 settle=8 capn=0 alive
        while :; do
            alive=0
            kill -0 "$RUN" 2>/dev/null && alive=1
            mapfile -t evs < <(tr -d '\r' < "$CONSOLE" 2>/dev/null | grep -aoE 'APP_(START|END) .*')
            while [ "$i" -lt "${#evs[@]}" ]; do
                e=${evs[$i]}
                i=$((i + 1))
                say "event: $e"
                case $e in
                    APP_START*)
                        read -r _ name opts <<<"$e"
                        case " $opts " in *" nowin "*) host_result[$name]="no window (by design)"; cur=; continue ;; esac
                        cur=$name t_win=0 capn=0 settle=8
                        case " $opts " in *" settle="*) settle=${opts##*settle=}; settle=${settle%% *} ;; esac
                        host_result[$cur]="no window"
                        dpms on
                        ;;
                    APP_END*)
                        read -r _ name rc <<<"$e"
                        if [ "$name" = "$cur" ]; then
                            [ "$t_win" != 0 ] && [ "$capn" = 0 ] && capture "$cur-atexit"
                            dpms off
                            cur=
                        fi
                        ;;
                esac
            done
            while new_guest_view; do
                if [ -z "$cur" ]; then
                    say "a window outside any slot: $(describe "$LAST")"
                elif [ "$t_win" = 0 ]; then
                    t_win=$SECONDS
                    host_result[$cur]=window
                    say "$cur: window $(describe "$LAST")"
                else
                    say "$cur: another window $(describe "$LAST")"
                fi
            done
            if [ -n "$cur" ] && [ "$t_win" != 0 ]; then
                if [ "$capn" = 0 ] && [ $((SECONDS - t_win)) -ge 5 ]; then
                    capture "$cur-early"
                    host_result[$cur]=captured
                    capn=1
                elif [ "$capn" = 1 ] && [ $((SECONDS - t_win)) -ge $((5 + settle)) ]; then
                    capture "$cur"
                    say "$cur: title now $(view_geom | cut -d' ' -f4-)"
                    capn=2
                fi
            fi
            [ "$alive" = 1 ] || break
            sleep 0.5
        done
    }
    slot_begin() { dpms on; }
    slot_end() { dpms off; }
    trap 'live_cleanup' EXIT
    trap 'exit 130' INT TERM
else
    # ---- headless sway -------------------------------------------------
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
    wait_view() { # wait_view NAME [SECS]: a view sway has not shown before
        local id secs=${2:-20}
        for _ in $(seq 1 $((secs * 5))); do
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
    more_views() {
        local id
        for id in $(view_ids); do
            [ -z "${seen_view[$id]:-}" ] && { seen_view[$id]=1; say "$1: another window $(view_geom)"; }
        done
        return 0
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
    slot_begin() { :; }
    slot_end() { :; }
fi

# Run the guest in the background; its console is our cue.
CONSOLE=$RIG/logs/$TAG.console.log
N=$(tr ',' '\n' <<<"$APPS" | wc -l)
say "guest: apps=$APPS slot=${SLOT}s live=$LIVE"
# Split on whitespace only: read -a does not expand a glob in a flag's value.
read -r -a BACKEND_ARGS <<<"${NVGPU_APPS_BACKEND_ARGS:-}"
NVGPU_TIMEOUT=${NVGPU_TIMEOUT:-$((N * (SLOT + 10) + 90))} \
    NVGPU_CMDLINE_EXTRA="nvgpu_apps=$APPS nvgpu_slot=$SLOT nvgpu_live=$LIVE ${NVGPU_APPS_EXTRA:-}" \
    "$REPO/scripts/run-guest.sh" --wayland-socket "$SOCK" apps "$TAG" \
    -- ${BACKEND_ARGS[@]+"${BACKEND_ARGS[@]}"} > "$OUT/run.log" 2>&1 &
RUN=$!

declare -A host_result
if [ "$LIVE" = 1 ]; then
    live_loop
else
seen_end=0
while kill -0 "$RUN" 2>/dev/null; do
    # Anywhere in a line: the guest's console interleaves its own output
    # with what the probe writes to /dev/console directly.
    line=$(tr -d '\r' < "$CONSOLE" 2>/dev/null | grep -ao 'APP_START .*' | sed -n "$((seen_end + 1))p")
    if [ -z "$line" ]; then sleep 0.3; continue; fi
    seen_end=$((seen_end + 1))
    # APP_START <name> [nowin] [wait=S]
    read -r _ app opts <<<"$line"
    say "event $seen_end: $line"
    wait_s=20
    case " $opts " in *" wait="*) wait_s=${opts##*wait=}; wait_s=${wait_s%% *} ;; esac
    case " $opts " in
        *" nowin "*)
            host_result[$app]="no window (by design)"
            continue
            ;;
    esac
    slot_begin
    case $app in
        clipboard) wl-copy clip-from-host >/dev/null 2>&1 </dev/null && say "clipboard: host selection set" ;;
    esac
    if ! wait_view "$app" "$wait_s"; then
        host_result[$app]="no window"
        slot_end
        continue
    fi
    sleep 3
    more_views "$app"
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
                [ "$i" = 2 ] && [ "$LIVE" != 1 ] && keys -M ctrl l -m ctrl -s 300 "abc" && say "$app: typed into the address bar"
                sleep 5
            done
            ;;
        ffsupport | chromegpu)
            # Chromium ignores a chrome:// URL on its command line: type it
            # into the address bar, which a new window focuses (not live).
            [ "$app" = chromegpu ] && [ "$LIVE" != 1 ] && sleep 4 && keys "chrome://gpu" -k Return
            # Long pages: let them fill in, then a capture per screenful.
            sleep 20
            for i in 1 2 3 4 5; do
                capture "$app-$i"
                [ "$LIVE" = 1 ] && break
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
        *)
            # Slow starters: a second look after they have settled.
            case " $opts " in *" settle="*)
                s=${opts##*settle=}; s=${s%% *}
                sleep "$s"; more_views "$app"; capture "$app-early" ;;
            esac
            ;;
    esac
    sleep 2
    capture "$app"
    say "$app: title now $(view_geom | cut -d' ' -f4-)"
    slot_end
    host_result[$app]=${host_result[$app]:-captured}
done
fi
wait "$RUN"
rc=$?
[ "$LIVE" = 1 ] || pkill -u "$(id -u)" -f "^$(command -v wl-copy)" 2>/dev/null

say "guest verdicts:"
tr -d '\r' < "$CONSOLE" | grep -aE '\[apps\] (PASS|FAIL|SKIP)|NVGPU_PROBE_DONE' | sed 's/^/    /'
say "host side:"
for a in "${!host_result[@]}"; do printf '    %-12s %s\n' "$a" "${host_result[$a]}"; done
say "captures: $OUT"
exit "$rc"
