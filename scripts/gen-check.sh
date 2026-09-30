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
#   vidmem_extract.py check   --vram-limit's fields (gen/vidmem)
#   nvkms_extract.py check    NVKMS/nvidia-drm       (gen/nvkms)
#   uvm_extract.py check      UVM blocks            (gen/uvm)
#   uvm_extract.py scan       every published tag against the UVM ranges
#   schema_gen.py             the IOCTL2 schema, regenerated and diffed
#   nvgpu_gen.py              the guest's RM_ALLOC class sizes and V1->V2
#                             rewrites (driver/gen), from 595.58.03
#   nvabi_gen.py              the ABI profiles, if GVISOR names a checkout
#                             (GVISOR=fetch clones gVisor's master into the cache)
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
step "vidmem" python3 vidmem_extract.py check --cache "$CACHE/rmallow"
step "nvkms" python3 nvkms_extract.py --cache "$CACHE/nvkms" check
step "uvm" python3 uvm_extract.py --cache "$CACHE/uvm" check
step "uvm scan" python3 uvm_extract.py --cache "$CACHE/uvm" scan
step "nvabi_sizes self-test" python3 nvabi_sizes.py --self-test

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

# driver/gen's two nvgpu_gen.py headers, from the whole source tree of the
# release they were made from (the probes it compiles include much of it).
NVGPU_GEN_RELEASE=595.58.03
nvgpu_gen_check() {
    local src=$CACHE/nvgpu-gen/$NVGPU_GEN_RELEASE out rc=0 f
    if [ ! -d "$src/src" ]; then
        mkdir -p "$CACHE/nvgpu-gen"
        python3 - "$NVGPU_GEN_RELEASE" "$src" <<'PY' || return 1
import sys, tarfile, urllib.request, shutil, pathlib
tag, dest = sys.argv[1], pathlib.Path(sys.argv[2])
tmp = dest.with_name(dest.name + ".partial")
shutil.rmtree(tmp, ignore_errors=True)
url = f"https://codeload.github.com/NVIDIA/open-gpu-kernel-modules/tar.gz/refs/tags/{tag}"
with urllib.request.urlopen(url, timeout=300) as r, tarfile.open(fileobj=r, mode="r|gz") as tf:
    for m in tf:
        parts = m.name.split("/", 1)
        if len(parts) < 2 or not (m.isfile() or m.isdir()):
            continue
        target = tmp / parts[1]
        if m.isdir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        with tf.extractfile(m) as f:
            target.write_bytes(f.read())
tmp.rename(dest)
PY
    fi
    out=$(mktemp -d)
    if ! python3 nvgpu_gen.py --src "$src" --out "$out" --version "$NVGPU_GEN_RELEASE" >"$out/log" 2>&1; then
        cat "$out/log" >&2
        rm -rf "$out"
        return 1
    fi
    for f in nvgpu_rmalloc_classes.h nvgpu_v1v2_rewrites.h; do
        if ! cmp -s "$out/$f" "../driver/gen/$f"; then
            echo "driver/gen/$f is not what nvgpu_gen.py makes of $NVGPU_GEN_RELEASE" >&2
            rc=1
        fi
    done
    rm -rf "$out"
    return $rc
}
step "nvgpu_gen (driver/gen)" nvgpu_gen_check

if [ "${GVISOR:-}" = fetch ]; then
    GVISOR=$CACHE/gvisor
    if [ -d "$GVISOR/.git" ]; then
        git -C "$GVISOR" fetch -q --depth 1 origin master && git -C "$GVISOR" reset -q --hard FETCH_HEAD
    else
        git clone -q --depth 1 https://github.com/google/gvisor "$GVISOR"
    fi
fi
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
