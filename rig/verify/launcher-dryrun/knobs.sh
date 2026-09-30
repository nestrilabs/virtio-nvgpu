#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# The tuning knobs (rig/launcher/tuning.sh), unprivileged, with stubs that
# say what they were given: each knob unset leaves the command lines as they
# were, each set puts in what it says and nothing else, and each refuses a
# value it cannot take. NVGPU_CORE_SCHED=shared must put the backend and the
# VMM in one core-scheduling cookie from their first instruction, crosvm's
# sandbox and user namespace included. Self-checking: exits non-zero, having
# said FAIL, when any check fails.
#
# Usage: knobs.sh <run-guest.sh>   (run.sh runs it)
set -u
D=$(cd "$(dirname "$0")" && pwd)
L=$1
R=$D/knobrig
rm -rf "$R"
mkdir -p "$R"/{bin,kernel,guest,logs,run}
echo kernel > "$R/kernel/vmlinux"
truncate -s 64M "$R/guest/rootfs.ext4"
X=$(mktemp -d /tmp/rk.XXXXXX)
chmod 0700 "$X"
trap 'rm -rf "$X" "$R"' EXIT

# The backend: its arguments and its cookie, then the socket, served until
# the VMM has come and gone.
cat > "$R/bin/vhost-user-nvgpu" <<'EOF'
#!/bin/sh
echo "stub backend: args: $*"
echo "stub backend: cookie $(coresched get -s $$ 2>&1 | sed 's/.* //')"
sock=
for a; do case $a in --socket) sock=next ;; *) [ "$sock" = next ] && sock=$a ;; esac; done
exec python3 -c 'import socket, sys, time
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(); time.sleep(30)' "$sock"
EOF
# crosvm: its help (the flags the launcher looks for), or its argv, its
# cookie, its RLIMIT_FSIZE and a device process's cookie.
cat > "$R/bin/crosvm" <<'EOF'
#!/bin/sh
if [ "$2" = --help ]; then
    echo "  --no-pci-hotplug-port  --prefault-memory  --per-vm-core-scheduling  nvgpu-uvm-aperture"
    exit 0
fi
echo "stub vmm: argv: $*"
echo "stub vmm: cookie $(coresched get -s $$ 2>&1 | sed 's/.* //')"
sh -c 'echo "stub vmm: a device process: cookie $(coresched get -s $$ 2>&1 | sed "s/.* //")"'
echo "stub vmm: fsize $(prlimit --pid $$ --fsize -o SOFT --noheadings | tr -d ' ')"
EOF
# nesbox: its config and the same. (It takes the window's size from the
# backend: VHOST_USER_GET_SHMEM_CONFIG.)
cat > "$R/bin/nesbox" <<'EOF'
#!/bin/sh
# VHOST_USER_GET_SHMEM_CONFIG
echo "stub vmm: config: $(tr -d '\n ' < "$1")"
echo "stub vmm: cookie $(coresched get -s $$ 2>&1 | sed 's/.* //')"
echo "stub vmm: fsize $(prlimit --pid $$ --fsize -o SOFT --noheadings | tr -d ' ')"
EOF
chmod 0755 "$R/bin/"*

