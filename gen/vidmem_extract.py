#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""What the backend reads and rewrites to hold a VM to `--vram-limit`.

The backend counts the video memory a VM's RM calls allocate, refuses what
would take it past the limit, and rewrites the replies nvidia-smi, NVML,
Vulkan and CUDA size video memory from (device/src/vidmem.rs). Every field it
reads or writes for that is measured here, per release, from NVIDIA's SDK
headers at the tag, by compiling a probe: the memory classes' allocation
parameters, VID_HEAP_CONTROL's allocating, freeing and INFO functions,
ALLOC_MEMORY's limit, NV2080_CTRL_CMD_FB_GET_INFO's and _V2's lists, the OS_UNIX
export and import controls, and the constants those are read with (the
control and function numbers, the FB_INFO indices, the attribute bits).
Nothing is carried from one release to another: the backend uses the
layout of the host's exact release, and refuses `--vram-limit` on any
other.

    ./vidmem_extract.py all                # fetch + measure every release, render
    ./vidmem_extract.py extract 610.57.04  # one release (fetches if needed)
    ./vidmem_extract.py extract 595.99.02 --src ../../open-gpu-kernel-modules
    ./vidmem_extract.py render             # gen/vidmem/*.json -> gen/src/vidmem/generated.rs
    ./vidmem_extract.py check              # re-measure everything; fail if stale

