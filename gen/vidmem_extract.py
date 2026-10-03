#!/usr/bin/env python3
"""Generate what a video memory limit needs to know about one RM release.

Two jobs need it. Counting: which allocations take video memory and how
big they are, from VID_HEAP_CONTROL's (NVOS32) blocks and the
NV01_MEMORY_LOCAL_USER allocation parameters. Telling: what the guest is
told the card holds, from FB_GET_INFO's index list and VID_HEAP_CONTROL
INFO -- with a limit, every one of those has to agree with the limit, or an
application computes negative free space.

    ./vidmem_extract.py --ogkm ~/forks/ogkm-615.71.09 --version 615.71.09 \\
        > src/vidmem/v615_71_09.rs

Nothing is transcribed. Index values, struct sizes, field offsets and the
attribute bit range are printed by a C probe compiled against the release's
own headers. The FB_INFO index numbers move between releases
(HEAP_RECLAIMABLE is 0x3c on 595.99.02, 0x44 on 615.71.09 and absent on
610), and a table carrying one release's numbers would clamp the wrong
field of another.

Every FB_INFO index whose name says it is a size, a free count or a heap
(SIZE, FREE, HEAP, RAM, _KB) has to be classified below, or this script
stops: a new release adding one must be read by a person before a clamp
silently ignores it. A classified index a release lacks is emitted as
absent, so the clamp skips it deliberately rather than writing field 0.

FB_GET_INFO itself (0x20801301) is in RM's deprecated-control table and is
rewritten by RM into FB_GET_INFO_V2 (0x20801303); a guest can send either,
and NVIDIA's userspace sends the first. Both layouts are emitted. V1 carries
its list behind a pointer, V2 inline, and V2's length is
NV2080_CTRL_FB_INFO_MAX_LIST_SIZE, which differs by release (0x36 on
535.129.03, 0x80 on 615.71.09).

Needs: python3, cc.
"""

import argparse
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile

INC = "src/common/sdk/nvidia/inc"
FB_H = os.path.join(INC, "ctrl/ctrl2080/ctrl2080fb.h")

# What each memory-reporting FB_INFO index is, by name. Values are read per
# release; only the meaning is written here, and it comes from the comments
# in ctrl2080fb.h. All are in KiB.
ROLES = {
    # How much video memory there is. With a limit, at most the limit.
    "RAM_SIZE": "Total",
    "TOTAL_RAM_SIZE": "Total",
    "HEAP_SIZE": "Total",
    "MAPPABLE_HEAP_SIZE": "Total",
    "USABLE_RAM_SIZE": "Total",
    # How much of it is free. With a limit, at most what the guest has left.
    "HEAP_FREE": "Free",
    "LARGEST_FREE_REGION_SIZE_KB": "Free",
    "HEAP_RECLAIMABLE": "Free",
    # Carve-outs RM keeps out of the heap. Not the guest's to spend, but they
    # are subtracted from a total somewhere, so a clamp has to decide.
    "VISTA_RESERVED_HEAP_SIZE": "Reserved",
    "FB_TAX_SIZE_KB": "Reserved",
    "HEAP_OFFLINE_SIZE": "Reserved",
    "SUSPEND_RESUME_RSVD_SIZE": "Reserved",
    "PROTECTED_MEM_SIZE_TOTAL_KB": "Reserved",
    "PROTECTED_MEM_SIZE_FREE_KB": "Reserved",
    # Where things are, not how much. A clamped heap can still start here.
    "HEAP_BASE_KB": "Position",
    "HEAP_START": "Position",
    "LARGEST_FREE_REGION_BASE_KB": "Position",
    # BAR1 is the CPU's window onto video memory, not video memory.
    "BAR1_SIZE": "Bar1",
    "BAR1_AVAIL_SIZE": "Bar1",
    "BAR1_MAX_CONTIGUOUS_AVAIL_SIZE": "Bar1",
    "SMOOTHDISP_RSVD_BAR1_SIZE": "Bar1",
}

