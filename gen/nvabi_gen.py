#!/usr/bin/env python3
"""Generate a virtio-nvgpu ABI profile from gVisor's nvproxy.

nvproxy records each NVIDIA driver version as a delta against its parent, so
the ABI for one version is the base map plus every override along its
inheritance chain. This script walks that chain, resolves the parameter struct
for each frontend escape, computes its size with nvabi_sizes, and emits a Rust
table.

    ./nvabi_gen.py --gvisor ~/forks/gvisor --version 580.178.04 \
        > src/versions/v580_178_04.rs

The struct half is mechanical. The judgement half -- which escapes exist and
how each must be handled -- is nvproxy's, which is why this reads nvproxy
rather than restating it.
"""

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from nvabi_sizes import load, layout  # noqa: E402

# nvproxy handler function -> our IoctlKind.
KIND = {
    "frontendIoctlSimple": "Simple",
    "frontendIoctlSimpleNoStatus": "Simple",
    "frontendIoctlBytes": "Bytes",
    "frontendRegisterFD": "FdCarrying",
    "frontendIoctlHasFD": "FdCarrying",
    "frontendExportToDMABufFD": "FdCarrying",
    "rmMapMemory": "Mapping",
    "rmControl": "RmControl",
    "rmAlloc": "RmAlloc",
    "rmAllocMemory": "FdCarrying",
    "rmAllocContextDMA2": "Simple",
    "rmFree": "Simple",
    "rmDupObject": "Simple",
    "rmVidHeapControl": "VidHeapControl",
    "rmIdleChannels": "IdleChannels",
    "rmNumaInfo": "Bytes",
}

VER_RE = re.compile(
    r"(?:(v\d+_\d+_\d+)\s*:=\s*|_\s*=\s*)add(?:Unsupported)?DriverABI\("
    r"\s*(\d+),\s*(\d+),\s*(\d+),"
)
PARENT_CALL_RE = re.compile(r"abi\s*:=\s*(v\d+_\d+_\d+)\(\)")
FE_HANDLER_RE = re.compile(
    r"(?:nvgpu\.(NV_ESC_\w+):\s*feHandler\(|abi\.frontendIoctl\[nvgpu\.(NV_ESC_\w+)\]\s*=\s*feHandler\()"
    r"\s*(\w+)(?:\[nvgpu\.(\w+)\])?"
)
FE_INFO_RE = re.compile(
    r"(?:nvgpu\.(NV_ESC_\w+):\s*|info\.FrontendInfos\[nvgpu\.(NV_ESC_\w+)\]\s*=\s*)"
    r"(ioctlInfoWithStructName|ioctlInfo|simpleIoctlInfo)\(\s*\"NV_ESC_\w+\"((?:[^()]|\([^()]*\))*)\)"
)
ESC_DEF_RE = re.compile(r"^\s*(NV_ESC_\w+)\s*=\s*(0[xX][0-9a-fA-F]+|\d+)", re.M)
# Frontend escapes are defined relative to NV_IOCTL_BASE rather than literally.
ESC_BASE_RE = re.compile(r"^\s*(NV_ESC_\w+)\s*=\s*NV_IOCTL_BASE\s*\+\s*(\d+)", re.M)


def split_blocks(text):
    """Yield (varname, (maj,min,patch), block_text) for each addDriverABI call."""
    marks = [(m.start(), m) for m in VER_RE.finditer(text)]
    for i, (pos, m) in enumerate(marks):
        end = marks[i + 1][0] if i + 1 < len(marks) else len(text)
        name = m.group(1) or f"_v{m.group(2)}_{m.group(3)}_{m.group(4)}"
        ver = (int(m.group(2)), int(m.group(3)), int(m.group(4)))
        yield name, ver, text[pos:end]