`render` needs nothing but Python and is what the Rust staleness test runs.
`extract`, `all` and `check` need gcc and network access the first time;
the sources are the RM allowlist's (rmallow_extract.py fetches them, into
$RMALLOW_EXTRACT_CACHE, default $TMPDIR/ogkm-rmallow). x86_64 (LP64).
"""

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import rmallow_extract as ra

HERE = Path(__file__).resolve().parent
OUT_DIR = HERE / "vidmem"
RUST_OUT = HERE / "src" / "vidmem" / "generated.rs"
FORMAT = "virtio-nvgpu/vidmem/1"
VERSIONS = ra.VERSIONS

HEADERS = [
    "nvtypes.h", "nvmisc.h", "nvstatus.h", "nvos.h",
    "ctrl/ctrl2080/ctrl2080fb.h", "ctrl/ctrl0000/ctrl0000unix.h", "class/cl0040.h",
]

# (struct, fields): the offsets the backend reads and writes, and each
# struct's size. A field of a union member is named by its designator.
BLOCKS = {
    "NV_MEMORY_ALLOCATION_PARAMS": ["flags", "attr", "size"],
    "NVOS02_PARAMETERS": ["hRoot", "hClass", "limit", "status"],
    "NVOS32_PARAMETERS": [
        "hRoot", "hObjectParent", "function", "status", "total", "free",
        "data.AllocSize.hMemory", "data.AllocSize.flags", "data.AllocSize.attr",
        "data.AllocSize.size",
        "data.AllocSizeRange.hMemory", "data.AllocSizeRange.flags",
        "data.AllocSizeRange.attr", "data.AllocSizeRange.size",
        "data.AllocTiledPitchHeight.hMemory", "data.AllocTiledPitchHeight.flags",
        "data.AllocTiledPitchHeight.attr", "data.AllocTiledPitchHeight.size",
        "data.AllocTiledPitchHeight.height", "data.AllocTiledPitchHeight.pitch",
        "data.Free.hMemory", "data.HwFree.hResourceHandle", "data.Info.size",
    ],
    "NVOS54_PARAMETERS": ["hClient", "cmd", "paramsSize", "status"],
    "NVOS64_PARAMETERS": ["hRoot", "hObjectNew", "hClass", "status"],
    "NV2080_CTRL_FB_GET_INFO_PARAMS": ["fbInfoListSize", "fbInfoList"],
    "NV2080_CTRL_FB_GET_INFO_V2_PARAMS": ["fbInfoListSize", "fbInfoList"],
    "NV2080_CTRL_FB_INFO": ["index", "data"],
    "NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_PARAMS": [
        "object.type", "object.data.rmObject.hObject", "fd", "flags"],
    "NV0000_CTRL_OS_UNIX_IMPORT_OBJECT_FROM_FD_PARAMS": [
        "fd", "object.type", "object.data.rmObject.hParent", "object.data.rmObject.hObject"],
    "NV0000_CTRL_OS_UNIX_EXPORT_OBJECTS_TO_FD_PARAMS": [
        "fd", "objects", "numObjects", "index"],
    "NV0000_CTRL_OS_UNIX_IMPORT_OBJECTS_FROM_FD_PARAMS": [
        "fd", "hParent", "objects", "numObjects", "index"],
}

# Constants, as the probe evaluates them. Those in OPTIONAL may be missing
# from a release (null in its JSON).
CONSTANTS = [
    "NV_OK", "NV_ERR_NO_MEMORY",
    "NV01_MEMORY_LOCAL_USER",
    "NV2080_CTRL_CMD_FB_GET_INFO", "NV2080_CTRL_CMD_FB_GET_INFO_V2",
    "NV2080_CTRL_FB_INFO_MAX_LIST_SIZE",
    "NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE", "NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE",
    "NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE", "NV2080_CTRL_FB_INFO_INDEX_MAPPABLE_HEAP_SIZE",
    "NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE", "NV2080_CTRL_FB_INFO_INDEX_USABLE_RAM_SIZE",
    "NV2080_CTRL_FB_INFO_INDEX_LARGEST_FREE_REGION_SIZE_KB",
    "NV2080_CTRL_FB_INFO_INDEX_HEAP_RECLAIMABLE",
    "NVOS32_FUNCTION_ALLOC_SIZE", "NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT",
    "NVOS32_FUNCTION_ALLOC_SIZE_RANGE", "NVOS32_FUNCTION_FREE", "NVOS32_FUNCTION_INFO",
    "NVOS32_FUNCTION_HW_FREE",
    "NVOS32_ALLOC_FLAGS_VIRTUAL", "NVOS32_ATTR_LOCATION_VIDMEM",
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD",
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECTS_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECTS_FROM_FD",
    "NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TYPE_RM",
    "NV0000_CTRL_OS_UNIX_EXPORT_OBJECTS_TO_FD_MAX_OBJECTS",
    "NV0000_CTRL_OS_UNIX_IMPORT_OBJECTS_TO_FD_MAX_OBJECTS",
]
OPTIONAL = {"NV2080_CTRL_FB_INFO_INDEX_HEAP_RECLAIMABLE"}
# DRF fields, as (shift, mask) pairs: NAME_SHIFT and NAME_MASK.
DRF = ["NVOS32_ATTR_LOCATION", "NV0000_CTRL_OS_UNIX_EXPORT_OBJECT_TO_FD_FLAGS_EMPTY_FD"]


def probe(root, workdir):
    """({struct: {field: offset, "sizeof": n}}, {constant: value or None})."""
    inc = root / ra.SDK
    lines = ["#include <stdio.h>", "#include <stddef.h>"]
    lines += [f'#include "{h}"' for h in HEADERS]
    lines.append("int main(void) {")
    for s, fields in BLOCKS.items():
        lines.append(f'  printf("B {s} sizeof %zu\\n", sizeof({s}));')
        for f in fields:
            lines.append(f'  printf("B {s} {f} %zu\\n", offsetof({s}, {f}));')
    for c in CONSTANTS:
        guard = c in OPTIONAL
        if guard:
            lines.append(f"#ifdef {c}")
        lines.append(f'  printf("C {c} %llu\\n", (unsigned long long)({c}));')
        if guard:
            lines.append("#endif")
    for d in DRF:
        lines.append(f'  printf("C {d}_SHIFT %llu\\n", (unsigned long long)DRF_SHIFT({d}));')
        lines.append(f'  printf("C {d}_MASK %llu\\n", (unsigned long long)DRF_MASK({d}));')
    lines.append("  return 0;\n}")
    c = workdir / "probe.c"
    c.write_text("\n".join(lines) + "\n")
    exe = workdir / "probe"
    r = subprocess.run(["gcc", "-w", "-DNV_LINUX", "-o", str(exe), str(c)]
                       + [f"-I{root / d}" for d in ra.INCLUDE_DIRS] + [f"-I{inc}"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise ra.ExtractError("probe does not compile:\n" + r.stderr[-4000:])
    out = subprocess.run([str(exe)], capture_output=True, text=True, check=True).stdout
    blocks, consts = {}, {c: None for c in CONSTANTS}
    for line in out.splitlines():
        p = line.split()
        if p[0] == "B":
            blocks.setdefault(p[1], {})[p[2]] = int(p[3])
        elif p[0] == "C":
            consts[p[1]] = int(p[2])
    for c, v in consts.items():
        if v is None and c not in OPTIONAL:
            raise ra.ExtractError(f"probe did not evaluate {c}")
    return blocks, consts


def extract(version, root, source):
    with tempfile.TemporaryDirectory() as d:
        blocks, consts = probe(root, Path(d))
    for s, fields in BLOCKS.items():
        if s not in blocks or any(f not in blocks[s] for f in fields):
            raise ra.ExtractError(f"probe did not measure {s}")
    return {"format": FORMAT, "version": version, "abi": "x86_64 LP64", "source": source,
            "blocks": blocks, "constants": consts}


def load_all(out_dir=OUT_DIR):
    data = []
    for v in VERSIONS:
        p = out_dir / f"{v}.json"
        if not p.exists():
            raise ra.ExtractError(f"{p} missing: run {sys.argv[0]} extract {v}")
        data.append(json.loads(p.read_text()))
    return data


def snake(name):
    """nvos32 data.AllocSize.hMemory -> alloc_size_h_memory."""
    parts = name.split(".")
    parts = [p for p in parts[:-1] if p not in ("data", "object")] + parts[-1:]
    return "_".join(re.sub(r"(?<!^)([A-Z])", r"_\1", p).lower() for p in parts)


def short(struct):
    s = re.sub(r"_PARAM(ETER)?S$", "", struct)
    s = s.replace("NV0000_CTRL_OS_UNIX_", "UNIX_").replace("NV2080_CTRL_", "")
    return s.lower()


def fields_of():
    """[(rust field, kind, json path)] in a fixed order."""
    out = []
    for s, fields in BLOCKS.items():
        out.append((f"{short(s)}_sizeof", "usize", ("blocks", s, "sizeof")))
        for f in fields:
            out.append((f"{short(s)}_{snake(f)}", "usize", ("blocks", s, f)))
    for c in CONSTANTS:
        kind = "Option<u32>" if c in OPTIONAL else "u32"
        out.append((c.lower(), kind, ("constants", c)))
    for d in DRF:
        for sfx in ("SHIFT", "MASK"):
            out.append((f"{d}_{sfx}".lower(), "u32", ("constants", f"{d}_{sfx}")))
    return out


def render(data):
    rows = fields_of()
    names = [r[0] for r in rows]
    if len(set(names)) != len(names):
        raise ra.ExtractError("two measured fields render to one name")
    out = [
        "// SPDX-License-Identifier: Apache-2.0",
        "// @generated by gen/vidmem_extract.py render -- do not edit.",
        "//",
        "// What the backend reads and rewrites to hold a VM to --vram-limit",
        "// (device/src/vidmem.rs), measured per release from NVIDIA's SDK headers",
        "// (gen/vidmem/<version>.json).",
        "// Releases: " + ", ".join(d["version"] for d in data) + ".",
        "",
        "/// One release's layout: `*_sizeof` a struct's size, other `usize`s a",
        "/// field's offset in its struct, `u32`s the constants, `None` where the",
        "/// release lacks one.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct Layout {",
        "    pub version: (u32, u32, u32),",
    ]
    out += [f"    pub {n}: {k}," for n, k, _ in rows]
    out += ["}", "", "pub static LAYOUTS: &[Layout] = &["]
    for d in data:
        a, b, c = (int(x) for x in d["version"].split("."))
        out.append("    Layout {")
        out.append(f"        version: ({a}, {b}, {c}),")
        for n, k, path in rows:
            v = d[path[0]][path[1]] if len(path) == 2 else d[path[0]][path[1]][path[2]]
            if k == "Option<u32>":
                val = "None" if v is None else f"Some({v:#x})"
            elif k == "u32":
                val = f"{v:#x}"
            else:
                val = str(v)
            out.append(f"        {n}: {val},")
        out.append("    },")
    out.append("];")
    return "\n".join(out) + "\n"


def write_json(path, obj):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, indent=1) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract")
    e.add_argument("version")
    e.add_argument("--src", type=Path, help="a local open-gpu-kernel-modules tree")
    sub.add_parser("all")
    r = sub.add_parser("render")
    r.add_argument("--out", type=Path, help="write generated.rs here instead of gen/src/vidmem/")
    sub.add_parser("check")
    for p in (e, sub.choices["all"], sub.choices["check"]):
        p.add_argument("--cache", type=Path, default=ra.DEFAULT_CACHE)
    a = ap.parse_args()

    def measured(v):
        root = ra.fetch(v, a.cache)
        return extract(v, root, json.loads((root / "SOURCE.json").read_text()))

    try:
        if a.cmd == "extract":
            if a.src:
                root = a.src.resolve()
                write_json(OUT_DIR / f"{a.version}.json",
                           extract(a.version, root, ra.local_source(root)))
            else:
                a.cache.mkdir(parents=True, exist_ok=True)
                write_json(OUT_DIR / f"{a.version}.json", measured(a.version))
        elif a.cmd == "all":
            a.cache.mkdir(parents=True, exist_ok=True)
            for v in VERSIONS:
                write_json(OUT_DIR / f"{v}.json", measured(v))
            RUST_OUT.parent.mkdir(parents=True, exist_ok=True)
            RUST_OUT.write_text(render(load_all()))
        elif a.cmd == "render":
            text = render(load_all())
            dest = (a.out / "generated.rs") if a.out else RUST_OUT
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(text)
        elif a.cmd == "check":
            a.cache.mkdir(parents=True, exist_ok=True)
            stale = []
            for v in VERSIONS:
                p = OUT_DIR / f"{v}.json"
                if not p.exists() or json.loads(p.read_text()) != measured(v):
                    stale.append(str(p))
            if not RUST_OUT.exists() or RUST_OUT.read_text() != render(load_all()):
                stale.append(str(RUST_OUT))
            if stale:
                print("stale: " + ", ".join(stale) + f"; run {sys.argv[0]} all", file=sys.stderr)
                return 1
            print("gen/vidmem and gen/src/vidmem/generated.rs are up to date")
    except ra.ExtractError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