n=0
bad=0
run() { # run TAG [ENV=...]... [-- launcher args]: the launcher's output, then both logs
    local tag=$1 envs=() args=()
    shift
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    [ $# -gt 0 ] && shift && args=("$@")
    env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 \
        NVGPU_TIMEOUT=5 NVGPU_VMM_NETNS=0 NVGPU_OOM_SCORE_ADJ= ${envs[@]+"${envs[@]}"} \
        bash "$L" ${args[@]+"${args[@]}"} probe "$tag" > "$R/logs/$tag.out" 2>&1
    OUT=$(cat "$R/logs/$tag.out" "$R/logs/$tag.backend.log" "$R/logs/$tag.console.log" 2>/dev/null)
}
has() { # has WHAT TEXT: OUT contains TEXT
    n=$((n + 1))
    if grep -qF -- "$2" <<<"$OUT"; then
        echo "  ok: $1"
    else
        echo "  FAIL: $1: no \"$2\"; the run said:"
        sed 's/^/    | /' <<<"$OUT" | head -40
        bad=$((bad + 1))
    fi
}
hasnt() { # hasnt WHAT TEXT
    n=$((n + 1))
    if grep -qF -- "$2" <<<"$OUT"; then
        echo "  FAIL: $1: \"$2\" is there"
        bad=$((bad + 1))
    else
        echo "  ok: $1"
    fi
}
cookie() { # cookie WHO: the cookie the stub named WHO printed
    sed -n "s/^stub $1: cookie //p" <<<"$OUT" | head -1
}

echo "== knobs unset: crosvm as before"
run cvdef -- --vmm crosvm
has "prefault as before" "--prefault-memory"
hasnt "no core-scheduling flag" "core-scheduling"
hasnt "no poll flag" "--queue-poll-us"
hasnt "no rate flags" "--fifo-disable"
hasnt "no guest knob" "cpuidle_haltpoll"
has "no file-size limit" "stub vmm: fsize unlimited"
has "the backend's default window" "stub vmm: argv:"
hasnt "no window flag" "--window-size"

echo "== knobs unset: nesbox as before"
run nbdef
has "no cookie for nesbox" "stub vmm: cookie 0x0"
hasnt "nesbox config unchanged" "prefault"

echo "== NVGPU_CORE_SCHED"
run cvoff NVGPU_CORE_SCHED=off -- --vmm crosvm
has "off: crosvm told" "--core-scheduling=false"
run cvlegacy NVGPU_CROSVM_CORE_SCHED=0 -- --vmm crosvm
has "the older NVGPU_CROSVM_CORE_SCHED=0 still means off" "--core-scheduling=false"
run cvvm NVGPU_CORE_SCHED=vm -- --vmm crosvm
has "vm: crosvm told" "--per-vm-core-scheduling"
run cvshared NVGPU_CORE_SCHED=shared -- --vmm crosvm
has "shared: crosvm's own off" "--core-scheduling=false"
has "shared: said" "the backend and the VMM share one cookie"
b=$(cookie backend) v=$(cookie vmm)
n=$((n + 1))
if [ -n "$b" ] && [ "$b" != 0x0 ] && [ "$b" = "$v" ] &&
    grep -qF "stub vmm: a device process: cookie $v" <<<"$OUT"; then
    echo "  ok: shared: backend $b, VMM $v and its device process one cookie"
else
    echo "  FAIL: shared: backend '$b', VMM '$v'"
    bad=$((bad + 1))
fi
run cvsharedns NVGPU_CORE_SCHED=shared NVGPU_VMM_NETNS=1 -- --vmm crosvm
b=$(cookie backend) v=$(cookie vmm)
n=$((n + 1))
if [ -n "$b" ] && [ "$b" != 0x0 ] && [ "$b" = "$v" ]; then
    echo "  ok: shared: kept into crosvm's own user namespace"
else
    echo "  FAIL: shared, NVGPU_VMM_NETNS=1: backend '$b', VMM '$v'"
    bad=$((bad + 1))
fi
run nbshared NVGPU_CORE_SCHED=shared
b=$(cookie backend) v=$(cookie vmm)
n=$((n + 1))
if [ -n "$b" ] && [ "$b" != 0x0 ] && [ "$b" = "$v" ]; then
    echo "  ok: shared under nesbox: one cookie"
else
    echo "  FAIL: nesbox shared: backend '$b', VMM '$v'"
    bad=$((bad + 1))
fi
run nbvm NVGPU_CORE_SCHED=vm
b=$(cookie backend) v=$(cookie vmm)
n=$((n + 1))
if [ "$b" = 0x0 ] && [ -n "$v" ] && [ "$v" != 0x0 ]; then
    echo "  ok: vm under nesbox: a cookie of the VMM's own, none for the backend"
else
    echo "  FAIL: nesbox vm: backend '$b', VMM '$v'"
    bad=$((bad + 1))
fi
run nbpervcpu NVGPU_CORE_SCHED=per-vcpu
has "nesbox has no per-vcpu" "nesbox makes no core-scheduling cookie per vCPU"
run csbad NVGPU_CORE_SCHED=sometimes
has "a mode it does not know" "NVGPU_CORE_SCHED=sometimes: per-vcpu, shared, vm or off"

echo "== the backend's flags"
run poll NVGPU_QUEUE_POLL_US=10 NVGPU_FIFO_DISABLE_RATES=10,20,400,320
has "poll" "--queue-poll-us 10"
has "rates" "--fifo-disable-proc-rate 10 --fifo-disable-proc-burst 20 --fifo-disable-vm-rate 400 --fifo-disable-vm-burst 320"
has "rates, told to the guest" "nvgpu_fifo_disable_rates=10,20,400,320"
run pollbad NVGPU_QUEUE_POLL_US=5000
has "a poll past 1000" "NVGPU_QUEUE_POLL_US=5000: 0 to 1000"
run ratesbad NVGPU_FIFO_DISABLE_RATES=10,20
has "rates, not four" "PROC_RATE,PROC_BURST,VM_RATE,VM_BURST"
run creative NVGPU_WINDOW_PRESET=creative
has "the creative preset" "--window-size 8192"
run creative2 NVGPU_WINDOW_PRESET=creative NVGPU_WINDOW_MIB=4096
has "an explicit size wins" "--window-size 4096"
run presetbad NVGPU_WINDOW_PRESET=huge
has "a preset it does not know" "NVGPU_WINDOW_PRESET=huge: default or creative"

echo "== the VMM's file-size limit"
run fsize NVGPU_VMM_FSIZE_MIB=8192 -- --vmm crosvm
has "crosvm under it" "stub vmm: fsize $((8192 * 1048576))"
run fsizenb NVGPU_VMM_FSIZE_MIB=8192
has "nesbox under it" "stub vmm: fsize $((8192 * 1048576))"
run fsizelow NVGPU_VMM_FSIZE_MIB=2048
has "below guest RAM" "is below guest RAM (NVGPU_MEM_MIB), 4096 MiB"
run fsizewin NVGPU_VMM_FSIZE_MIB=4096 NVGPU_WINDOW_MIB=8192
has "below the window" "is below the window (NVGPU_WINDOW_MIB), 8192 MiB"
truncate -s 5G "$R/guest/rootfs.ext4"
run fsizedisk NVGPU_VMM_FSIZE_MIB=4096
has "below the disk" "is below the disk, 5120 MiB"
truncate -s 64M "$R/guest/rootfs.ext4"

echo "== the C-state cap, unprivileged"
run cstate NVGPU_CPU_LATENCY_US=50
has "refused without root" "/dev/cpu_dma_latency is root's"

echo "== the guest's knobs"
run guest NVGPU_GUEST_HALTPOLL=1 NVGPU_GUEST_RT_SPIN_US=10 NVGPU_GUEST_ASYNC_FENCE_WATCH=0 \
    NVGPU_GUEST_THP=madvise -- --vmm crosvm
has "haltpoll" "cpuidle_haltpoll.force=1"
has "haltpoll is not recommended" "which cost a D3D11 game under Wine 9%"
has "rt_spin_us" "virtio_gpu_nv.rt_spin_us=10"
has "async_fence_watch" "virtio_gpu_nv.async_fence_watch=N"
has "THP" "transparent_hugepage=madvise"
run guestbad NVGPU_GUEST_RT_SPIN_US=99999
has "a spin past 1000" "NVGPU_GUEST_RT_SPIN_US=99999: 0 to 1000"
run thpbad NVGPU_GUEST_THP=sometimes
has "a THP mode it does not know" "NVGPU_GUEST_THP=sometimes: always, madvise or never"

echo "== NVGPU_PREFAULT=0"
run noprefault NVGPU_PREFAULT=0 -- --vmm crosvm
hasnt "crosvm not told to prefault" "--prefault-memory"

echo "== the units' helper (contrib/systemd/nvgpu-vmm-exec)"
H=$(dirname "$L")/../contrib/systemd/nvgpu-vmm-exec
vx() { # vx [ENV=...]... -- ARGS: the helper's output, with a stub VMM
    local envs=()
    while [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    OUT=$(env -i PATH="$R/bin:$PATH" ${envs[@]+"${envs[@]}"} sh "$H" "$@" 2>&1)
}
cat > "$R/bin/vmmstub" <<'EOF'
#!/bin/sh
echo "stub vmm: argv: $*"
echo "stub vmm: cookie $(coresched get -s $$ 2>&1 | sed 's/.* //')"
echo "stub vmm: slice $(chrt -p $$ | sed -n 's/.*runtime parameter: //p')"
EOF
chmod 0755 "$R/bin/vmmstub"
vx -- crosvm vmmstub run --x
has "units, crosvm defaults: prefault, no core flag" "stub vmm: argv: run --prefault-memory --x"
has "units, crosvm defaults: no cookie of its own" "stub vmm: cookie 0x0"
has "units, crosvm defaults: the 100 us slice" "stub vmm: slice 100000"
vx NVGPU_SLICE_US=250 -- crosvm vmmstub run --x
has "units: NVGPU_SLICE_US=250" "stub vmm: slice 250000"
vx NVGPU_CORE_SCHED=off NVGPU_PREFAULT=0 -- crosvm vmmstub run --x
has "units, crosvm off, no prefault" "stub vmm: argv: run --core-scheduling=false --x"
vx NVGPU_CORE_SCHED=vm -- crosvm vmmstub run --x
has "units, crosvm vm" "stub vmm: argv: run --per-vm-core-scheduling --prefault-memory --x"
vx NVGPU_CORE_SCHED=shared -- crosvm vmmstub run --x
has "units, crosvm shared: crosvm's own off" "stub vmm: argv: run --core-scheduling=false --prefault-memory --x"
hasnt "units, crosvm shared: a cookie from its exec" "stub vmm: cookie 0x0"
vx -- nesbox vmmstub --config c
has "units, nesbox defaults" "stub vmm: argv: --config c"
has "units, nesbox defaults: no cookie" "stub vmm: cookie 0x0"
vx NVGPU_CORE_SCHED=vm -- nesbox vmmstub --config c
hasnt "units, nesbox vm: a cookie of its own" "stub vmm: cookie 0x0"
for refusal in "NVGPU_CORE_SCHED=per-vcpu:nesbox:nesbox makes no cookie per vCPU" \
    "NVGPU_PREFAULT=0:nesbox:nesbox's prefault is its config's" \
    "NVGPU_CORE_SCHED=sometimes:crosvm:per-vcpu, vm, shared or off" \
    "NVGPU_SLICE_US=50:crosvm:0, or 100 to 100000" \
    "NVGPU_SLICE_US=1e3:crosvm:0, or 100 to 100000" \
    "NVGPU_PREFAULT=yes:crosvm:0 or 1"; do
    IFS=: read -r e k says <<<"$refusal"
    if [ "$k" = crosvm ]; then vx "$e" -- crosvm vmmstub run; else vx "$e" -- nesbox vmmstub; fi
    has "units: $e refused ($k)" "$says"
    hasnt "units: $e: the VMM not run" "stub vmm: argv"
done
# join: a backend (a sleep with no cookie) takes the VMM's, as root would
# give it; systemctl answers with the backend's pid.
sleep 30 &
BE=$!
coresched new -- sleep 30 &
VM=$!
printf '#!/bin/sh\necho %s\n' "$BE" > "$R/bin/systemctl"
chmod 0755 "$R/bin/systemctl"
sleep 0.2
vx -- join 0 "$VM"
hasnt "units, join: nothing without shared" "shares"
vx NVGPU_CORE_SCHED=shared -- join 0 "$VM"
has "units, join: the backend takes the VMM's cookie" "shares the VMM's core-scheduling cookie"
n=$((n + 1))
if [ "$(coresched get -s "$BE" | sed 's/.* //')" = "$(coresched get -s "$VM" | sed 's/.* //')" ]; then
    echo "  ok: units, join: one cookie"
else
    echo "  FAIL: units, join: the backend's cookie is not the VMM's"
    bad=$((bad + 1))
fi
kill "$BE" "$VM" 2>/dev/null
wait "$BE" "$VM" 2>/dev/null

if [ "$bad" = 0 ]; then
    echo "knobs: all $n checks passed"
else
    echo "knobs: FAIL: $bad of $n checks failed"
    exit 1
fi
