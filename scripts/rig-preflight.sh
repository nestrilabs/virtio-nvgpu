#!/usr/bin/env bash
# Is this host ready to run the rig? Checks what an unprivileged
# scripts/run-guest.sh needs, one line each:
#
#   OK    it is there and usable
#   WARN  it works, but something about it will bite a particular stage
#   FAIL  a run will not work until this is fixed
#
# followed by a hint where there is something to do. Exits non-zero only when
# something FAILed.
#
# Usage: rig-preflight.sh [--rig DIR] [--guest-version VERSION]
#   --rig DIR               the rig (default $NVGPU_RIG, else the repo's .rig)
#   --guest-version V       the NVIDIA userspace the guest image carries
#                           (default: guest/nvidia-version in the rig, else
#                           $NVGPU_GUEST_NV_VERSION, else 595.99.02)
#
# It opens nothing: device nodes are checked with test -r/-w, which asks the
# kernel's permission check without touching the driver. The only programs it
# runs are the rig's own binaries, with --help, to see that they start at all
# (a binary built against a nix store path the host does not have does not).
set -uo pipefail

REPO=$(cd -- "$(dirname -- "$0")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
GUEST_VERSION=
while [ $# -gt 0 ]; do
    case $1 in
        --rig) RIG=${2:?--rig needs a directory}; shift 2 ;;
        --guest-version) GUEST_VERSION=${2:?--guest-version needs a version}; shift 2 ;;
        -h | --help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument $1 (see --help)" >&2; exit 2 ;;
    esac
done

NFAIL=0
NWARN=0
ok() { printf 'OK    %s\n' "$*"; }
warn() { printf 'WARN  %s\n' "$*"; NWARN=$((NWARN + 1)); }
fail() { printf 'FAIL  %s\n' "$*"; NFAIL=$((NFAIL + 1)); }
hint() { printf '      hint: %s\n' "$*"; }
section() { printf '\n== %s ==\n' "$*"; }

# id prints the number and fails for a uid with no passwd entry (a sandbox).
ME=$(id -un 2>/dev/null) || ME="uid $(id -u)"
MYGROUPS=$(id -Gn 2>/dev/null | tr ' ' ',')

# Who owns a node and how, for a hint that says which group or bind is missing.
node_desc() { stat -c '%A %U:%G' -- "$1" 2>/dev/null || echo '?'; }

# A device node the user must be able to open read-write.
need_rw() {
    local node=$1 what=$2 level=${3:-fail}
    if [ ! -e "$node" ]; then
        "$level" "$node missing ($what)"
        hint "not present here: inside the sandbox it needs a --dev-bind; on the host, is the module loaded?"
    elif [ -r "$node" ] && [ -w "$node" ]; then
        ok "$node rw ($what)"
    else
        "$level" "$node not rw for $ME ($what): $(node_desc "$node")"
        hint "add the user to group $(stat -c %G -- "$node" 2>/dev/null), or give the node to the session (uaccess)"
    fi
}

# ── The user ─────────────────────────────────────────────────────────────────
section "user"
if [ "$(id -u)" = 0 ]; then
    fail "running as root"
    hint "the rig runs unprivileged: run this and scripts/run-guest.sh as your own user" \
        "(as root, run-guest.sh uses the /root layout and its own user switching instead)"
else
    ok "running as $ME (uid $(id -u))"
fi
# The backend keeps the user's groups (an unprivileged setpriv cannot drop
# them), so a group that is root in all but name is one step from a guest
# that takes the backend over.
risky=()
for g in ${MYGROUPS//,/ }; do
    case $g in docker | lxd | incus-admin | libvirt | libvirtd | disk | podman) risky+=("$g") ;; esac
