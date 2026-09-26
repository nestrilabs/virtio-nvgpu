#!/bin/sh
# Stand-in for vhost-user-nvgpu: binds the socket it is given and waits, or,
# with DRY_ATTACK=1, puts a symlink to a root-owned socket there instead -- what
# a compromised backend user could do in a directory it owns.
sock=
while [ $# -gt 0 ]; do
    case $1 in --socket) sock=$2; shift 2 ;; *) shift ;; esac
done
if [ "${DRY_ATTACK:-0}" = 1 ]; then
    ln -s /run/rootd.sock "$sock"
    exec sleep 30
fi
exec python3 -c 'import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); time.sleep(30)' "$sock"
