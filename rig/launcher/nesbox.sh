# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's nesbox: the window check, the config and the command
# line. (As root nesbox runs under its jailer: root.sh's jail_vmm.) Sourced by
# the launcher.

# nesbox before virtio-nvgpu-v4 publishes a 1 GiB window whatever the backend
# says, and every placement past it then fails on its own. v4 asks the backend
# (GET_SHMEM_CONFIG), and the string it names that request by is in the binary.
nesbox_check_window() {
    if [ "$WINDOW_MIB" != 1024 ] && ! grep -q VHOST_USER_GET_SHMEM_CONFIG "$VMM" 2>/dev/null; then
        die "NVGPU_WINDOW_MIB=$WINDOW_MIB: $VMM takes no window size from the backend;" \
            "build nesbox's virtio-nvgpu-v4 branch, or leave the window at 1024"
    fi
}

# The config, at $CFG. The schema is nesbox's vmm/src/config.rs, which
# refuses unknown keys. The mixed casing is its own: kebab-case sections,
# snake_case fields inside boot-source, drives and machine-config, kebab-case
# inside gpu-forward and shared-directories.
nesbox_config() {
    SHARES=
    if [ -n "$NVIDIA_SHARE" ]; then
        SHARES=",
  \"shared-directories\": [
    { \"tag\": \"nvidia\", \"path-on-host\": $(json_str "$NVIDIA_SHARE"), \"read-only\": true }
  ]"
    fi
    VMM_MARK=$CFG
    MACHINE_EXTRA=
    [ -z "$CPU_AFFINITY_J" ] || MACHINE_EXTRA+=", \"cpu_affinity\": [$CPU_AFFINITY_J]"
    [ -z "$VCPU_PINS_J" ] || MACHINE_EXTRA+=", \"vcpu_pins\": [$VCPU_PINS_J]"
    [ -z "$IO_AFFINITY_J" ] || MACHINE_EXTRA+=", \"io_affinity\": [$IO_AFFINITY_J]"
    [ -z "$HUGEPAGES" ] || MACHINE_EXTRA+=", \"hugepages\": \"$HUGEPAGES\""
    [ "$PREFAULT" != 0 ] || MACHINE_EXTRA+=", \"prefault\": false"
    cat > "$CFG" <<JSON
{
  "boot-source": {
    "kernel_image_path": $(json_str "$KERNEL"),
    "boot_args": $(json_str "$BOOT_ARGS")
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": $(json_str "$DISK"), "is_root_device": true, "is_read_only": false }
  ],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEM_MIB$MACHINE_EXTRA },
  "gpu-forward": { "socket": $(json_str "$SOCK") },
  "unshare-network": $VMM_OWN_NETNS$SHARES
}
JSON
}

# The config path is nesbox's one positional argument.
nesbox_cmd() {
    VMM_CMD=("${VMM_NETNS[@]}" "$VMM" "$CFG")
}
