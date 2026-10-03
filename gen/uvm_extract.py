#!/usr/bin/env python3
"""Generate the UVM commands a guest may send, and where their descriptors sit.

`/dev/nvidia-uvm` takes its own ioctls, numbered in their own namespace and
carrying their own parameter structs. None of RM's privilege machinery applies
to them: `uvm_ioctl.h` has no flags word, so unlike `rmallow_extract.py` there
is no privilege to read. What there is, and what the backend needs, is the
shape of each call.

    ./uvm_extract.py --ogkm ~/forks/ogkm-615.71.09 --version 615.71.09 \\
        > src/uvm/v615_71_09.rs

Three things come out of each release:

  * The command number and `sizeof` its parameter struct. A UVM ioctl passes
    only if it is in the table at exactly that size.
  * The byte offset of every file descriptor a parameter struct carries, and
    of the `NvHandle` beside it. A descriptor in a guest's ioctl means nothing
    in the backend's process, so the backend has to find it, translate it, and
    check it is this VM's -- which it cannot do without knowing where it is.
  * The `UVM_INIT_FLAGS_*` values and their mask, which the backend sends in
    place of whatever the guest asked for.

Every number is read from the release's own headers by a probe compiled
against them. Nothing is transcribed, for the reason `rmallow_extract.py`
learned the hard way: these constants move. `UVM_INIT_FLAGS_DISABLE_HMM` and
`UVM_INIT_FLAGS_DISABLE_PAGEABLE_MIGRATIONS` are both `0x1` -- the same bit
under two names, because one release renamed it -- so a backend carrying the
wrong name for a release sends a flag that means something else. Offsets move
for duller reasons: a field added anywhere above a descriptor moves it.

Needs: python3, cc.
"""

import argparse
import glob
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile

UVM = "kernel-open/nvidia-uvm"

# Every descriptor field UVM declares, by name. These are not interchangeable
# and the backend does not treat them so: `rmCtrlFd` is a descriptor for
# `/dev/nvidiactl`, `uvmFd` one for `/dev/nvidia-uvm`, `handleFd` one for a
# dma-buf, which is not a device of ours at all. A name not listed here stops
# the script rather than being forwarded as an opaque integer: an unknown
# descriptor field is a hole, and the whole point of this table is that the
# backend knows where every descriptor is.
FD_FIELDS = {
    "rmCtrlFd": "Ctl",
    "uvmFd": "Uvm",
    "handleFd": "Foreign",
}

# The handle that accompanies a descriptor. UVM passes `{rmCtrlFd, hClient}`
# together: the descriptor says which RM client table to look in and the
# handle indexes it. Forwarding a guest's handle beside a translated
# descriptor would name an object in the backend's client, not the guest's.
HANDLE_FIELD = "hClient"

CMD = re.compile(r"^#define\s+(UVM_\w+)\s+UVM_IOCTL_BASE\((\d+)\)", re.M)
LINUX_CMD = re.compile(r"^#define\s+(UVM_(?:INITIALIZE|DEINITIALIZE))\s+(0x[0-9a-fA-F]+)", re.M)
STRUCT = re.compile(r"typedef\s+struct\s*\{(?P<body>.*?)\}\s*(?P<name>UVM_\w+_PARAMS)\s*;", re.S)
FIELD = re.compile(r"^\s*(?:const\s+)?\w[\w ]*?(\w+)\s*(?:NV_ALIGN_BYTES\(\d+\))?\s*(?:\[[^\]]*\])?\s*;", re.M)
INIT_FLAG = re.compile(r"^#define\s+(UVM_INIT_FLAGS_\w+)\s+\(\(NvU64\)(0x[0-9a-fA-F]+)\)", re.M)


def read(ogkm, name):
    path = os.path.join(ogkm, UVM, name)
    if not os.path.exists(path):
        sys.exit(f"{path} is missing; is this an open-gpu-kernel-modules checkout?")
    return open(path, errors="replace").read()