def build(gvisor: Path, target):
    vg = (gvisor / "pkg" / "sentry" / "devices" / "nvproxy" / "version.go").read_text()

    escapes = {}
    for f in ("frontend.go", "nvgpu.go"):
        p = gvisor / "pkg" / "abi" / "nvgpu" / f
        if p.exists():
            body = p.read_text()
            for m in ESC_DEF_RE.finditer(body):
                escapes.setdefault(m.group(1), int(m.group(2), 0))
            bm = re.search(r"^\s*NV_IOCTL_BASE\s*=\s*(\d+)", body, re.M)
            if bm:
                base = int(bm.group(1))
                for m in ESC_BASE_RE.finditer(body):
                    escapes.setdefault(m.group(1), base + int(m.group(2)))

    blocks, order = {}, []
    for name, ver, body in split_blocks(vg):
        pm = PARENT_CALL_RE.search(body)
        parent = pm.group(1) if pm else None
        if parent is None:
            # parent passed as the trailing argument, e.g. addDriverABI(..., v580_167_08)
            tail = re.search(r",\s*(v\d+_\d+_\d+)\)\s*$", body.strip().splitlines()[0].strip())
            if not tail:
                tail = re.search(r",\s*(v\d+_\d+_\d+)\)", body)
            parent = tail.group(1) if tail else None
        blocks[name] = dict(ver=ver, parent=parent, body=body)
        order.append(name)

    match = [n for n in order if blocks[n]["ver"] == target]
    if not match:
        sys.exit(f"driver {target} not present in this gVisor checkout")
    chain, cur = [], match[0]
    while cur:
        chain.append(cur)
        cur = blocks[cur]["parent"]
    chain.reverse()

    handlers, structs_by_esc = {}, {}
    for name in chain:
        body = blocks[name]["body"]
        for m in FE_HANDLER_RE.finditer(body):
            esc = m.group(1) or m.group(2)
            handlers[esc] = (m.group(3), m.group(4))
        for m in FE_INFO_RE.finditer(body):
            esc = m.group(1) or m.group(2)
            args = m.group(4) or ""
            sm = re.findall(r"nvgpu\.(\w+)\{\}", args)
            structs_by_esc[esc] = sm[-1] if sm else None
    return chain, handlers, structs_by_esc, escapes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gvisor", required=True, type=Path)
    ap.add_argument("--version", required=True)
    args = ap.parse_args()

    target = tuple(int(x) for x in args.version.split("."))
    chain, handlers, structs_by_esc, escapes = build(args.gvisor, target)
    gs, consts = load(args.gvisor)

    rows = []
    for esc, (fn, tparam) in sorted(handlers.items(), key=lambda kv: escapes.get(kv[0], 1 << 30)):
        num = escapes.get(esc)
        if num is None:
            print(f"warning: no number for {esc}", file=sys.stderr)
            continue
        kind = KIND.get(fn)
        if kind is None:
            print(f"warning: unmapped handler {fn} for {esc}", file=sys.stderr)
            continue
        st = tparam or structs_by_esc.get(esc)
        size = None
        if st:
            try:
                size = layout(st, gs, consts)[0]
            except (KeyError, ValueError) as e:
                print(f"warning: {esc}: {e}", file=sys.stderr)
        rows.append((esc, num, kind, st, size))

    maj, mnr, pat = target
    print(f"""// Generated by gen/nvabi_gen.py -- do not edit by hand.
//
//   ./nvabi_gen.py --gvisor <gvisor> --version {args.version} \\
//       > src/versions/v{maj}_{mnr}_{pat:02}.rs
//
// ABI profile for NVIDIA driver {args.version}.
//
// Derived from gVisor's nvproxy (Apache-2.0), which records each driver
// version as a delta against its parent. The inheritance chain resolved for
// this version was:
//
//   {' -> '.join(chain)}
//
// Struct sizes are computed from gVisor's pkg/abi/nvgpu declarations, not
// transcribed. Sizes marked `None` are variable-length ioctls with no fixed
// parameter struct.

use crate::ioctl::*;
use super::{{IoctlEntry, IoctlKind}};

/// Build the ioctl table for {args.version}.
pub fn table() -> &'static [IoctlEntry] {{
    static TABLE: &[IoctlEntry] = &[""")
    for esc, num, kind, st, size in rows:
        # A Bytes ioctl carries a variable-length array; the struct size we can
        # compute is one element, not the parameter buffer, so do not claim it.
        if kind == "Bytes":
            note = f"{st} array, variable length" if st else "variable length"
            size = None
        else:
            note = st or "variable length"
        sz = "None" if size is None else f"Some({size})"
        print(f"        // {note}")
        print(f"        IoctlEntry {{ escape: {esc}, param_size: {sz}, kind: IoctlKind::{kind} }},")
    print("    ];\n    TABLE\n}")


if __name__ == "__main__":
    main()
