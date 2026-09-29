# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# What the rig's host scripts share: rig-bench.sh, rig-framepace.sh,
# rig-native-run.sh and rig-tools/inject-hook.sh source it. Nothing here runs
# on its own, and nothing is for root: rig/run-guest.sh has pieces of its
# own (rig/launcher/), which a root run checks before it reads them.

RIG_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# rig_phys PATH: where a store path's bytes are -- in the chroot store (nix
# without root, under ~/.local/share/nix/root) when there is one, else PATH.
rig_phys() {
    local b
    for b in "$HOME/.local/share/nix/root" ""; do
        [ -e "$b$1" ] && { echo "$b$1"; return; }
    done
    echo "$1"
}

# rig_nv_env OD: NV_ENV, the variables that pin every loader to the NVIDIA
# userspace at OD (the guest image's opengl-driver), as the guest's
# probes/env.sh pins them, and its libraries first.
rig_nv_env() {
    # shellcheck disable=SC2034 # the caller's
    NV_ENV=(
        "LD_LIBRARY_PATH=$1/lib"
        "VK_DRIVER_FILES=$1/share/vulkan/icd.d/nvidia_icd.json"
        "__EGL_VENDOR_LIBRARY_FILENAMES=$1/share/glvnd/egl_vendor.d/10_nvidia.json"
        "__GLX_VENDOR_LIBRARY_NAME=nvidia"
        "GBM_BACKENDS_PATH=$1/lib/gbm"
        "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=$1/share/egl/egl_external_platform.d"
    )
}

# rig_backend_pid, rig_vmm_pid: the newest backend and VMM of this user's, as
# rig/run-guest.sh starts them (the backend's binary, then --socket; nesbox
# or crosvm by name): not another VM's, nor an editor's with the path in its
# arguments.
rig_backend_pid() {
    pgrep -n -u "$(id -u)" -f '^[^ ]*/vhost-user-nvgpu --socket '
}
rig_vmm_pid() {
    pgrep -n -u "$(id -u)" -x nesbox || pgrep -n -u "$(id -u)" -x crosvm
}

# rig_run_vm TAG SOCKET SECS GUEST_CMD [BACKEND_WORD...]: the `run` probe
# under rig/run-guest.sh, against the compositor at SOCKET, running the shell
# command GUEST_CMD (nvgpu_cmd) with SECS to do it in before the guest's own
# watchdog, and 90 before the launcher's (unless NVGPU_TIMEOUT says). The
# words go to the backend. Every other knob of run-guest.sh is the
# environment's.
rig_run_vm() {
    local tag=$1 sock=$2 secs=$3 gcmd=$4
    shift 4
    NVGPU_TIMEOUT=${NVGPU_TIMEOUT:-$((secs + 90))} \
        NVGPU_CMDLINE_EXTRA="nvgpu_wl=1 nvgpu_timeout=$((secs + 60)) nvgpu_cmd=$(printf %s "$gcmd" | base64 -w0) ${NVGPU_CMDLINE_EXTRA:-}" \
        "$RIG_REPO/rig/run-guest.sh" --wayland-socket "$sock" run "$tag" ${1+-- "$@"}
}

# rig_pacing CONSOLE BACKEND_LOG MARK: both sides' pacing counters -- the
# guest driver's, printed between MARK_BEGIN and MARK_END on the console, and
# the backend's teardown report (device::pacing).
rig_pacing() {
    tr -d '\r' <"$1" | sed -n "/^${3}_BEGIN/,/^${3}_END/p" | sed '1d;$d' | sed 's/^/guest: /'
    grep -a 'pacing:' "$2" | sed 's/^.*\] //'
}
