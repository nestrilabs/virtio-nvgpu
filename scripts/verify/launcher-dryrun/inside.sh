#!/bin/bash
# Runs as root of a user namespace, in its own mount and pid namespaces, chrooted
# into a tmpfs root that holds a root-owned rig: two launchers (the old, e513ba9,
# and the new), stub binaries, and a fake pool of slot users. The namespace maps
# one uid, so every user is uid 0 here: the backend is "root" and needs
# NVGPU_ALLOW_ROOT_UNSAFE (and, new, NVGPU_DIAGNOSTIC); what is checked --
# which processes are killed, what the socket's permissions go to -- does not
# depend on the uids.
set -u
export PATH=/stubs:/run/current-system/sw/bin
cd /
run() { # run <launcher> <tag> [env...]
    local l=$1 tag=$2; shift 2
    env -i PATH=$PATH HOME=/root NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
        NVGPU_OOM_SCORE_ADJ= NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 "$@" \
        bash "/rig/$l" probe "$tag" > "/rig/logs/$tag.launcher" 2>&1
    echo "  exit $?; launcher said:"
    sed 's/^/    | /' "/rig/logs/$tag.launcher" | grep -v "^    | *$" | head -12
}
others() { # start another VM's backend and VMM, as a stale-process pattern sees them
    mkdir -p /run/nvgpu.other
    cp /rig/bin/nesbox /run/nvgpu.other/vhost-user-nvgpu
    /run/nvgpu.other/vhost-user-nvgpu --socket /run/nvgpu.other/nvgpu.sock &
    OB=$!
    /rig/bin/nesbox /rig/logs/other.json &
    OV=$!
    sleep 0.3
}
alive() { kill -0 "$1" 2>/dev/null && echo alive || echo KILLED; }
rootd() { # a root daemon's socket, 0600
    rm -f /run/rootd.sock
    python3 -c 'import socket; socket.socket(socket.AF_UNIX).bind("/run/rootd.sock")'
    chmod 0600 /run/rootd.sock
}

for l in run-guest.old.sh run-guest.new.sh; do
    echo "== $l: another VM's backend and VMM are running"
    others
    run "$l" "pk-${l%%.sh}"
    echo "  the other VM's backend: $(alive $OB); its VMM: $(alive $OV)"
    kill "$OB" "$OV" 2>/dev/null; wait "$OB" "$OV" 2>/dev/null
    rm -rf /run/nvgpu.other

    echo "== $l: the backend replaces its socket with a symlink to a root daemon's socket"
    rootd
    echo "  /run/rootd.sock before: $(stat -c '%a %G' /run/rootd.sock)"
    run "$l" "sym-${l%%.sh}" DRY_ATTACK=1
    echo "  /run/rootd.sock after:  $(stat -c '%a %G' /run/rootd.sock)"
    echo
done

echo "== new: a benign run gets as far as the VMM"
run run-guest.new.sh benign
echo "== new: as root without NVGPU_RIG or NVGPU_PREFIX"
env -i PATH=$PATH bash /rig/run-guest.new.sh probe nolayout 2>&1 | sed 's/^/    | /' | head -3
echo "== new: NVGPU_SANDBOX=off as root without NVGPU_DIAGNOSTIC"
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_SANDBOX=off bash /rig/run-guest.new.sh probe nodiag 2>&1 | sed 's/^/    | /' | head -3
echo "== new: a rig with a directory someone else can write"
chmod 0777 /rig/kernel
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 \
    bash /rig/run-guest.new.sh probe writable 2>&1 | sed 's/^/    | /' | grep -v WARNING | head -3
chmod 0755 /rig/kernel
echo "== new: a launcher in a directory another user can write (a checkout, say)"
mkdir -p /home/user && chmod 0777 /home/user && cp /rig/run-guest.new.sh /home/user/run-guest.sh
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 \
    bash /home/user/run-guest.sh probe checkout 2>&1 | sed 's/^/    | /' | grep -v WARNING | head -3
echo "== new: a file not root's (uid 65534 here: /proc/version, of a uid this namespace does not map)"
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_KERNEL=/proc/version NVGPU_SKIP_MEM_CHECK=1 NVGPU_ALLOW_ROOT_UNSAFE=1 \
    NVGPU_DIAGNOSTIC=1 bash /rig/run-guest.new.sh probe notroots 2>&1 | sed 's/^/    | /' | grep -v WARNING | head -3
echo "== new: NVGPU_VMM_JAIL defaults to on: no jailer, no run"
mv /rig/bin/jailer /rig/bin/jailer.away
run run-guest.new.sh nojailer
mv /rig/bin/jailer.away /rig/bin/jailer
