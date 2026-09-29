# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's backend: the compositor it will connect to, the
# backend started, and its socket checked before the VMM is given it.
# Sourced by the launcher.

# The compositor's socket has to be one the backend's user can connect to;
# better to say so here than as a failed CONNECT from inside the guest.
check_wayland() {
    if [ -n "$WL_SOCK" ] && [ ${#AS_BACKEND[@]} -gt 0 ] &&
        ! "${AS_BACKEND[@]}" test -S "$WL_SOCK" -a -w "$WL_SOCK"; then
        echo "$NVGPU_USER cannot connect to $WL_SOCK; run as its owner" \
            "(NVGPU_USER=$(stat -c %U "$WL_SOCK" 2>/dev/null || echo '?'))" >&2
        exit 1
    fi
    # The live desktop's own compositor is a legitimate target (the lease stages
    # need it), but not an accidental one: everything the proxy lets through, it
    # lets through to the session the user is sitting in.
    if [ -n "$WL_SOCK" ] && [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
        live=$WAYLAND_DISPLAY
        case $live in /*) ;; *) live=$XDG_RUNTIME_DIR/$live ;; esac
        if [ "$(realpath -m -- "$WL_SOCK")" = "$(realpath -m -- "$live")" ]; then
            echo "WARNING: $WL_SOCK is this session's own compositor; the guest's clients" \
                "appear on the live desktop (rig/TESTING-RIG.md says which stages want that)" >&2
        fi
    fi
    if [ $PRIV = user ] && [ -n "$WL_EXPORT" ]; then
        [ -d "$(dirname -- "$WL_EXPORT")" ] && [ -w "$(dirname -- "$WL_EXPORT")" ] ||
            die "--wayland-export $WL_EXPORT: its directory must exist and be $NVGPU_USER's"
    fi
}

# The backend, started in the background ($BACKEND). Its log file is opened
# here, by whoever runs this; the backend meters every log call site
# (device/src/ratelimit.rs), so a guest cannot grow it at will.
start_backend() {
    # The backend refuses its diagnostic flags without --diagnostic
    # (vhost-user-nvgpu --diagnostic --help). One here was asked for -- after
    # --, or by NVGPU_SANDBOX=off, NVGPU_ALLOW_ROOT_UNSAFE=1 or --inject, and
    # as root with NVGPU_DIAGNOSTIC=1 -- so say so, each on the terminal: the
    # backend's own DIAGNOSTIC lines go to its log.
    DIAG_LINES=$(diag_flags)
    if [ -n "$DIAG_LINES" ]; then
        while IFS= read -r l; do
            echo "run-guest: WARNING: diagnostic flag $l; for diagnosis only" >&2
        done <<<"$DIAG_LINES"
        case " ${BACKEND_ARGS[*]} " in *" --diagnostic "*) ;; *) BACKEND_ARGS+=(--diagnostic) ;; esac
    fi
    echo "backend: as $NVGPU_USER ($PRIV, $LAYOUT layout)${BACKEND_ARGS[*]:+, with ${BACKEND_ARGS[*]}}" >&2
    BACKEND_AFFINITY=()
    [ -z "$BACKEND_CPUS" ] || BACKEND_AFFINITY=(taskset -c "$BACKEND_CPUS")
    # Who binds the socket. Unprivileged, the backend, in the run's
    # directory, which is this user's anyway. As root, root:
    # systemd-socket-activate binds it in root's $RUN and, once the VMM
    # connects, execs the rest of the line -- the same process, so still
    # $BACKEND -- with the socket as descriptor 3 (LISTEN_FDS, as systemd's
    # socket activation passes it). It passes on only the variables it is
    # told to.
    if [ $PRIV = root ]; then
        BIND=("$SOCKET_ACTIVATE" --fdname=vhost-user -l "$SOCK"
            -E RUST_LOG -E NVGPU_DIAGNOSTIC -E NVGPU_PACING_STATS --)
        SOCKET_ARGS=()
        # What the process's command line names from start to end: the run's
        # own copy of the binary (the socket is not on the backend's).
        BACKEND_MARK=$BACKEND_EXE
    else
        BIND=()
        SOCKET_ARGS=(--socket "$SOCK")
        BACKEND_MARK=$SOCK
    fi
    : > "$BLOG"
    exec {BLOG_W}> >(exec {SLOT_FD}>&- {TAG_FD}>&-; capped "$BLOG")
    BLOG_WRITER=$!
    RUST_LOG=${RUST_LOG:-info} "${SLICE[@]}" "${BACKEND_AFFINITY[@]}" "${BACKEND_NETNS[@]}" ${BIND[@]+"${BIND[@]}"} \
        "${AS_BACKEND[@]}" "$BACKEND_EXE" ${SOCKET_ARGS[@]+"${SOCKET_ARGS[@]}"} "${BACKEND_ARGS[@]}" \
        {SLOT_FD}>&- {TAG_FD}>&- >&"$BLOG_W" 2>&1 {BLOG_W}>&- &
    BACKEND=$!
    exec {BLOG_W}>&-
}

# The socket must exist before the VMM connects, and be what it should: the
# backend's (unprivileged) or root's (as root), a socket, and not a link to
# one -- anything else at that path is not ours to connect to (or, as root,
# to open to a group). As root nobody else can change $RUN; unprivileged,
# nobody else can enter it.
await_socket() {
    for _ in $(seq 1 50); do
        [ -S "$SOCK" ] && break
        kill -0 "$BACKEND" 2>/dev/null || break
        sleep 0.1
    done
    if ! kill -0 "$BACKEND" 2>/dev/null; then
        wait "$BLOG_WRITER" 2>/dev/null || true
        echo "backend exited; see $BLOG" >&2
        tail -n 5 "$BLOG" | clean >&2
        exit 1
    fi
    if [ $PRIV = root ]; then SOCK_UID=0 SOCK_OWNER=root; else SOCK_UID=$BACKEND_UID SOCK_OWNER=$NVGPU_USER; fi
    [ ! -L "$SOCK" ] || { echo "$SOCK is a symbolic link, not the backend's socket; refusing it" >&2; exit 1; }
    [ -S "$SOCK" ] || { echo "no socket at $SOCK; see $BLOG" >&2; exit 1; }
    [ "$(stat -c %u -- "$SOCK")" = "$SOCK_UID" ] || {
        echo "$SOCK is not owned by $SOCK_OWNER; refusing to connect" >&2
        exit 1
    }
}
