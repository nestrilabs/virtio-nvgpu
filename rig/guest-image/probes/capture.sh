#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Capture injection (SECURITY.md §18): host buffers the host's helper injected
# are opened here through /dev/nvgpu-capture, imported into EGL and Vulkan,
# and every pixel checked against the pattern the host painted; the CPU gets
# no writable mapping; a wrong token, a released id and a process without
# the node's group get nothing. Run as
#
#   NVGPU_BEFORE_VMM=rig/rig-tools/inject-hook.sh rig/run-guest.sh --inject capture cap1
#
# whose hook puts the helper's ids and tokens on the command line:
#   nvgpu_cap=ID:TOKEN:FRAME:FNV,...  nvgpu_cap_live=ID:TOKEN
#   nvgpu_cap_released=ID:TOKEN       nvgpu_cap_size=WxH
. /opt/nvgpu/probe-common.sh
probe_init capture 240

IMPORT=/opt/nvgpu/bin/nvgpu-capture-import

if load_module; then
    section "the node"
    if [ -c /dev/nvgpu-capture ]; then
        pass "/dev/nvgpu-capture: $(stat -c '%A %U:%G' /dev/nvgpu-capture)"
        mode=$(stat -c '%a' /dev/nvgpu-capture)
        if [ $((8#$mode & 8#007)) -eq 0 ]; then
            pass "nothing for other (mode $mode)"
        else
            fail "mode $mode gives other access"
        fi
        # A process without the node's group opens nothing, token or not.
        if setpriv --reuid=1000 --regid=1000 --clear-groups sh -c 'exec 3<>/dev/nvgpu-capture' 2>/dev/null; then
            fail "uid 1000 without the group opened /dev/nvgpu-capture"
        else
            pass "uid 1000 without the group cannot open /dev/nvgpu-capture"
        fi
    else
        fail "/dev/nvgpu-capture missing (backend without --inject-socket?): $(dmesg_nvgpu | grep -i capture)"
    fi

    caps=$(arg cap)
    if [ -z "$caps" ]; then
        fail "no nvgpu_cap= on the command line (run with the inject hook)"
    fi
    first_id= first_tok=
    IFS=, read -r -a bufs <<<"$caps"
    for b in "${bufs[@]}"; do
        IFS=: read -r id tok frame fnv <<<"$b"
        [ -z "$first_id" ] && first_id=$id first_tok=$tok
        section "buffer id $id (frame $frame)"
        step "EGL and Vulkan import of id $id, pixels and read-only mappings" 60 \
            "$IMPORT" --id "$id" --token "$tok" --frame "$frame" --fnv "$fnv"
    done

    section "refusals"
    if [ -n "$first_id" ]; then
        # The token's last digit changed: nothing opens, and the answer is
        # the one an id that does not exist gets.
        last=${first_tok: -1}
        case $last in f) new=0 ;; *) new=f ;; esac
        step "id $first_id with a wrong token: ENOENT" 20 \
            "$IMPORT" --id "$first_id" --token "${first_tok%?}$new" --expect-errno 2
        step "id 999999 (none): ENOENT" 20 \
            "$IMPORT" --id 999999 --token "$first_tok" --expect-errno 2
    fi
    rel=$(arg cap_released)
    if [ -n "$rel" ]; then
        step "a released id: ENOENT" 20 \
            "$IMPORT" --id "${rel%%:*}" --token "${rel#*:}" --expect-errno 2
    else
        skip "no nvgpu_cap_released="
    fi

    section "live"
    live=$(arg cap_live)
    if [ -n "$live" ]; then
        step "the live buffer moves on without a new open (Vulkan)" 60 \
            "$IMPORT" --id "${live%%:*}" --token "${live#*:}" --no-egl --watch 500
    else
        skip "no nvgpu_cap_live="
    fi

    section "guest log"
    dmesg_nvgpu | grep -i capture | sed 's/^/    /'
fi
finish
