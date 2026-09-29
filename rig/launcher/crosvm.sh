# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's crosvm: what it is refused, the host checks, the
# command line (and the config that records it). Sourced by the launcher.
#
# crosvm (--vmm crosvm, rig layout: bin/crosvm, built with all of
# patches/crosvm applied by rig/rig-build-crosvm.sh): the same kernel, disk,
# console log, probe and backend; the guest's device is crosvm's vhost-user
# frontend of type nvgpu. crosvm has no config file, so <tag>.json records
# the command line it was given. Unprivileged, crosvm runs with its sandbox: every device
# it emulates itself (disk, consoles, rng) is a process of its own in a
# minijail -- user, pid, mount and network namespaces, pivoted into an empty
# directory (the rig's run/crosvm-empty), under its seccomp policy -- and so,
# with patches/crosvm 0007-0009, is the nvgpu vhost-user frontend: it checks
# the backend's mapping requests and passes them to the main process, which
# checks each again against the regions it laid out and makes it (window
# mappings; with --allow-compute, UVM pools in the aperture). A crosvm without
# those patches keeps the frontend in its main process, as upstream has it,
# and takes no --allow-compute. With NVGPU_VMM_NETNS=1 the main
# process is started in a user and network namespace of its own (unshare),
# the counterpart of nesbox's "unshare-network". No virtiofs share: crosvm's
# has no read-only mode, so NVGPU_NVIDIA_SHARE must be empty.
#
# Not implemented, as root: what production would use is crosvm as the slot's
# nvgpu-vmmN (setpriv, with only the slot's group and kvm), with its sandbox
# on and /var/empty as the pivot root, in a network namespace made by root,
# and the backend's socket 0660 in the slot's group -- the same slot, the same
# socket arrangement and the same disk copy as nesbox's jailer gets (root.sh).
# The launcher refuses --vmm crosvm as root until that has been built and run.

# What crosvm is refused: a root run (above), and a share.
crosvm_allowed() {
    [ $PRIV = user ] ||
        die "--vmm crosvm runs unprivileged only for now; the header of $LIB/crosvm.sh says what root would need"
    [ -z "$NVIDIA_SHARE" ] ||
        die "--vmm crosvm has no read-only virtiofs share; NVGPU_NVIDIA_SHARE must be empty"
}

# The host GPU's PCI address has to be free in the guest. The guest driver
# gives each GPU the host's own PCI address, and it cannot make a bus the VMM
# already has: the guest logs "cannot put the GPU at its host address" and
# has no GPU device. crosvm's root bus is bus 0, and it puts an empty
# hot-plug root port on the first free bus, bus 1 -- where most hosts have
# their GPU -- unless told --no-pci-hotplug-port (patches/crosvm/0003). The
# GPUs are the ones the backend serves: those nvidia.ko lists.
crosvm_check_host() {
    CROSVM_HELP=$("$VMM" run --help 2>&1 || true)
    case $CROSVM_HELP in *--no-pci-hotplug-port*) CROSVM_NO_HP=1 ;; esac
    for g in /proc/driver/nvidia/gpus/*; do
        [[ ${g##*/} =~ ^0000:([0-9a-fA-F]{2}): ]] || continue
        case $((16#${BASH_REMATCH[1]})) in
            0) die "the host GPU ${g##*/} is on PCI bus 0, crosvm's root bus in the guest:" \
                "the guest driver cannot give it that address" ;;
            1) [ "$CROSVM_NO_HP" = 1 ] ||
                die "the host GPU ${g##*/} is on PCI bus 1, where $VMM puts its hot-plug" \
                    "root port, and it has no --no-pci-hotplug-port to leave it out: build" \
                    "it with patches/crosvm (rig/rig-build-crosvm.sh)" ;;
        esac
    done
    [ "$CROSVM_NO_HP" = 1 ] ||
        echo "run-guest: note: $VMM has no --no-pci-hotplug-port; its hot-plug root port" \
            "takes PCI bus 1" >&2
    # Compute needs the UVM aperture (patches/crosvm 0007-0009), which the
    # help of --vhost-user names. A crosvm without it publishes the window
    # alone: the guest would get a UVM device whose pools it cannot map, and
    # CUDA would fail late.
    case $CROSVM_HELP in *nvgpu-uvm-aperture*) CROSVM_UVM=1 ;; *) CROSVM_UVM=0 ;; esac
    [ "$COMPUTE" = 0 ] || [ "$CROSVM_UVM" = 1 ] ||
        die "--allow-compute: $VMM has no UVM aperture; build crosvm with all of" \
            "patches/crosvm (rig/rig-build-crosvm.sh) or drop --allow-compute"
}

