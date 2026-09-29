# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# What only an unprivileged run of rig/run-guest.sh does. Sourced by the
# launcher, which calls each of these only when it is not root.
#
# Run as an ordinary user, one VM's backend and VMM are that user: nobody
# else is available without privilege, and it is who owns the compositor
# socket and the export directory anyway. setpriv still gives the backend
# no_new_privs and empty inheritable and ambient capability sets. It cannot
# change the uid, the groups or the bounding set -- all three need privilege
# -- so the backend keeps the user's own supplementary groups: a group that is
# root in all but name (docker, libvirt, disk) is then within reach of
# anything that takes the backend over, and rig-preflight.sh warns about
# those. The backend's sandbox still applies: it enters a user and network
# namespace of its own (where unprivileged user namespaces are allowed), and
# Landlock and seccomp confine it. nesbox leaves the host's network the same
# way ("unshare-network"). What sharing the uid loses, against a slot of its
# own: the VMM (seccomp, but no Landlock) can open whatever the user can, and
# can signal the user's processes and, on a host with kernel.yama.ptrace_scope
# 0, trace them; RM's security token -- the uid -- is the user's own, the same
# as every GPU program on the desktop; and two VMs started by one user are one
# principal to RM and to the kernel. Keep a rig for one VM at a time.

# Who the backend runs as: without privilege there is exactly one user
# to be. Saying so beats silently ignoring a NVGPU_USER that asks for
# another. (A uid with no passwd entry, as in a sandbox, is still a user.)
user_backend_user() {
    BACKEND_UID=$(id -u)
    ME=$(id -un 2>/dev/null) || ME=$BACKEND_UID
    if [ -n "${NVGPU_USER:-}" ] && [ "$NVGPU_USER" != "$ME" ] && [ "$NVGPU_USER" != "$BACKEND_UID" ]; then
        die "NVGPU_USER=$NVGPU_USER needs root; unprivileged, the backend runs as $ME"
    fi
    NVGPU_USER=$ME
    # What an unprivileged setpriv can still do (util-linux 2.42 checked):
    # set no_new_privs, and empty the inheritable and ambient sets.
    # --reuid, --groups/--clear-groups and --bounding-set all fail with EPERM.
    AS_BACKEND=(setpriv --no-new-privs --inh-caps=-all --ambient-caps=-all)
}

# A stale backend or VMM of the rig's own holds the GPU and confuses the
# logs. The rig is one VM at a time (this file's header), and a stale one is
# killed: processes of this user started from exactly this launcher's
# backend binary and socket directory, and VMMs started from this VMM binary
# with a config in this logs directory. The patterns are anchored at the
# start of the command line, so an editor or a grep with the path in its
# arguments is not one of them.
kill_stale() {
    pkill -u "$(id -u)" -f "^$(re "$BACKEND_BIN") --socket $(re "$RUN_PARENT")/nvgpu-run\." || true
    if [ "$VMM_KIND" = crosvm ]; then
        pkill -u "$(id -u)" -f "^$(re "$VMM") run .*--vhost-user type=nvgpu,socket=$(re "$RUN_PARENT")/nvgpu-run\." || true
    else
        pkill -u "$(id -u)" -f "^$(re "$VMM") $(re "$LOGS")/[^/ ]+\.json" || true
    fi
}

# The run's directory: our own runtime directory is already 0700 and ours;
# without one, the rig's run/ is made so. mktemp -d makes the run's own
# directory 0700.
user_run_dir() {
    if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
        (umask 077 && mkdir -p "$RUN_PARENT")
        chmod 0700 "$RUN_PARENT"
    fi
    RUN=$(mktemp -d "$RUN_PARENT/nvgpu-run.XXXXXX")
    chmod 0700 "$RUN"
    BACKEND_EXE=$BACKEND_BIN
}
