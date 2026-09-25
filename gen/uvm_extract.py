#!/usr/bin/env python3
"""Measure the parameter block of every UVM command the backend lets through.

nvidia-uvm's ioctl numbers carry no size: UVM_IOCTL_BASE(n) is plain n, and
UVM_INITIALIZE/DEINITIALIZE are 0x30000001/2, whose _IOC_SIZE of 0x3000 is
not the size of anything. The kernel copies exactly sizeof(<cmd>_PARAMS) in
and out (uvm_api.h, __UVM_ROUTE_CMD_STACK/_ALLOC), so a forwarder has to know
that size per command and per release, or it copies too little (the host
reads past the backend's buffer, and writes past it on the way back) or too
much (stale bytes written past the caller's struct, 12 KiB of them when the
guest used 0x3000 as a bound). This script is where both sides' table comes
from.

For one driver release it:

1. fetches the UVM ioctl headers (kernel-open/nvidia-uvm/uvm_linux_ioctl.h,
   uvm_ioctl.h, uvm_types.h) and everything they include from the SDK, file
   by file from the release's tag;
2. compiles and runs a probe against them for every command in COMMANDS:
   whether the release has it (its macro is defined), the number it takes,
   sizeof its _PARAMS, and for the commands that name another open file,
   the offset and width of that descriptor (device/src/uvmfd.rs);
3. writes gen/uvm/<release>.json. Nothing is transcribed.

    ./uvm_extract.py all                  # every release in VERSIONS
    ./uvm_extract.py extract 610.57.04    # one release (fetches if needed)
    ./uvm_extract.py extract 610.57.04 --src ../nvidia-driver
    ./uvm_extract.py check                # re-measure VERSIONS; fail if stale
    ./uvm_extract.py scan                 # every tag since the first release

`scan` measures every tag of NVIDIA/open-gpu-kernel-modules from the oldest
release in VERSIONS on, and fails if any tag's table differs from the table
of the newest release in VERSIONS at or below it. That is what makes the
ranges gen/schema_gen.py builds from these files exact rather than a guess:
a release between two measured ones takes the older one's table, which is
only right if nothing changed in between. Each release in VERSIONS that is
there for that reason says so.

The tables are rendered into driver/gen/nvgpu_schema.h and
gen/src/schema/generated.rs by gen/schema_gen.py (gen/schema/uvm.py loads
these files). Needs Python 3.8+, gcc, and network access the first time
(sources are cached in $UVM_EXTRACT_CACHE, default $TMPDIR/ogkm-uvm;
--cache overrides). x86_64 (LP64).
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT_DIR = HERE / "uvm"
REPO = "NVIDIA/open-gpu-kernel-modules"
FORMAT = "virtio-nvgpu/uvm-params/1"
DEFAULT_CACHE = Path(os.environ.get("UVM_EXTRACT_CACHE",
                                    Path(tempfile.gettempdir()) / "ogkm-uvm"))

# The releases measured, and why each is here. The first six are the ABI
# profiles and gen/nvkms's set; the rest are where `scan` found a size, or a
# command, change between two of them.
VERSIONS = {
    "535.129.03": "ABI profile",
    "550.40.53": "UVM_MAX_GPUS_V2 (256): MAP_EXTERNAL_ALLOCATION and "
                 "ALLOC_SEMAPHORE_POOL grow",
    "565.57.01": "ALLOC_DEVICE_P2P and CLEAR_ALL_ACCESS_COUNTERS appear",
    "580.65.06": "DISCARD appears",
    "580.178.04": "ABI profile",
    "590.44.01": "FREE loses length, UNREGISTER_CHANNEL loses gpuUuid",
    "595.71.05": "ABI profile",
    "595.99.02": "ABI profile",
    "610.43.02": "UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS appears, range groups go",
    "610.57.04": "ABI profile; nvidia-driver/",
    "615.71.09": "ABI profile",
}

# The UVM commands the backend lets through (device/src/guestptr.rs,
# UVM_ALLOWED; a Rust test holds the two lists equal), and for those that
# name another open file, the descriptor field and what it must name.
COMMANDS = [
    ("INITIALIZE", None, None),
    ("DEINITIALIZE", None, None),
    ("REGISTER_GPU_VASPACE", "rmCtrlFd", "rmctl"),
    ("UNREGISTER_GPU_VASPACE", None, None),
    ("REGISTER_CHANNEL", "rmCtrlFd", "rmctl"),
    ("UNREGISTER_CHANNEL", None, None),
    ("ENABLE_PEER_ACCESS", None, None),
    ("DISABLE_PEER_ACCESS", None, None),
    ("MAP_EXTERNAL_ALLOCATION", "rmCtrlFd", "rmctl"),
    ("FREE", None, None),
    ("REGISTER_GPU", "rmCtrlFd", "rmctl"),
    ("UNREGISTER_GPU", None, None),
    ("PAGEABLE_MEM_ACCESS", None, None),
    # Range groups, gone in 610.43.02; flat blocks of range group ids and
    # UVM's own managed ranges (uvm_range_group.c).
    ("CREATE_RANGE_GROUP", None, None),
    ("DESTROY_RANGE_GROUP", None, None),
    ("SET_RANGE_GROUP", None, None),
    ("PREVENT_MIGRATION_RANGE_GROUPS", None, None),
    ("ALLOW_MIGRATION_RANGE_GROUPS", None, None),
    ("MIGRATE_RANGE_GROUP", None, None),
    ("SET_PREFERRED_LOCATION", None, None),
    ("UNSET_PREFERRED_LOCATION", None, None),
    ("ENABLE_READ_DUPLICATION", None, None),
    ("DISABLE_READ_DUPLICATION", None, None),
    ("SET_ACCESSED_BY", None, None),
    ("UNSET_ACCESSED_BY", None, None),
    ("MIGRATE", None, None),
    ("MAP_DYNAMIC_PARALLELISM_REGION", None, None),
    ("UNMAP_EXTERNAL", None, None),
    ("TOOLS_FLUSH_EVENTS", None, None),
    ("ALLOC_SEMAPHORE_POOL", None, None),
    ("CLEAN_UP_ZOMBIE_RESOURCES", None, None),
    ("PAGEABLE_MEM_ACCESS_ON_GPU", None, None),
    ("VALIDATE_VA_RANGE", None, None),
    ("CREATE_EXTERNAL_RANGE", None, None),
    ("MAP_EXTERNAL_SPARSE", None, None),
    ("MM_INITIALIZE", "uvmFd", "uvm"),
    ("ALLOC_DEVICE_P2P", "rmCtrlFd", "rmctl"),
    ("CLEAR_ALL_ACCESS_COUNTERS", None, None),
    ("DISCARD", None, None),
]

# UVM_DEINITIALIZE takes no argument (uvm.c: `case UVM_DEINITIALIZE: return 0`).
NO_PARAMS = {"DEINITIALIZE"}

ROOT_HEADER = "kernel-open/nvidia-uvm/uvm_linux_ioctl.h"
INCLUDE_DIRS = [
    "kernel-open/nvidia-uvm",
    "kernel-open/common/inc",
    "src/common/sdk/nvidia/inc",
    "src/common/inc",
]
INCLUDE = re.compile(r'^\s*#\s*include\s+"([^"]+)"', re.M)


class ExtractError(Exception):
    """A header, command or field is not what the extractor expects."""


def version_key(v):
    parts = [int(x) for x in v.split(".")]
    return tuple(parts + [0] * (3 - len(parts)))


# --------------------------------------------------------------------------
# Fetching
# --------------------------------------------------------------------------

def _get(url):
    try:
        with urllib.request.urlopen(url, timeout=60) as r:
            return r.read()
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise ExtractError(f"{url}: {e}") from e


def fetch(version, cache):
    """The UVM headers of `version` and their include closure, under
    cache/<version>, as the tree lays them out."""
    dest = cache / version
    if (dest / "SOURCE.json").exists():
        return dest
    base = f"https://raw.githubusercontent.com/{REPO}/{version}/"
    root = _get(base + ROOT_HEADER)
    if root is None:
        raise ExtractError(f"{base + ROOT_HEADER}: not found")
    files, todo, missing = {ROOT_HEADER: root}, [ROOT_HEADER], set()
    while todo:
        text = files[todo.pop()].decode("latin-1")
        for inc in INCLUDE.findall(text):
            if inc in missing or any(r.endswith("/" + inc) for r in files):
                continue
            for d in INCLUDE_DIRS:
                got = _get(f"{base}{d}/{inc}")
                if got is not None:
                    files[f"{d}/{inc}"] = got
                    todo.append(f"{d}/{inc}")
                    break
            else:
                # A system header, or one a configuration never includes.
                missing.add(inc)
    for rel, data in files.items():
        p = dest / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
    (dest / "SOURCE.json").write_text(json.dumps({
        "tag": version, "url": base,
        "files": {rel: hashlib.sha256(files[rel]).hexdigest()
                  for rel in sorted(files)},
    }, indent=2) + "\n")
    return dest


def source_of(root):
    p = root / "SOURCE.json"
    if p.exists():
        return json.loads(p.read_text())
    return {"tag": None, "url": None, "files": None, "local": True}


# --------------------------------------------------------------------------
# Measuring
# --------------------------------------------------------------------------

def probe_source():
    out = ["#include <stddef.h>", "#include <stdio.h>",
           f'#include "{os.path.basename(ROOT_HEADER)}"',
           "int main(void) {", '  const char *sep = "";', '  printf("[");']
    for name, fd, _ in COMMANDS:
        m = f"UVM_{name}"
        out.append(f"#ifdef {m}")
        if name in NO_PARAMS:
            size, off, width = "0", "-1", "0"
        else:
            p = f"{m}_PARAMS"
            size = f"(long)sizeof({p})"
            off = f"(long)offsetof({p}, {fd})" if fd else "-1"
            width = f"(long)sizeof((({p} *)0)->{fd})" if fd else "0"
        out.append(
            f'  printf("%s{{\\"name\\": \\"{name}\\", \\"cmd\\": %lu, '
            f'\\"size\\": %ld, \\"fd_offset\\": %ld, \\"fd_width\\": %ld}}", '
            f"sep, (unsigned long)({m}), {size}, {off}, {width});")
        out.append('  sep = ", ";')
        out.append("#endif")
    out += ['  printf("]\\n");',
            # The initialization flags this release's UVM accepts
            # (uvm_va_space_create refuses any other bit): which of the
            # flags that turn pageable access off the backend may force.
            "#ifdef UVM_INIT_FLAGS_MASK",
            '  printf("%llu\\n", (unsigned long long)UVM_INIT_FLAGS_MASK);',
            "#else",
            '  printf("-1\\n");',
            "#endif",
            "  return 0;", "}"]
    return "\n".join(out) + "\n"


def measure(root):
    incs = []
    for d in INCLUDE_DIRS:
        incs += ["-I", str(root / d)]
    with tempfile.TemporaryDirectory() as t:
        c, exe = Path(t) / "probe.c", Path(t) / "probe"
        c.write_text(probe_source())
        r = subprocess.run(["gcc", "-w", *incs, "-o", str(exe), str(c)],
                           capture_output=True, text=True)
        if r.returncode:
            raise ExtractError(f"probe does not compile:\n{r.stderr}")
        lines = subprocess.run([str(exe)], check=True, capture_output=True,
                               text=True).stdout.splitlines()
        rows, init_flags_mask = json.loads(lines[0]), int(lines[1])
    if init_flags_mask < 0:
        raise ExtractError("no UVM_INIT_FLAGS_MASK: not a UVM this backend knows")
    by = {r["name"]: r for r in rows}
    commands, absent = [], []
    for name, fd, of in COMMANDS:
        r = by.get(name)
        if r is None:
            absent.append(name)
            continue
        if not 0 <= r["size"] < 1 << 16:
            raise ExtractError(f"{name}: size {r['size']}")
        entry = {"name": name, "cmd": r["cmd"], "size": r["size"], "fd": None}
        if fd:
            if r["fd_width"] != 4:
                raise ExtractError(f"{name}.{fd} is {r['fd_width']} bytes, not an int")
            entry["fd"] = {"field": fd, "offset": r["fd_offset"], "of": of}
        commands.append(entry)
    for name in ("INITIALIZE", "MM_INITIALIZE"):
        if name in absent:
            raise ExtractError(f"no UVM_{name}: not a UVM this backend knows")
    return commands, absent, init_flags_mask


def extract(version, root, why):
    commands, absent, init_flags_mask = measure(root)
    return {
        "format": FORMAT,
        "driver_version": version,
        "why": why,
        "source": source_of(root),
        "init_flags_mask": init_flags_mask,
        "commands": commands,
        "absent": absent,
    }


def dump(d):
    return json.dumps(d, indent=1) + "\n"


def table_of(d):
    """What a release's table depends on: its commands and the
    initialization flags it takes, not its source."""
    return json.dumps([d["commands"], d.get("init_flags_mask")], sort_keys=True)


def cmd_extract(args):
    v = args.version
    root = Path(args.src).resolve() if args.src else fetch(v, args.cache)
    d = extract(v, root, VERSIONS.get(v, "measured by hand"))
    OUT_DIR.mkdir(exist_ok=True)
    (OUT_DIR / f"{v}.json").write_text(dump(d))
    print(f"gen/uvm/{v}.json: {len(d['commands'])} commands, "
          f"absent {d['absent'] or 'none'}")


def cmd_all(args):
    OUT_DIR.mkdir(exist_ok=True)
    for v, why in VERSIONS.items():
        d = extract(v, fetch(v, args.cache), why)
        (OUT_DIR / f"{v}.json").write_text(dump(d))
        print(f"gen/uvm/{v}.json: {len(d['commands'])} commands")
    stale = {p.stem for p in OUT_DIR.glob("*.json")} - set(VERSIONS)
    for v in sorted(stale):
        (OUT_DIR / f"{v}.json").unlink()
        print(f"gen/uvm/{v}.json: removed (not in VERSIONS)")


def cmd_check(args):
    bad = []
    for v, why in VERSIONS.items():
        fresh = dump(extract(v, fetch(v, args.cache), why))
        p = OUT_DIR / f"{v}.json"
        if not p.exists() or p.read_text() != fresh:
            bad.append(v)
    extra = {p.stem for p in OUT_DIR.glob("*.json")} - set(VERSIONS)
    if bad or extra:
        print(f"gen/uvm is stale: {sorted(bad)} differ, {sorted(extra)} extra; "
              "run ./uvm_extract.py all", file=sys.stderr)
        return 1
    print("gen/uvm is up to date")
    return 0


def tags():
    r = subprocess.run(["git", "ls-remote", "--tags", f"https://github.com/{REPO}"],
                       check=True, capture_output=True, text=True)
    out = set()
    for line in r.stdout.splitlines():
        ref = line.split("\t")[1]
        name = ref.rsplit("/", 1)[1].removesuffix("^{}")
        if re.fullmatch(r"\d+\.\d+(\.\d+)?", name):
            out.add(name)
    return sorted(out, key=version_key)


def cmd_scan(args):
    measured = sorted(VERSIONS, key=version_key)
    tables = {v: table_of(json.loads((OUT_DIR / f"{v}.json").read_text()))
              for v in measured}
    first = version_key(measured[0])
    bad = 0
    for t in tags():
        k = version_key(t)
        if k < first:
            continue
        base = [v for v in measured if version_key(v) <= k][-1]
        commands, _ = measure(fetch(t, args.cache))
        if json.dumps(commands, sort_keys=True) != tables[base]:
            print(f"{t}: differs from {base}, the measured release it would take",
                  file=sys.stderr)
            bad += 1
    if bad:
        print(f"{bad} release(s) need a place in VERSIONS", file=sys.stderr)
        return 1
    print("every tag matches the measured release below it")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract")
    e.add_argument("version")
    e.add_argument("--src", help="a local open-gpu-kernel-modules tree")
    sub.add_parser("all")
    sub.add_parser("check")
    sub.add_parser("scan")
    args = ap.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)
    try:
        return {"extract": cmd_extract, "all": cmd_all, "check": cmd_check,
                "scan": cmd_scan}[args.cmd](args) or 0
    except ExtractError as e:
        print(f"uvm_extract.py: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