def commands(ogkm):
    """Each UVM command, its number, and the parameter struct that follows it.

    The header is written as a command define followed by its struct, so the
    struct is found by position rather than by assuming the name -- a few
    commands take a struct whose name is not the command's with `_PARAMS`
    glued on, and a few take none at all.
    """
    text = read(ogkm, "uvm_ioctl.h")
    structs = [(m.start(), m.group("name"), m.group("body")) for m in STRUCT.finditer(text)]
    out = {}
    marks = [(m.start(), m.group(1), int(m.group(2))) for m in CMD.finditer(text)]
    if not marks:
        sys.exit("no UVM_IOCTL_BASE commands in uvm_ioctl.h; this release numbers them differently")
    for i, (pos, name, num) in enumerate(marks):
        stop = marks[i + 1][0] if i + 1 < len(marks) else len(text)
        here = [s for s in structs if pos < s[0] < stop]
        if not here:
            # A command with no parameters at all. Listed, at size zero.
            out[name] = dict(num=num, struct=None, body="")
            continue
        out[name] = dict(num=num, struct=here[0][1], body=here[0][2])

    # UVM_INITIALIZE and UVM_DEINITIALIZE are numbered outside the base: they
    # are the only two legal on the file before it is initialised.
    linux = read(ogkm, "uvm_linux_ioctl.h")
    for m in LINUX_CMD.finditer(linux):
        name, num = m.group(1), int(m.group(2), 16)
        s = STRUCT.search(linux, m.end())
        if s and s.group("name") == name + "_PARAMS":
            out[name] = dict(num=num, struct=s.group("name"), body=s.group("body"))
        else:
            out[name] = dict(num=num, struct=None, body="")
    if "UVM_INITIALIZE" not in out:
        sys.exit("no UVM_INITIALIZE in uvm_linux_ioctl.h; the backend decides its flags and cannot without this")
    return out


def descriptors(name, body):
    """The descriptor fields in one parameter struct, and the handle beside each.

    The handle is taken as the next `hClient` *after* the descriptor, which is
    how UVM writes them. A descriptor with no handle after it is fine --
    UVM_IMPORT_DMA_BUF carries one -- but a struct with more descriptors than
    this script can place stops it.
    """
    fields = FIELD.findall(body)
    out = []
    for i, f in enumerate(fields):
        if f not in FD_FIELDS:
            continue
        handle = None
        for g in fields[i + 1 :]:
            if g == HANDLE_FIELD:
                handle = g
                break
            if g in FD_FIELDS:
                break
        out.append(dict(field=f, kind=FD_FIELDS[f], handle=handle))
    unknown = [f for f in fields if f.lower().endswith("fd") and f not in FD_FIELDS]
    if unknown:
        sys.exit(
            f"{name} carries a descriptor field this script does not know: {', '.join(unknown)}. "
            "A descriptor the backend cannot place is one it would forward as the guest's own "
            "number; add it to FD_FIELDS once you know which device it names."
        )
    return out


def init_flags(ogkm):
    """The UVM_INIT_FLAGS_* this release defines, and its mask.

    Read rather than written down because two of them share a bit:
    DISABLE_HMM and DISABLE_PAGEABLE_MIGRATIONS are both 0x1, one release's
    name for what another calls something else.
    """
    text = read(ogkm, "uvm_types.h")
    flags = {m.group(1): int(m.group(2), 16) for m in INIT_FLAG.finditer(text)}
    if "UVM_INIT_FLAGS_MASK" not in flags:
        sys.exit("no UVM_INIT_FLAGS_MASK in uvm_types.h; nothing bounds the flags word")
    mask = flags.pop("UVM_INIT_FLAGS_MASK")
    for need in ("UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE",):
        if need not in flags:
            sys.exit(f"{need} is not in uvm_types.h; the backend sends it and cannot without a value")
    stray = {n: v for n, v in flags.items() if v & ~mask}
    if stray:
        sys.exit("flags outside UVM_INIT_FLAGS_MASK: " + ", ".join(f"{n}={v:#x}" for n, v in stray.items()))
    return flags, mask