done
if [ ${#risky[@]} -gt 0 ]; then
    warn "user is in ${risky[*]}: the backend inherits these groups, and each is root-equivalent"
    hint "run the rig as a user without them, or as root with the nvgpu system user (run-guest.sh as root)"
else
    ok "no root-equivalent supplementary groups ($MYGROUPS)"
fi
if command -v setpriv >/dev/null && setpriv --no-new-privs --inh-caps=-all --ambient-caps=-all true 2>/dev/null; then
    ok "setpriv can set no_new_privs and clear the inheritable/ambient sets"
else
    fail "setpriv missing or refuses --no-new-privs --inh-caps=-all --ambient-caps=-all"
    hint "util-linux provides setpriv; run-guest.sh starts the backend through it"
fi
if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -d "$XDG_RUNTIME_DIR" ] && [ -w "$XDG_RUNTIME_DIR" ]; then
    ok "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR (the backend's socket directory goes here)"
else
    warn "no usable XDG_RUNTIME_DIR; the socket directory goes under $RIG/run"
    hint "fine, as long as $RIG/run gives a socket path under 108 bytes"
fi

# ── KVM ──────────────────────────────────────────────────────────────────────
section "kvm"
need_rw /dev/kvm "the VMM"
if [ -d /sys/module/kvm_amd ]; then
    ok "kvm_amd loaded (AMD host: guest PAT honoured; the Intel-only caching failures cannot occur)"
elif [ -d /sys/module/kvm_intel ]; then
    ok "kvm_intel loaded (Intel host: the caching stage's H-4 case applies)"
elif [ -d /sys/module ]; then
    warn "neither kvm_amd nor kvm_intel in /sys/module"
else
    warn "no /sys: cannot tell which KVM module is loaded"
fi

# ── NVIDIA nodes ─────────────────────────────────────────────────────────────
section "nvidia device nodes"
need_rw /dev/nvidiactl "RM control"
need_rw /dev/nvidia0 "the GPU"
need_rw /dev/nvidia-uvm "UVM"
need_rw /dev/nvidia-modeset "NVKMS (display stages, semsurf)"
need_rw /dev/udmabuf "stub fences, only when no NVIDIA render node takes a syncobj" warn

# ── DRM nodes ────────────────────────────────────────────────────────────────
section "drm nodes"
NV_CARDS=()
NV_RENDER=()
if ! compgen -G '/dev/dri/*' >/dev/null; then
    fail "no /dev/dri nodes"
    hint "the backend gives the guest a render node, and the display stages a card node;" \
        "inside the sandbox /dev/dri needs a --dev-bind"
else
    for node in /dev/dri/card* /dev/dri/renderD*; do
        [ -e "$node" ] || continue
        name=${node##*/}
        vendor=$(cat "/sys/class/drm/$name/device/vendor" 2>/dev/null) || vendor=
        driver=$(basename -- "$(readlink "/sys/class/drm/$name/device/driver" 2>/dev/null)" 2>/dev/null) || driver=
        pci=$(basename -- "$(readlink "/sys/class/drm/$name/device" 2>/dev/null)" 2>/dev/null) || pci=
        access=no-access
        [ -r "$node" ] && [ -w "$node" ] && access=rw
        if [ -z "$vendor" ]; then
            warn "$name: cannot tell whose it is (no /sys/class/drm/$name/device/vendor); $access"
            continue
        fi
        if [ "$vendor" = 0x10de ]; then
            case $name in card*) NV_CARDS+=("$name") ;; *) NV_RENDER+=("$name") ;; esac
            if [ $access = rw ]; then
                ok "$name: NVIDIA ($driver, $pci), rw"
            elif [[ $name == renderD* ]]; then
                fail "$name: NVIDIA ($driver, $pci), not rw: $(node_desc "$node")"
                hint "the guest's Vulkan device is this node; add the user to group $(stat -c %G "$node")"
            else
                warn "$name: NVIDIA ($driver, $pci), not rw: $(node_desc "$node")"
                hint "only the KMS stages (lease 4/5/9 via the compositor, compositor-VM 6/7) open it;" \
                    "logind hands it to the active session's user on a TTY"
            fi
        else
            ok "$name: vendor $vendor ($driver, $pci), not the NVIDIA card; $access"
        fi
    done
    [ ${#NV_RENDER[@]} -gt 0 ] || { fail "no NVIDIA render node identified"; hint "needs /sys/class/drm to be visible, and nvidia_drm loaded"; }
fi
# Monitors on the NVIDIA card: what a KMS stage will be taking over.
MONITORS=()
for c in "${NV_CARDS[@]}"; do
    for st in /sys/class/drm/"$c"-*/status; do
        [ -e "$st" ] || continue
        [ "$(cat "$st" 2>/dev/null)" = connected ] || continue
        conn=${st%/status}
        MONITORS+=("${conn##*/"$c"-}")
    done
done
if [ ${#NV_CARDS[@]} -gt 0 ]; then
    if [ ${#MONITORS[@]} -gt 0 ]; then
        ok "monitors connected to the NVIDIA card: ${MONITORS[*]}"
    else
        ok "no monitor connected to the NVIDIA card"
    fi
fi

# ── Driver ───────────────────────────────────────────────────────────────────
section "driver"
if [ -z "$GUEST_VERSION" ]; then
    if [ -r "$RIG/guest/nvidia-version" ]; then
        GUEST_VERSION=$(tr -d '[:space:]' < "$RIG/guest/nvidia-version")
        GV_FROM="$RIG/guest/nvidia-version"
    elif command -v debugfs >/dev/null && [ -r "$RIG/guest/rootfs.ext4" ] &&
        GUEST_VERSION=$(debugfs -R 'cat /etc/nvgpu/manifest' "$RIG/guest/rootfs.ext4" 2>/dev/null |
            sed -n 's/^nvidia-userspace \([^ ]*\).*/\1/p') && [ -n "$GUEST_VERSION" ]; then
        # guest-image/mkimage.sh records what it baked in here.
        GV_FROM="the image's /etc/nvgpu/manifest"
    else
        GUEST_VERSION=${NVGPU_GUEST_NV_VERSION:-595.99.02}
        GV_FROM="default (no $RIG/guest/nvidia-version)"
    fi
else
    GV_FROM="--guest-version"
fi
HOST_VERSION=
if [ -r /proc/driver/nvidia/version ]; then
    line=$(head -n 1 /proc/driver/nvidia/version)
    HOST_VERSION=$(grep -o -E '[0-9]{3}\.[0-9]+(\.[0-9]+)?' <<<"$line" | head -n 1)
    flavour=proprietary
    [[ $line == *"Open Kernel Module"* ]] && flavour=open
    if [ -z "$HOST_VERSION" ]; then
        fail "cannot read a version from /proc/driver/nvidia/version: $line"
    elif [ "$HOST_VERSION" = "$GUEST_VERSION" ]; then
        ok "host kernel module $HOST_VERSION ($flavour) = guest userspace $GUEST_VERSION ($GV_FROM)"
    else
        fail "host kernel module $HOST_VERSION, guest userspace $GUEST_VERSION ($GV_FROM)"
        hint "RM refuses a client of another version: rebuild the guest image with the host's .run"
    fi
else
    fail "no /proc/driver/nvidia/version: the nvidia module is not loaded, or /proc/driver is hidden"
fi
V=${HOST_VERSION:-$GUEST_VERSION}
for t in nvkms uvm; do
    if [ -f "$REPO/gen/$t/$V.json" ]; then
        ok "gen/$t/$V.json present"
    else
        fail "no gen/$t/$V.json: the backend has no $t table for $V"
        hint "the per-release tables are generated from the driver source (gen/)"
    fi
done
# The backend reads each GPU's PCI config space and its drm/ directory in
# /sys, by the address /proc/driver/nvidia/gpus lists.
if compgen -G '/proc/driver/nvidia/gpus/*' >/dev/null; then
    for g in /proc/driver/nvidia/gpus/*; do
        addr=${g##*/}
        model=$(sed -n 's/^Model:[[:space:]]*//p' "$g/information" 2>/dev/null | head -n 1)
        if [ -r "/sys/bus/pci/devices/$addr/config" ] && [ -d "/sys/bus/pci/devices/$addr/drm" ]; then
            ok "GPU $addr (${model:-?}): PCI config and drm/ readable"
            # sysfs gives an unprivileged reader only the first 64 bytes of
            # config space; the guest's fake PCI device then has no
            # capability list (no PCIe capability: nvidia-smi's link fields
            # read as errors). Informational: the backend runs unprivileged
            # by design.
            cfg_len=$(head -c 4096 "/sys/bus/pci/devices/$addr/config" 2>/dev/null | wc -c)
            if [ "${cfg_len:-0}" -lt 256 ]; then
                warn "GPU $addr: only $cfg_len bytes of PCI config space readable as $(id -un 2>/dev/null || id -u)"
                hint "the guest's PCI device gets no capabilities past byte $cfg_len; expect nvidia-smi PCIe link/gen fields to be errors, not a stage failure"
            fi
        else
            fail "GPU $addr (${model:-?}): /sys/bus/pci/devices/$addr/{config,drm} not readable"
            hint "the backend hands the guest driver that config space and finds render nodes there;" \
                "inside the sandbox /sys (or at least /sys/bus/pci and /sys/class/drm) must be bound"
        fi
    done
else
    fail "no GPUs under /proc/driver/nvidia/gpus"
fi
if [ -r /sys/module/nvidia_drm/parameters/modeset ]; then
    if [ "$(cat /sys/module/nvidia_drm/parameters/modeset)" = Y ]; then
        ok "nvidia_drm modeset=Y"
    else
        fail "nvidia_drm modeset is off"
        hint "boot with nvidia_drm.modeset=1: without it there is no NVKMS and no semsurf"
    fi
else
    warn "cannot read /sys/module/nvidia_drm/parameters/modeset"
    hint "needs /sys; the host must boot with nvidia_drm.modeset=1"
fi
for p in fbdev vblank; do
    f=/sys/module/nvidia_drm/parameters/$p
    [ -r "$f" ] && ok "nvidia_drm $p=$(cat "$f")"
done

# ── The desktop ──────────────────────────────────────────────────────────────
section "desktop"
socks=()
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    for s in "$XDG_RUNTIME_DIR"/wayland-*; do
        [ -S "$s" ] && socks+=("${s##*/}")
    done
fi
hypr=
compgen -G "${XDG_RUNTIME_DIR:-/nonexistent}/hypr/*/.socket.sock" >/dev/null && hypr=" (a Hyprland instance)"
on=${MONITORS[*]:-}
if [ ${#socks[@]} -gt 0 ]; then
    warn "a compositor is running: ${socks[*]}$hypr${WAYLAND_DISPLAY:+; this session is $WAYLAND_DISPLAY}"
    hint "who is DRM master of the NVIDIA card is not visible from here. If this desktop runs on it" \
        "(${on:-no monitors seen on it}), the KMS stages 4, 5, 6 and 9 take a monitor or need the" \
        "desktop stopped -- TESTING-RIG.md. Stages 1, 2, 3 (against a separate headless compositor)" \
        "and the security negatives leave it alone."
else
    ok "no Wayland socket in ${XDG_RUNTIME_DIR:-(no XDG_RUNTIME_DIR)}"
    hint "inside the sandbox the host's runtime directory is hidden, so this proves nothing about the" \
        "live desktop; with monitors on the NVIDIA card (${on:-none seen}), assume the desktop runs there"
fi

# ── If it goes wrong (.rig/SAFETY-NOTES.md) ──────────────────────────────────
section "recovery"
avail_mib=$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo 2>/dev/null)
want_mib=$((${NVGPU_MEM_MIB:-4096} + 1024 + ${NVGPU_MEM_HEADROOM_MIB:-4096}))
if [ -z "$avail_mib" ]; then
    warn "cannot read MemAvailable"
elif [ "$avail_mib" -lt "$want_mib" ]; then
    warn "$avail_mib MiB available; run-guest.sh wants $want_mib (guest + 1 GiB window + desktop headroom) and will refuse"
else
    ok "$avail_mib MiB available (a run wants $want_mib)"
fi
sysrq=$(cat /proc/sys/kernel/sysrq 2>/dev/null)
if [ "$sysrq" = 0 ]; then
    warn "kernel.sysrq=0: no Alt+SysRq way out of a frozen desktop"
    hint "sysctl kernel.sysrq=1 (or at least 0xf4: sync, remount, reboot, kill), as root, before the first GPU stage"
elif [ -n "$sysrq" ]; then
    ok "kernel.sysrq=$sysrq (Alt+SysRq works if the keyboard still does)"
fi
if [ "$(cat /proc/sys/kernel/panic_on_oops 2>/dev/null)" = 1 ]; then
    if [ "$(cat /proc/sys/kernel/panic 2>/dev/null)" = 0 ]; then
        warn "panic_on_oops=1 and panic=0: a host oops (nvidia, nvidia-drm, nvidia-uvm) freezes the machine until it is power-cycled"
        hint "save your work before each GPU stage; kernel.panic=10 would reboot on its own instead"
    else
        warn "panic_on_oops=1: a host oops reboots the machine; save your work before each GPU stage"
    fi
fi
ok "VRAM is not checked here (that opens the GPU); see SAFETY-NOTES.md: nvidia-smi on the host before and after a stage"

# ── The rig ──────────────────────────────────────────────────────────────────
section "rig artifacts ($RIG)"
runs() {
    local bin=$1 what=$2
    if [ ! -e "$bin" ]; then
        fail "$bin missing ($what)"
        return 1
    elif [ ! -x "$bin" ]; then
        fail "$bin not executable ($what)"
        return 1
    elif ! timeout 10 "$bin" --help >/dev/null 2>&1; then
        fail "$bin does not start ($what): $("$bin" --help 2>&1 | head -n 1)"
        hint "a nix-built binary needs its interpreter and libraries in /nix/store on this host;" \
            "one built in a chroot store may only have them in that store"
        return 1
    fi
    ok "$bin starts ($what)"
}
runs "$RIG/bin/vhost-user-nvgpu" "backend" || hint "cargo build --release -p device --features vhost-user --bin vhost-user-nvgpu, then copy it"
runs "$RIG/bin/nesbox" "VMM" || true
if [ -e "$RIG/bin/virtiofsd" ]; then
    runs "$RIG/bin/virtiofsd" "virtiofsd, only for NVGPU_NVIDIA_SHARE" || true
fi

KERNEL=$RIG/kernel/vmlinux
KREL=
if [ -r "$KERNEL" ]; then
    if [ "$(head -c 4 "$KERNEL" | od -An -c | tr -d ' ')" = '177ELF' ]; then
        KREL=$(grep -a -o -m 1 -E 'Linux version [^ ]+' "$KERNEL" | head -n 1 | awk '{print $3}')
        ok "$KERNEL is an ELF kernel, release ${KREL:-unknown}"
        if command -v readelf >/dev/null; then
            # nesbox does not use PVH: it loads the PT_LOAD segments at their
            # physical addresses and starts a vCPU in 64-bit mode at the ELF
            # entry (startup_64) with a boot_params page. QEMU's -kernel (the
            # TCG smoke, .rig/tcg-smoke.sh) boots an ELF only through PVH.
            entry=$(readelf -h "$KERNEL" 2>/dev/null | awk '/Entry point/ {print $NF}')
            if [ -n "$entry" ] && [ $((entry)) -gt 0 ] && [ $((entry)) -lt $((0x100000000)) ]; then
                ok "vmlinux ELF entry $entry is a physical address (nesbox's 64-bit entry)"
            else
                fail "vmlinux ELF entry '${entry:-?}' is not a low physical address"
                hint "nesbox jumps to the ELF entry itself; a vmlinux from the kernel build (not bzImage) has it"
            fi
            if readelf -n "$KERNEL" 2>/dev/null | grep -q -E 'Xen.*(0x00000012|PHYS32_ENTRY)'; then
                ok "vmlinux has a PVH entry note (only QEMU's TCG smoke uses it)"
            else
                warn "vmlinux has no PVH entry note: nesbox does not need one, .rig/tcg-smoke.sh (QEMU) does"
                hint "CONFIG_PVH=y"
            fi
        else
            ok "PVH note not checked (no readelf here)"
        fi
    else
        fail "$KERNEL is not an ELF file (a bzImage will not do; the VMM wants vmlinux)"
    fi
else
    fail "$KERNEL missing"
fi

KO=$RIG/kernel/nvgpu.ko
if [ -r "$KO" ]; then
    VM=$(grep -a -o -m 1 'vermagic=[^[:cntrl:]]*' "$KO" | head -n 1)
    VM=${VM#vermagic=}
    if [ -z "$VM" ]; then
        fail "$KO has no vermagic"
    elif [ -r "$KERNEL" ] && grep -a -q -F -- "$VM" "$KERNEL"; then
        ok "nvgpu.ko vermagic \"$VM\" matches vmlinux"
    elif [ -n "$KREL" ] && [ "${VM%% *}" = "$KREL" ]; then
        warn "nvgpu.ko release ${VM%% *} matches, but its full vermagic \"$VM\" is not in vmlinux"
        hint "a module built against another config of the same release may still refuse to load"
    else
        fail "nvgpu.ko vermagic \"$VM\" does not match vmlinux (${KREL:-unknown release})"
        hint "rebuild the module against exactly this kernel's build tree"
    fi
else
    fail "$KO missing (copied into the image at /opt/nvgpu/nvgpu.ko at assembly time)"
fi

ROOTFS=$RIG/guest/rootfs.ext4
if [ -r "$ROOTFS" ]; then
    magic=$(od -An -tx2 -j 1080 -N 2 "$ROOTFS" 2>/dev/null | tr -d ' ')
    if [ "$magic" = ef53 ]; then
        ok "$ROOTFS is ext4 ($(du -h --apparent-size "$ROOTFS" | cut -f1) apparent, $(du -h "$ROOTFS" | cut -f1) used)"
    else
        fail "$ROOTFS has no ext2/3/4 superblock"
    fi
    if [ -w "$ROOTFS" ]; then
        ok "golden rootfs is writable by you; runs boot a copy, so it stays as built"
    fi
    mkdir -p "$RIG/logs" 2>/dev/null
    need=$(stat -c %s "$ROOTFS")
    avail=$(df --output=avail -B1 "$RIG/logs" 2>/dev/null | tail -n 1 | tr -d ' ')
    if [ -n "$avail" ] && [ "$avail" -lt "$need" ]; then
        warn "$RIG/logs has $((avail >> 20)) MiB free, the rootfs is $((need >> 20)) MiB"
        hint "fine on btrfs/XFS (the per-run copy is a reflink); elsewhere each run needs a full copy"
    fi
    # The module is copied into the image at assembly time, not by nix: after
    # a kernel or module rebuild the image still carries the old one until
    # mkimage.sh --module-only runs.
    if command -v debugfs >/dev/null && [ -r "$KO" ]; then
        tmpko=$(mktemp)
        debugfs -R "dump /opt/nvgpu/nvgpu.ko $tmpko" "$ROOTFS" >/dev/null 2>&1
        if [ ! -s "$tmpko" ]; then
            fail "the image has no /opt/nvgpu/nvgpu.ko"
            hint "guest-image/mkimage.sh --module-only"
        elif cmp -s "$tmpko" "$KO"; then
            ok "the image's /opt/nvgpu/nvgpu.ko is $KO"
        else
            fail "the image's /opt/nvgpu/nvgpu.ko differs from $KO"
            hint "guest-image/mkimage.sh --module-only"
        fi
        rm -f "$tmpko"
    fi
    fstype=$(stat -f -c %T "$RIG/logs" 2>/dev/null)
    case $fstype in btrfs | xfs) ok "logs on $fstype: per-run rootfs copies are reflinks" ;; *) ok "logs on ${fstype:-?}: per-run rootfs copies are full copies" ;; esac
else
    fail "$ROOTFS missing"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
printf '\n%d FAIL, %d WARN\n' "$NFAIL" "$NWARN"
[ "$NFAIL" -eq 0 ]
