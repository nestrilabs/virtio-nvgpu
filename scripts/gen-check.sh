#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Re-measure every checked-in table under gen/ from NVIDIA's (and gVisor's)
# sources, and fail if any is not what its extractor produces now.
#
# The unit tests (`cargo test -p abi`) only check that each generated file is
# what its checked-in JSON renders to; they cannot tell whether the JSON is
# still what the sources say, because that needs the sources. This does:
#
#   rmctrl_extract.py check   RM control pointers   (gen/rmctrl, nvgpu_rm_deep.h)
#   rmallow_extract.py check  the RM allowlist      (gen/rmallow)
#   nvkms_extract.py check    NVKMS/nvidia-drm       (gen/nvkms)
#   uvm_extract.py check      UVM blocks            (gen/uvm)
#   uvm_extract.py scan       every published tag against the UVM ranges
#   schema_gen.py             the IOCTL2 schema, regenerated and diffed
#   nvabi_gen.py              the ABI profiles, if GVISOR names a checkout
#
# Needs network access (it fetches open-gpu-kernel-modules tarballs from
# GitHub, a few hundred MB the first time, cached under GEN_CHECK_CACHE,
# default $TMPDIR/nvgpu-gen-check), python3 and gcc. Run it before a release
# and whenever NVIDIA publishes a driver the project supports; it is not part
# of `cargo test` because of the network.
#
# Usage: scripts/gen-check.sh              (GVISOR=~/forks/gvisor to include
#                                           the ABI profiles)
# Exit 0 when every table is current, 1 naming each that is not.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT/gen"
CACHE=${GEN_CHECK_CACHE:-${TMPDIR:-/tmp}/nvgpu-gen-check}
mkdir -p "$CACHE"

failed=()
step() {
    local name=$1
    shift
    echo "== $name" >&2
    if ! "$@"; then
        failed+=("$name")
    fi
}

step "rmctrl" python3 rmctrl_extract.py check --cache "$CACHE/rmctrl"
step "rmallow" python3 rmallow_extract.py check --cache "$CACHE/rmallow"
step "nvkms" python3 nvkms_extract.py --cache "$CACHE/nvkms" check
step "uvm" python3 uvm_extract.py --cache "$CACHE/uvm" check
step "uvm scan" python3 uvm_extract.py --cache "$CACHE/uvm" scan

schema_check() {
    local out
    out=$(mktemp -d)
    python3 schema_gen.py --out "$out" &&
        cmp -s "$out/gen/src/schema/generated.rs" src/schema/generated.rs &&
        cmp -s "$out/driver/gen/nvgpu_schema.h" ../driver/gen/nvgpu_schema.h
    local rc=$?
    rm -rf "$out"
    [ $rc = 0 ] || echo "gen/src/schema/generated.rs or driver/gen/nvgpu_schema.h is stale: run gen/schema_gen.py" >&2
    return $rc
}
step "schema" schema_check

if [ -n "${GVISOR:-}" ]; then
    # The table rows, not the header: the inheritance chain it names grows
    # as gVisor records releases in between.
    abi_check() {
        local rs v fresh rc=0
        for rs in src/versions/v*.rs; do
            v=$(basename "$rs" .rs)
            v=${v#v}
            v=${v//_/.}
            fresh=$(python3 nvabi_gen.py --gvisor "$GVISOR" --version "$v") || {
                rc=1
                continue
            }
            if [ "$(grep IoctlEntry <<<"$fresh")" != "$(grep IoctlEntry "$rs")" ]; then
                echo "$rs is not what nvabi_gen.py makes of $GVISOR" >&2
                rc=1
            fi
        done
        return $rc
    }
    step "abi profiles" abi_check
else
    echo "== abi profiles: skipped (GVISOR unset)" >&2
fi

if [ ${#failed[@]} -gt 0 ]; then
    echo "gen-check: stale or failing: ${failed[*]}" >&2
    exit 1
fi
echo "gen-check: every table is what its sources say" >&2