PROBE = r"""
#include <stddef.h>
#include <stdio.h>
#include "nvtypes.h"
#include "nvstatus.h"
#include "uvm_types.h"
#include "uvm_ioctl.h"
#include "uvm_linux_ioctl.h"

int main(void) {
@BODY@
  return 0;
}
"""


def probe(cmds):
    """A C program printing every size and offset, from the release's headers.

    Offsets are `offsetof`, not counted by hand: NV_ALIGN_BYTES(8) on a field
    changes where everything after it sits, and the alignment rules are the
    compiler's to apply.
    """
    body = []
    for name in sorted(cmds):
        c = cmds[name]
        # Some commands sit inside an #if this release does not take -- the
        # UVM_REGION_*_BACKING pair among them. A probe that names one does not
        # compile, so each row is asked whether its command exists at all, and
        # a command this release leaves out is left out of the table too.
        body.append(f"#ifdef {name}")
        if c["struct"] is None:
            body.append(f'  printf("C {name} %d 0\\n", {name});')
            body.append("#endif")
            continue
        body.append(f'  printf("C {name} %d %zu\\n", {name}, sizeof({c["struct"]}));')
        for d in c["descriptors"]:
            h = (
                f'offsetof({c["struct"]}, {d["handle"]})'
                if d["handle"]
                else "(size_t)-1"
            )
            body.append(
                f'  printf("D {name} {d["kind"]} %zu %zd\\n", '
                f'offsetof({c["struct"]}, {d["field"]}), (ptrdiff_t)({h}));'
            )
        body.append("#endif")
    return PROBE.replace("@BODY@", "\n".join(body))