# Names the guard below would catch that do not report video memory capacity.
NOT_MEMORY = {
    "TILE_REGION_COUNT",
    "TILE_REGION_FREE_COUNT",  # a count of tile regions
    "COMPRESSION_SIZE",
    "DRAM_PAGE_STRIDE",
    "RAM_CFG",
    "RAM_TYPE",
    "RAM_LOCATION",
    "L2CACHE_SIZE",
    "L2CACHE_ONLY_MODE",
    "ECC_STATUS_SIZE",  # a count of subpartitions or channels
    "P2P_MAILBOX_SIZE",
    "P2P_MAILBOX_ALIGNMENT",
    "P2P_MAILBOX_ALIGNMENT_SIZE",
    "P2P_MAILBOX_BAR1_MAX_OFFSET_64KB",
    "FORCED_BAR1_64KB_MAPPING_ENABLED",
    "IS_ZERO_FB",
    # Deprecated: the index is reused and RM answers 0.
    "GPU_VADDR_SPACE_SIZE_KB",
    "GPU_VADDR_HEAP_SIZE_KB",
    "GPU_VADDR_MAPPBLE_SIZE_KB",
}

GUARD = re.compile(r"SIZE|FREE|HEAP|RAM|_KB")

# The VID_HEAP_CONTROL functions that allocate, and the two that free.
NVOS32_FUNCTIONS = (
    "ALLOC_SIZE",
    "ALLOC_TILED_PITCH_HEIGHT",
    "ALLOC_SIZE_RANGE",
    "FREE",
    "INFO",
    "HW_ALLOC",
    "HW_FREE",
)
# Their blocks in NVOS32_PARAMETERS.data, for the three that allocate.
NVOS32_ALLOCS = (
    ("alloc_size", "AllocSize"),
    ("alloc_tiled_pitch_height", "AllocTiledPitchHeight"),
    ("alloc_size_range", "AllocSizeRange"),
)


def fb_indices(ogkm):
    """FB_INFO index names, in the block that ends at INDEX_MAX."""
    text = open(os.path.join(ogkm, FB_H)).read()
    end = re.search(r"#define\s+NV2080_CTRL_FB_INFO_INDEX_MAX\s", text)
    if not end:
        sys.exit("no NV2080_CTRL_FB_INFO_INDEX_MAX in ctrl2080fb.h")
    names = []
    for m in re.finditer(r"#define\s+NV2080_CTRL_FB_INFO_INDEX_(\w+)\s", text[: end.start()]):
        if m.group(1) not in names:
            names.append(m.group(1))
    return names


def check_classified(names):
    unknown = [n for n in names if GUARD.search(n) and n not in ROLES and n not in NOT_MEMORY]
    if unknown:
        sys.exit(
            "FB_INFO indices that may report video memory and are not classified "
            "in ROLES or NOT_MEMORY:\n  " + "\n  ".join(unknown)
        )


PROBE_HEAD = r"""
#include <stddef.h>
#include <stdio.h>
#include "nvtypes.h"
#include "nvos.h"
#include "ctrl/ctrl2080/ctrl2080fb.h"
#define S(k, v) printf("S %s %llu\n", k, (unsigned long long)(v))
int main(void) {
"""


