# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
# A guest compositor on a guest card node, for compositor.sh and export.sh:
# sway (default) or Hyprland (nvgpu_comp=hyprland, what TESTING.md stage 6
# names). Source it after probe-common.sh.
#
#   start_comp CARD   start it; wait for its sockets. Sets COMP (name),
#                     COMP_PID, COMP_WL (its WAYLAND_DISPLAY), COMP_T0.
#   comp_outputs      print its outputs; 0 if one is enabled
#   comp_windows      how many client windows it has
#   stop_comp         PASS if it is still alive, then stop it

COMP=$(arg comp sway)
COMP_LOG=/var/log/nvgpu/$COMP.log

# The compositor's socket is the wayland-N that was not there before it.
_comp_wait_socket() {
    local s pre=" $* "
    COMP_WL=
    for _ in $(seq 1 150); do
        kill -0 "$COMP_PID" 2>/dev/null || return 1
        for s in "$XDG_RUNTIME_DIR"/wayland-*; do
            case $s in *.lock) continue ;; esac
            [ -S "$s" ] || continue
            case $pre in *" $(basename "$s") "*) continue ;; esac
            COMP_WL=$(basename "$s")
        done
        if [ -n "$COMP_WL" ]; then
            case $COMP in
                sway) SWAY_IPC=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.*.sock 2>/dev/null | head -n 1); [ -n "$SWAY_IPC" ] && return 0 ;;
                hyprland) HYPR_SIG=$(ls "$XDG_RUNTIME_DIR/hypr" 2>/dev/null | head -n 1); [ -n "$HYPR_SIG" ] && return 0 ;;
            esac
        fi
        sleep 0.1
    done
    return 1
}

start_comp() {
    local card=$1 pre
    pre=$(ls "$XDG_RUNTIME_DIR" 2>/dev/null | tr '\n' ' ')
    COMP_T0=$SECONDS
    case $COMP in
        sway)
            cat > /tmp/sway.cfg <<CFG
output * bg #1d3557 solid_color
default_border normal
font DejaVu Sans Mono 12
CFG
            say "---- sway on $card (WLR_RENDERER=$(arg wlr_renderer vulkan), log $COMP_LOG)"
            env WLR_RENDERER="$(arg wlr_renderer vulkan)" \
                WLR_BACKENDS=drm \
                WLR_DRM_DEVICES="$card" \
                WLR_NO_HARDWARE_CURSORS=1 \
                LIBSEAT_BACKEND=noop \
                XDG_SESSION_TYPE=wayland \
                sway --unsupported-gpu -d -c /tmp/sway.cfg > "$COMP_LOG" 2>&1 &
            ;;
        hyprland)
            say "---- Hyprland on $card (default config; log $COMP_LOG)"
            # Everything in this guest runs as root; Hyprland refuses root
            # without the flag.
            env AQ_DRM_DEVICES="$card" \
                LIBSEAT_BACKEND=noop \
                XDG_SESSION_TYPE=wayland \
                Hyprland --i-am-really-stupid > "$COMP_LOG" 2>&1 &
            ;;
        *)
            fail "unknown nvgpu_comp=$COMP (sway or hyprland)"
            return 1
            ;;
    esac
    COMP_PID=$!
    BG_PIDS+=("$COMP_PID")
    _comp_wait_socket $pre
    sleep 3
    if ! kill -0 "$COMP_PID" 2>/dev/null; then
        fail "$COMP exited during startup"
        grep -Ei 'error|fail|unable|cannot' "$COMP_LOG" | tail -n 30 | sed 's/^/    /'
        tail -n 15 "$COMP_LOG" | sed 's/^/    /'
        return 1
    fi
    if [ -z "$COMP_WL" ]; then
        fail "$COMP running but no Wayland/IPC socket in $XDG_RUNTIME_DIR after 15 s"
        return 1
    fi
    pass "$COMP running on $card (display $COMP_WL)"
    grep -Ei 'Using (Vulkan|GLES2)|renderer|DRM device|Found DRM|nvidia|backend' "$COMP_LOG" | head -n 12 | sed "s/^/    [$COMP] /"
    if comp_outputs; then
        pass "$COMP enabled an output"
    else
        fail "$COMP reports no enabled output"
    fi
}

comp_outputs() {
    local out
    case $COMP in
        sway)
            # swaymsg prints JSON when stdout is not a terminal.
            out=$(swaymsg -s "$SWAY_IPC" -t get_outputs 2>&1)
            printf '%s\n' "$out" | jq -r '.[] | "    \(.name) \(.make) \(.model) active=\(.active) \(.current_mode.width)x\(.current_mode.height)@\(.current_mode.refresh / 1000)Hz"' 2>/dev/null ||
                printf '%s\n' "$out" | sed 's/^/    /'
            [ "$(printf '%s\n' "$out" | jq '[.[] | select(.active)] | length' 2>/dev/null)" -gt 0 ] 2>/dev/null
            ;;
        hyprland)
            out=$(HYPRLAND_INSTANCE_SIGNATURE=$HYPR_SIG hyprctl monitors all 2>&1)
            printf '%s\n' "$out" | sed 's/^/    /'
            printf '%s\n' "$out" | grep -q '^Monitor ' && ! printf '%s\n' "$out" | grep -q 'disabled: true'
            ;;
    esac
}

comp_windows() {
    case $COMP in
        sway) swaymsg -s "$SWAY_IPC" -t get_tree 2>/dev/null | grep -cE '"(app_id|class)": "[^"]' ;;
        hyprland) HYPRLAND_INSTANCE_SIGNATURE=$HYPR_SIG hyprctl clients -j 2>/dev/null | jq length 2>/dev/null || echo 0 ;;
    esac
}

comp_windows_detail() {
    case $COMP in
        sway) swaymsg -s "$SWAY_IPC" -t get_tree | grep -E '"(app_id|name|class)"' | head -n 20 ;;
        hyprland) HYPRLAND_INSTANCE_SIGNATURE=$HYPR_SIG hyprctl clients | head -n 30 ;;
    esac
}

stop_comp() {
    if kill -0 "$COMP_PID" 2>/dev/null; then
        pass "$COMP still running after $((SECONDS - COMP_T0))s"
        kill -TERM "$COMP_PID"
        for _ in $(seq 1 50); do kill -0 "$COMP_PID" 2>/dev/null || break; sleep 0.1; done
        kill -KILL "$COMP_PID" 2>/dev/null
    else
        fail "$COMP died before the end of its run"
        tail -n 30 "$COMP_LOG" | sed 's/^/    /'
    fi
    say "$COMP log: $COMP_LOG ($(wc -l < "$COMP_LOG") lines)"
    grep -E '\[ERROR\]|\[ERR\]|CRIT' "$COMP_LOG" | tail -n 10 | sed 's/^/    /'
}
