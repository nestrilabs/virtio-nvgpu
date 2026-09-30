#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Runs as root of a user namespace, in its own mount and pid namespaces, chrooted
# into a tmpfs root that holds a root-owned rig: two launchers (the old, e513ba9,
# and the new), stub binaries, and a fake pool of slot users. The namespace maps
# one uid, so every user is uid 0 here: the backend is "root" and needs
# NVGPU_ALLOW_ROOT_UNSAFE (and, new, NVGPU_DIAGNOSTIC); what is checked --
# which processes are killed, what the socket's permissions go to, what the
# launcher hands the backend and what it changes the ownership of -- does not
# depend on the uids. (What does -- another process of the backend's user
# renaming things in a directory that user owns -- cannot be played with one
# uid; the checks below show the new launcher never gives that user one.)
set -u
export PATH=/stubs:/run/current-system/sw/bin
cd /
run() { # run <launcher> <tag> [env...] [-- launcher args after the tag]
    local l=$1 tag=$2 envs=() args=()
    shift 2
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    [ $# -gt 0 ] && shift && args=("$@")
    env -i PATH=$PATH HOME=/root NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=5 \
        NVGPU_OOM_SCORE_ADJ= NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 ${envs[@]+"${envs[@]}"} \
        bash "/rig/$l" probe "$tag" ${args[@]+"${args[@]}"} > "/rig/logs/$tag.launcher" 2>&1
    echo "  exit $?; launcher said:"
    sed 's/^/    | /' "/rig/logs/$tag.launcher" | grep -v "^    | *$" | head -n "${LINES_SHOWN:-12}"
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
echo "== new: a piece of the launcher (rig/launcher) someone else can write"
chmod 0666 /rig/launcher/guest.sh
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 \
    bash /rig/run-guest.new.sh probe piece 2>&1 | sed 's/^/    | /' | grep -v WARNING | head -3
chmod 0644 /rig/launcher/guest.sh
echo "== new: a piece reached through a link in a directory someone else can write"
# The check passes (what the link names is root's), and the launcher must
# then read what it checked, not whatever the link names by then.
mkdir -p /rig/real && cp /rig/launcher/verdict.sh /rig/real/verdict.sh
echo 'echo "verdict.sh read from ${BASH_SOURCE[0]}" >&2' >> /rig/real/verdict.sh
mv /rig/launcher/verdict.sh /rig/launcher/verdict.sh.away
ln -s /home/user/verdict-link /rig/launcher/verdict.sh
ln -s /rig/real/verdict.sh /home/user/verdict-link
LINES_SHOWN=60 run run-guest.new.sh viadir | grep -E 'exit|verdict.sh read from'
rm /rig/launcher/verdict.sh /home/user/verdict-link
mv /rig/launcher/verdict.sh.away /rig/launcher/verdict.sh
echo "== new: a file not root's (uid 65534 here: /proc/version, of a uid this namespace does not map)"
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_KERNEL=/proc/version NVGPU_SKIP_MEM_CHECK=1 NVGPU_ALLOW_ROOT_UNSAFE=1 \
    NVGPU_DIAGNOSTIC=1 bash /rig/run-guest.new.sh probe notroots 2>&1 | sed 's/^/    | /' | grep -v WARNING | head -3
echo "== new: slot 0's socket unit listens (contrib/systemd's VM 0)"
mkdir -p /run/nvgpu/vm0
python3 -c 'import socket; socket.socket(socket.AF_UNIX).bind("/run/nvgpu/vm0/nvgpu.sock")'
run run-guest.new.sh unitslot | grep -E 'exit|slot'
rm -rf /run/nvgpu
echo "== new: NVGPU_VMM_JAIL defaults to on: no jailer, no run"
mv /rig/bin/jailer /rig/bin/jailer.away
run run-guest.new.sh nojailer
mv /rig/bin/jailer.away /rig/bin/jailer

# ── A root run: the socket, diagnostics, tags, the console, the terminal, the environment ──

echo "== new: VD-H1: root binds the socket and hands it over; the run's directory is never the backend user's"
: > /rig/logs/ownership.log
LINES_SHOWN=4 run run-guest.new.sh h1
grep -a '^stub backend' /rig/logs/h1.vm0.backend.log | sed 's/^/    /'
grep -a '^jailer stub: connected' /rig/logs/h1.vm0.console.log | sed 's/^/    /'
echo "  every change of ownership the launcher made:"
sed 's/^/    /' /rig/logs/ownership.log
if grep -q '/run/nvgpu\.[^/ ]*$' /rig/logs/ownership.log; then
    echo "  FAIL: the run's directory changed hands"
else
    echo "  ok: the run's directory stayed root's; only the socket and the disk copy went to the slot"
fi

echo "== new: a root run started with umask 000 writes nothing others can write"
: > /rig/logs/umask.log
(umask 000; LINES_SHOWN=2 run run-guest.new.sh umask0)
sed 's/^/    /' /rig/logs/umask.log
echo "  the VMM's config and the console log: $(stat -c %a /rig/logs/umask0.vm0.json /rig/logs/umask0.vm0.console.log | tr '\n' ' ')"

echo "== new: VD-H3: a diagnostic backend flag as root, without NVGPU_DIAGNOSTIC=1"
env -i PATH=$PATH NVGPU_RIG=/rig NVGPU_SKIP_MEM_CHECK=1 bash /rig/run-guest.new.sh probe h3a -- \
    --permissive-abi --keep-guest-coherency 2>&1 | sed 's/^/    | /' | head -3
echo "== new: VD-H3: with NVGPU_DIAGNOSTIC=1, each one is said on the terminal"
LINES_SHOWN=40 run run-guest.new.sh h3b -- -- --allow-unmeasured-release --proc-nvidia /x |
    grep -E 'exit|diagnostic flag'

echo "== new: VD-H7: a second run with a tag a live run holds"
(flock 9; sleep 4) 9>>/rig/logs/dup.vm0.json &
sleep 0.3
run run-guest.new.sh dup
wait
echo "  (a root run's files carry its slot: $(cd /rig/logs && printf '%s ' benign.vm0.*))"

echo "== new: VD-H2: a guest that floods its console, with NVGPU_LOG_MAX_MIB=1"
LINES_SHOWN=2 run run-guest.new.sh flood NVGPU_LOG_MAX_MIB=1
echo "  console log: $(stat -c %s /rig/logs/flood.vm0.console.log) bytes; $(grep -ac 'the console log passed NVGPU_LOG_MAX_MIB=1; the VM was stopped' /rig/logs/flood.vm0.console.log) cap line"

echo "== new: VD-H5: a verdict line with a terminal escape sequence in it"
LINES_SHOWN=40 run run-guest.new.sh esc | grep -E 'exit|probe:'
echo "  ESC bytes on the launcher's output: $(grep -ac $'\033' /rig/logs/esc.launcher)"

echo "== new: VD-H6: a root launcher started with LD_LIBRARY_PATH, FOO_EVIL and a PATH of the user's"
LINES_SHOWN=2 run run-guest.new.sh h6 FOO_EVIL=1 LD_LIBRARY_PATH=/home/user
grep -a '^jailer stub: environment\|^jailer stub: FOO_EVIL' /rig/logs/h6.vm0.console.log | sed 's/^/    /'
echo "== new: VD-H6: a VMM whose libraries are not root's (/nix is uid 65534 here)"
LINES_SHOWN=40 run run-guest.new.sh h6lib NVGPU_VMM=/rig/bin/nesbox-dyn | grep -E 'exit|library'
echo "== new: NVGPU_CPU_LATENCY_US=50: /dev/cpu_dma_latency written as hex text, held for the run"
LINES_SHOWN=40 run run-guest.new.sh cstate NVGPU_CPU_LATENCY_US=50 | grep -E 'exit|C-states'
sleep 0.3
echo "  cpu_dma_latency: $(cat /dev/cpu_dma_latency); its holder: $(pgrep -f '^sleep 125$' >/dev/null && echo alive || echo gone)"