def probe(names):
    out = [PROBE_HEAD]
    for n in names:
        out.append(f'  printf("I {n} %u\\n", (unsigned)(NV2080_CTRL_FB_INFO_INDEX_{n}));')
    for n in ROLES:
        if n not in names:
            out.append(f"#ifdef NV2080_CTRL_FB_INFO_INDEX_{n}")
            out.append(f'  printf("I {n} %u\\n", (unsigned)(NV2080_CTRL_FB_INFO_INDEX_{n}));')
            out.append("#endif")
    out += [
        '  S("cmd_v1", NV2080_CTRL_CMD_FB_GET_INFO);',
        '  S("cmd_v2", NV2080_CTRL_CMD_FB_GET_INFO_V2);',
        '  S("max_list", NV2080_CTRL_FB_INFO_MAX_LIST_SIZE);',
        '  S("entry_size", sizeof(NV2080_CTRL_FB_INFO));',
        '  S("entry_index", offsetof(NV2080_CTRL_FB_INFO, index));',
        '  S("entry_data", offsetof(NV2080_CTRL_FB_INFO, data));',
        '  S("v1_size", sizeof(NV2080_CTRL_FB_GET_INFO_PARAMS));',
        '  S("v1_count", offsetof(NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoListSize));',
        '  S("v1_list", offsetof(NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoList));',
        '  S("v2_size", sizeof(NV2080_CTRL_FB_GET_INFO_V2_PARAMS));',
        '  S("v2_count", offsetof(NV2080_CTRL_FB_GET_INFO_V2_PARAMS, fbInfoListSize));',
        '  S("v2_list", offsetof(NV2080_CTRL_FB_GET_INFO_V2_PARAMS, fbInfoList));',
        '  S("os32_size", sizeof(NVOS32_PARAMETERS));',
        '  S("os32_function", offsetof(NVOS32_PARAMETERS, function));',
        '  S("os32_status", offsetof(NVOS32_PARAMETERS, status));',
        '  S("os32_total", offsetof(NVOS32_PARAMETERS, total));',
        '  S("os32_free", offsetof(NVOS32_PARAMETERS, free));',
        '  S("os32_info_attr", offsetof(NVOS32_PARAMETERS, data.Info.attr));',
        '  S("os32_info_offset", offsetof(NVOS32_PARAMETERS, data.Info.offset));',
        '  S("os32_info_size", offsetof(NVOS32_PARAMETERS, data.Info.size));',
        '  S("os32_info_base", offsetof(NVOS32_PARAMETERS, data.Info.base));',
    ]
    for f in NVOS32_FUNCTIONS:
        out.append(f'  S("fn_{f.lower()}", NVOS32_FUNCTION_{f});')
    for key, blk in NVOS32_ALLOCS:
        for field in ("hMemory", "flags", "attr", "size", "limit"):
            out.append(f'  S("{key}_{field}", offsetof(NVOS32_PARAMETERS, data.{blk}.{field}));')
    out += [
        '  S("mem_size", sizeof(NV_MEMORY_ALLOCATION_PARAMS));',
        '  S("mem_flags", offsetof(NV_MEMORY_ALLOCATION_PARAMS, flags));',
        '  S("mem_attr", offsetof(NV_MEMORY_ALLOCATION_PARAMS, attr));',
        '  S("mem_bytes", offsetof(NV_MEMORY_ALLOCATION_PARAMS, size));',
        '  S("mem_limit", offsetof(NV_MEMORY_ALLOCATION_PARAMS, limit));',
        # DRF ranges are written hi:lo; the conditional picks each half.
        '  S("loc_lo", 0 ? NVOS32_ATTR_LOCATION);',
        '  S("loc_hi", 1 ? NVOS32_ATTR_LOCATION);',
        '  S("loc_vidmem", NVOS32_ATTR_LOCATION_VIDMEM);',
        '  S("flags_virtual", NVOS32_ALLOC_FLAGS_VIRTUAL);',
        "  return 0;\n}",
    ]
    return "\n".join(out)


def run_probe(ogkm, src, cc):
    incs = ["-I", os.path.join(ogkm, INC), "-I", os.path.join(ogkm, "src/common/inc")]
    with tempfile.TemporaryDirectory() as d:
        c = os.path.join(d, "vidmem.c")
        open(c, "w").write(src)
        exe = os.path.join(d, "vidmem")
        r = subprocess.run([cc, "-w", *incs, c, "-o", exe], capture_output=True, text=True)
        if r.returncode:
            sys.stderr.write(r.stderr[-4000:])
            sys.exit("the probe did not compile")
        lines = subprocess.run([exe], capture_output=True, text=True, check=True).stdout.split("\n")
    idx, s = {}, {}
    for ln in lines:
        f = ln.split()
        if not f:
            continue
        if f[0] == "I":
            idx[f[1]] = int(f[2])
        elif f[0] == "S":
            s[f[1]] = int(f[2])
    return idx, s