def emit(rows, flags, mask, version, stream):
    ident = "v" + version.replace(".", "_")
    p = lambda *a: print(*a, file=stream)
    p("// Generated by gen/uvm_extract.py -- do not edit by hand.")
    p("//")
    p(f"//   ./uvm_extract.py --ogkm <open-gpu-kernel-modules at {version}> \\")
    p(f"//       --version {version} > src/uvm/{ident}.rs")
    p("//")
    p(f"// The UVM commands driver {version} takes, their parameter sizes, and")
    p("// where in each the file descriptors sit. A command that is not here, or")
    p("// that arrives at any other size, is refused.")
    p()
    p("use super::{Fd, FdSlot, InitFlags, UvmCmd};")
    p()
    p("pub static CMD: &[UvmCmd] = &[")
    for r in rows:
        slots = ", ".join(
            f"FdSlot::new(Fd::{d['kind']}, {d['at']}, {d['handle']})" for d in r["descriptors"]
        )
        p(f"    UvmCmd::new({r['num']:#010x}, {r['size']}, &[{slots}]), // {r['name']}")
    p("];")
    p()
    opt = lambda n: f"Some({flags[n]:#x})" if n in flags else "None"
    p("/// What the backend sends in place of the guest's flags word.")
    p("///")
    p("/// A `None` is a flag this release does not define. That is not the same")
    p("/// as a flag it defines and leaves clear, and the backend treats it")
    p("/// differently: there is no bit to ask for, so it has to establish the")
    p("/// same thing another way or refuse the file.")
    p("pub static INIT: InitFlags = InitFlags {")
    p(f"    disable_hmm: {opt('UVM_INIT_FLAGS_DISABLE_HMM')},")
    p(f"    multi_process_sharing_mode: {flags['UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE']:#x},")
    p(f"    disable_pageable_access: {opt('UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS')},")
    p(f"    disable_pageable_migrations: {opt('UVM_INIT_FLAGS_DISABLE_PAGEABLE_MIGRATIONS')},")
    p(f"    mask: {mask:#x},")
    p("};")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ogkm", required=True, help="open-gpu-kernel-modules checkout at the release tag")
    ap.add_argument("--version", required=True)
    ap.add_argument("--cc", default=os.environ.get("CC", "cc"))
    a = ap.parse_args()

    cmds = commands(a.ogkm)
    for name, c in cmds.items():
        c["descriptors"] = descriptors(name, c["body"])
    flags, mask = init_flags(a.ogkm)

    # kernel-open/common/inc first: uvm_types.h pulls nv_uvm_user_types.h, which
    # lives there and not with the SDK headers.
    incs = []
    for d in (UVM, "kernel-open/common/inc", "src/common/sdk/nvidia/inc", "src/common/inc"):
        incs += ["-I", os.path.join(a.ogkm, d)]

    with tempfile.TemporaryDirectory() as d:
        src = os.path.join(d, "uvm.c")
        open(src, "w").write(probe(cmds))
        exe = os.path.join(d, "uvm")
        r = subprocess.run([a.cc, "-w", *incs, src, "-o", exe], capture_output=True, text=True)
        if r.returncode:
            sys.stderr.write(r.stderr[-6000:])
            sys.exit("the UVM probe did not compile")
        out = subprocess.run([exe], capture_output=True, text=True, check=True).stdout

    rows, slots = {}, {}
    for ln in out.split("\n"):
        f = ln.split()
        if not f:
            continue
        if f[0] == "C":
            rows[f[1]] = dict(name=f[1], num=int(f[2]), size=int(f[3]), descriptors=[])
        elif f[0] == "D":
            handle = int(f[4])
            slots.setdefault(f[1], []).append(
                dict(kind=f[2], at=int(f[3]), handle="None" if handle < 0 else f"Some({handle})")
            )
    for name, ds in slots.items():
        rows[name]["descriptors"] = ds

    # Declared in the header but not compiled by this release. Left out, which
    # refuses them -- the safe direction for a table like this -- and named.
    missing = sorted(set(cmds) - set(rows))

    ordered = sorted(rows.values(), key=lambda r: r["num"])
    dup = [r["name"] for r in ordered if sum(1 for o in ordered if o["num"] == r["num"]) > 1]
    if dup:
        sys.exit("two commands share a number: " + ", ".join(sorted(set(dup))))

    buf = io.StringIO()
    emit(ordered, flags, mask, a.version, buf)
    text = buf.getvalue()
    fmt = shutil.which("rustfmt")
    if fmt:
        r = subprocess.run([fmt, "--edition", "2024"], input=text, capture_output=True, text=True)
        if r.returncode == 0:
            text = r.stdout
        else:
            sys.stderr.write("rustfmt refused this table; writing it unformatted\n")
    sys.stdout.write(text)

    if missing:
        sys.stderr.write("in uvm_ioctl.h but not compiled by this release (left out):\n")
        for n in missing:
            sys.stderr.write(f"  {n}\n")
    carried = sum(len(r["descriptors"]) for r in ordered)
    noparams = [r["name"] for r in ordered if r["size"] == 0]
    sys.stderr.write(
        f"{len(ordered)} UVM commands, {carried} descriptor(s) across "
        f"{sum(1 for r in ordered if r['descriptors'])} of them; "
        f"{len(noparams)} take no parameters\n"
    )
    sys.stderr.write(
        "init flags: " + ", ".join(f"{n[len('UVM_INIT_FLAGS_'):]}={v:#x}" for n, v in sorted(flags.items(), key=lambda kv: kv[1]))
        + f", mask {mask:#x}\n"
    )
    for r in ordered:
        for d in r["descriptors"]:
            sys.stderr.write(f"  {r['name']}: {d['kind']} at {d['at']}, handle {d['handle']}\n")


if __name__ == "__main__":
    main()