# The command line, and <tag>.json recording it (crosvm has no config
# file).
crosvm_config() {
    # The same guest, said as crosvm's command line. Two consoles to the one
    # log, as nesbox has: hvc0 (virtio-console) and COM1 for earlyprintk.
    # The queue size is the backend's: crosvm offers 32768 unless told. No
    # hot-plug root port: crosvm puts it on PCI bus 1, and the guest driver
    # gives the GPU the host's own PCI address, commonly 0000:01:00.0 (a
    # crosvm without the option was checked to need none: crosvm_check_host).
    CONSOLE_OPTS=type=stdout,hardware=virtio-console,console
    [ "$INTERACTIVE" = 1 ] && CONSOLE_OPTS=$CONSOLE_OPTS,stdin
    VMM_ARGS=(run --cpus "$VCPUS" --mem "$MEM_MIB"
        --block "path=$DISK"
        --serial "$CONSOLE_OPTS"
        --serial "type=stdout,hardware=serial,num=1,earlycon"
        --vhost-user "type=nvgpu,socket=$SOCK,max-queue-size=256")
    [ "$CROSVM_NO_HP" = 0 ] || VMM_ARGS+=(--no-pci-hotplug-port)
    if [ -n "$VCPU_PINS" ]; then
        pins= i=0
        for c in ${VCPU_PINS_J//,/ }; do pins+="${pins:+:}$i=$c"; i=$((i + 1)); done
        VMM_ARGS+=(--cpu-affinity "$pins")
    elif [ -n "$CPU_AFFINITY" ]; then
        VMM_ARGS+=(--cpu-affinity "$CPU_AFFINITY")
    fi
    case $HUGEPAGES in
        transparent) VMM_ARGS+=(--hugepages) ;;
        2m | 1g) die "NVGPU_HUGEPAGES=$HUGEPAGES: crosvm takes only transparent (--hugepages)" ;;
    esac
    [ "${NVGPU_CROSVM_CORE_SCHED:-1}" = 1 ] || VMM_ARGS+=(--core-scheduling=false)
    VMM_ARGS+=(-p "${BOOT_ARGS#"$CONSOLE_ARGS "}")
    case $DISK$SOCK in *,*) die "a comma in $DISK or $SOCK would split crosvm's option" ;; esac
    if [ "$CROSVM_SANDBOX" = on ]; then
        # minijail pivots each device process into an empty directory:
        # crosvm's default is /var/empty, which not every host has.
        EMPTY=$RIG/run/crosvm-empty
        mkdir -p "$EMPTY"
        chmod 0755 "$EMPTY"
        [ -z "$(ls -A -- "$EMPTY")" ] || die "$EMPTY must be empty: crosvm pivots its devices into it"
        VMM_ARGS+=(--pivot-root "$EMPTY")
    else
        VMM_ARGS+=(--disable-sandbox)
        CROSVM_NOTE="run-guest: WARNING: crosvm runs with --disable-sandbox (NVGPU_CROSVM_SANDBOX=off): every device it emulates is in its main process, with no minijail and no seccomp policy. For diagnosis only"
        echo "$CROSVM_NOTE" >&2
    fi
    VMM_ARGS+=("$KERNEL")
    VMM_MARK=socket=$SOCK
    {
        printf '{\n  "vmm": "crosvm",\n  "argv": [\n    %s' "$(json_str "$VMM")"
        for a in "${VMM_ARGS[@]}"; do printf ',\n    %s' "$(json_str "$a")"; done
        printf '\n  ]\n}\n'
    } > "$CFG"
}

# The command line crosvm runs with. Unprivileged, crosvm leaves the host's
# network the only way it can: in a user namespace of its own that maps just
# this user, where minijail still makes each device's namespaces beneath it.
crosvm_cmd() {
    if [ "$VMM_OWN_NETNS" = true ]; then
        VMM_CMD=(unshare --user --map-current-user --net -- "$VMM" "${VMM_ARGS[@]}")
    else
        VMM_CMD=("$VMM" "${VMM_ARGS[@]}")
    fi
}