def emit(idx, s, version, stream):
    w = stream.write
    mod = "v" + version.replace(".", "_")
    w("// Generated by gen/vidmem_extract.py -- do not edit by hand.\n//\n")
    w(f"//   ./vidmem_extract.py --ogkm <open-gpu-kernel-modules at {version}> \\\n")
    w(f"//       --version {version} > src/vidmem/{mod}.rs\n//\n")
    w(f"// What a video memory limit needs to know about driver {version}.\n\n")
    w("use super::{Alloc, FbIndex, Layout, List, Role};\n\n")
    w("pub static LAYOUT: Layout = Layout {\n")
    w("    fb_info: &[\n")
    for name, role in ROLES.items():
        if name in idx:
            w(f'        FbIndex {{ name: "{name}", index: {idx[name]:#x}, role: Role::{role} }},\n')
    w("    ],\n")
    absent = [n for n in ROLES if n not in idx]
    w("    fb_absent: &[" + ", ".join(f'"{n}"' for n in absent) + "],\n")
    w(f"    cmd_fb_get_info: {s['cmd_v1']:#010x},\n")
    w(f"    cmd_fb_get_info_v2: {s['cmd_v2']:#010x},\n")
    w(f"    fb_info_max_list: {s['max_list']},\n")
    w(f"    fb_info_entry_size: {s['entry_size']},\n")
    w(f"    fb_info_entry_index: {s['entry_index']},\n")
    w(f"    fb_info_entry_data: {s['entry_data']},\n")
    for v in ("v1", "v2"):
        w(f"    fb_get_info_{v}: List {{ size: {s[v + '_size']}, count: {s[v + '_count']}, list: {s[v + '_list']} }},\n")
    w(f"    nvos32_size: {s['os32_size']},\n")
    for k in ("function", "status", "total", "free", "info_attr", "info_offset", "info_size", "info_base"):
        w(f"    nvos32_{k}: {s['os32_' + k]},\n")
    for f in NVOS32_FUNCTIONS:
        w(f"    nvos32_fn_{f.lower()}: {s['fn_' + f.lower()]},\n")
    for key, _ in NVOS32_ALLOCS:
        w(f"    nvos32_{key}: Alloc {{ ")
        w(", ".join(f"{fld.lower() if fld != 'hMemory' else 'h_memory'}: {s[key + '_' + fld]}"
                    for fld in ("hMemory", "flags", "attr", "size", "limit")))
        w(" },\n")
    w(f"    mem_alloc_size: {s['mem_size']},\n")
    w(f"    mem_alloc: Alloc {{ h_memory: u32::MAX, flags: {s['mem_flags']}, attr: {s['mem_attr']}, "
      f"size: {s['mem_bytes']}, limit: {s['mem_limit']} }},\n")
    w(f"    attr_location: ({s['loc_lo']}, {s['loc_hi']}),\n")
    w(f"    attr_location_vidmem: {s['loc_vidmem']:#x},\n")
    w(f"    alloc_flags_virtual: {s['flags_virtual']:#010x},\n")
    w("};\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ogkm", required=True, help="open-gpu-kernel-modules checkout at the release tag")
    ap.add_argument("--version", required=True)
    ap.add_argument("--cc", default=os.environ.get("CC", "cc"))
    a = ap.parse_args()

    names = fb_indices(a.ogkm)
    check_classified(names)
    idx, s = run_probe(a.ogkm, probe(names), a.cc)
    missing = [n for n in names if n not in idx]
    if missing:
        sys.exit("the probe printed no value for: " + ", ".join(missing))
    # Two names for one index would make a clamp write one field twice, or
    # one it did not mean to.
    seen = {}
    for n in ROLES:
        if n in idx:
            if idx[n] in seen:
                sys.exit(f"{n} and {seen[idx[n]]} are both index {idx[n]:#x}")
            seen[idx[n]] = n

    buf = io.StringIO()
    emit(idx, s, a.version, buf)
    text = buf.getvalue()
    fmt = shutil.which("rustfmt")
    if fmt:
        r = subprocess.run([fmt, "--edition", "2024"], input=text, capture_output=True, text=True)
        if r.returncode == 0:
            text = r.stdout
        else:
            sys.stderr.write("rustfmt refused this table; writing it unformatted\n")
    sys.stdout.write(text)
    absent = [n for n in ROLES if n not in idx]
    sys.stderr.write(
        f"{a.version}: {len(names)} FB_INFO indices, {len(ROLES) - len(absent)} memory-reporting "
        f"present, absent: {', '.join(absent) or 'none'}; V2 list {s['max_list']}\n"
    )


if __name__ == "__main__":
    main()
