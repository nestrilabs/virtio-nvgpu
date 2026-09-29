#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Stand-in for vhost-user-nvgpu.
#
# Given --socket (unprivileged, and the old launcher as root), it binds the
# socket and waits, or, with DRY_ATTACK=1, puts a symlink to a root-owned
# socket there instead -- what a compromised backend user could do in a
# directory it owns.
#
# Handed a socket (LISTEN_FDS: as root, the launcher binds it with
# systemd-socket-activate, which execs this once the VMM connects), it says
# what it was handed and where it runs from, and serves the one connection.
sock=
for a; do
    case $a in --socket) sock=next ;; *) [ "$sock" = next ] && sock=$a ;; esac
done
if [ -n "${LISTEN_FDS:-}" ]; then
    echo "stub backend: LISTEN_FDS=$LISTEN_FDS LISTEN_FDNAMES=${LISTEN_FDNAMES:-} LISTEN_PID is mine: $([ "${LISTEN_PID:-}" = $$ ] && echo yes || echo no)"
    echo "stub backend: --socket given: ${sock:-no}; runs from a directory of uid:mode $(stat -c '%u:%a' "$(dirname "$0")")"
    exec python3 -c 'import socket, time
s = socket.socket(fileno=3)
print("stub backend: descriptor 3:", s.family.name, s.type.name, "listening at", s.getsockname(), flush=True)
c, _ = s.accept()
print("stub backend: accepted the VMM", flush=True)
time.sleep(2)'
fi
if [ "${DRY_ATTACK:-0}" = 1 ]; then
    ln -s /run/rootd.sock "$sock"
    exec sleep 30
fi
exec python3 -c 'import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); time.sleep(30)' "$sock"
