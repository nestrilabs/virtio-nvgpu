#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The limits each VMM holds the backend's mapping requests to, against the
backend's own.

    scripts/vmm-parity.py [--crosvm DIR] [--nesbox DIR]

DIR is a source tree of that VMM (crosvm with patches/crosvm applied, nesbox
at its virtio-nvgpu branch), or only the files named below at their paths in
it. Each limit is read from each side's `const`, and every side given must
say the same; a VMM left out is not compared. The backend's side is this
repository's.

Each VMM keeps its own copy of these checks, in its own code (patches/
README.md, "Why each VMM carries its own checks"): this is what keeps the
copies saying the same thing.
"""

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# What is compared: a name, then where each side says it, as (file, const).
# None: that side has no such limit (the reason is in the note).
LIMITS = [
    (
        "UVM pool address band, lowest",
        ("protocol/src/messages.rs", "UVM_HVA_MIN"),
        ("vm_control/src/nvgpu.rs", "UVM_HVA_MIN"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_HVA_MIN"),
        None,
    ),
    (
        "UVM pool address band, end",
        ("protocol/src/messages.rs", "UVM_HVA_MAX"),
        ("vm_control/src/nvgpu.rs", "UVM_HVA_MAX"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_HVA_MAX"),
        None,
    ),
    (
        "largest UVM pool",
        ("device/src/uvmmap.rs", "MAX_LEN"),
        ("vm_control/src/nvgpu.rs", "UVM_POOL_MAX_LEN"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_MAX_LEN"),
        None,
    ),
    (
        "UVM pools placed at once",
        ("device/src/uvmmap.rs", "MAPS_PER_VM"),
        ("vm_control/src/nvgpu.rs", "UVM_MAX_POOLS"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_MAX_MAPPINGS"),
        None,
    ),
    (
        "UVM pool bytes placed at once",
        ("device/src/uvmmap.rs", "BYTES_PER_VM"),
        ("vm_control/src/nvgpu.rs", "UVM_MAX_POOL_BYTES"),
        None,
        "nesbox has none of its own: the backend's holds, and its 1 GiB aperture",
    ),
    (
        "UVM aperture",
        ("device/src/uvmmap.rs", "APERTURE_MAX"),
        ("vm_control/src/nvgpu.rs", "UVM_APERTURE_MAX"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_APERTURE_SIZE"),
        None,
    ),
    (
        "UVM aperture placement boundary",
        ("device/src/uvmmap.rs", "CHUNK"),
        ("vm_control/src/nvgpu.rs", "UVM_APERTURE_ALIGN"),
        ("virtio-devices/src/nvgpu/aperture.rs", "UVM_ALIGN"),
        None,
    ),
    (
        "UVM aperture's shared memory id",
        ("protocol/src/messages.rs", "SHM_ID_UVM"),
        ("vm_control/src/nvgpu.rs", "NVGPU_SHM_ID_UVM"),
        ("virtio-devices/src/nvgpu/aperture.rs", "NV_SHM_ID_UVM"),
        None,
    ),
    (
        "window placements at once",
        None,
        ("vm_control/src/sys/linux/nvgpu.rs", "WINDOW_MAX_MAPPINGS"),
        ("virtio-devices/src/nvgpu.rs", "WINDOW_MAX_PLACEMENTS"),
        "the two VMMs only: a count of mappings in their own address space",
    ),
    (
        "DRM character major",
        ("device/src/hostfd.rs", "DRM_MAJOR"),
        ("vm_control/src/sys/linux/nvgpu.rs", "DRM_MAJOR"),
        ("virtio-devices/src/nvgpu/fds.rs", "DRM_MAJOR"),
        None,
    ),
    (
        "NVIDIA character major",
        None,
        ("vm_control/src/sys/linux/nvgpu.rs", "NVIDIA_MAJOR"),
        ("virtio-devices/src/nvgpu/fds.rs", "NVIDIA_MAJOR"),
        "the backend has no constant of its own for it",
    ),
]

CONST = r"^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+{name}\s*:\s*[\w:]+\s*=\s*([^;]+);"
# What a limit's expression may be: numbers, shifts, sums and products.
EXPR = re.compile(r"^[0-9_ ()<>+*]+$")


def value(root, where):
    path, name = where
    try:
        text = (root / path).read_text()
    except OSError as e:
        raise SystemExit(f"vmm-parity: {root / path}: {e}")
    m = re.search(CONST.format(name=re.escape(name)), text, re.M)
    if not m:
        raise SystemExit(f"vmm-parity: {root / path}: no const {name}")
    expr = m.group(1).strip()
    if not EXPR.match(expr):
        raise SystemExit(f"vmm-parity: {root / path}: {name} = {expr}: not a plain number")
    return eval(expr.replace("_", ""), {"__builtins__": {}})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--crosvm", type=Path)
    ap.add_argument("--nesbox", type=Path)
    args = ap.parse_args()
    sides = [("backend", ROOT), ("crosvm", args.crosvm), ("nesbox", args.nesbox)]
    bad = 0
    for name, *wheres, note in LIMITS:
        seen = []
        for (side, root), where in zip(sides, wheres):
            if root is not None and where is not None:
                seen.append((side, value(root, where)))
        values = {v for _, v in seen}
        said = ", ".join(f"{s} {v:#x}" for s, v in seen)
        if len(values) > 1:
            print(f"DIFFERENT: {name}: {said}", file=sys.stderr)
            bad += 1
        else:
            print(f"same: {name}: {said}{f' ({note})' if note else ''}")
    sys.exit(1 if bad else 0)


main()
