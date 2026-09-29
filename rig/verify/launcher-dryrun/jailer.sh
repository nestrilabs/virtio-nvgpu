#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Stand-in for nesbox's jailer, as the root launcher runs it (the VMM of a
# root run): says what it was given and what environment reached it,
# connects to the backend's socket the config names, as nesbox would, and
# for some tags plays a guest: "flood" writes 3 MB to the console, "esc"
# prints a verdict line with a terminal escape sequence in it.
echo "jailer stub: $*"
cfg=
while [ $# -gt 0 ]; do
    case $1 in --config) cfg=$2; shift 2 ;; *) shift ;; esac
done
if env | grep -q '^FOO_EVIL='; then
    echo "jailer stub: FOO_EVIL reached the VMM; PATH=$PATH"
else
    echo "jailer stub: environment clean; PATH=$PATH"
fi
case $cfg in *flood*) yes xxxxxxxxxxxxxxx | head -c 3000000 ;; esac
case $cfg in *esc*) printf 'NVGPU_PROBE_DONE probe=esc \033]0;pwned\007 result=PASS pass=1 fail=0 skip=0\n' ;; esac
sock=$(sed -n 's/.*"gpu-forward": { "socket": "\([^"]*\)".*/\1/p' "$cfg")
if [ -n "$sock" ]; then
    python3 -c 'import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); time.sleep(1)
print("jailer stub: connected to", sys.argv[1], flush=True)' "$sock"
fi
exit 0
