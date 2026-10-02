#!/usr/bin/env python3
"""Generate the list of RM controls and classes a guest may ask for.

RM itself says which of its entry points an unprivileged caller may reach.
Every exported control method carries a `flags` word in the NVOC-generated
tables under `src/nvidia/generated/g_*_nvoc.c`, and every allocatable class
carries one in `src/nvidia/src/kernel/rmapi/resource_list.h`. This script
reads both and emits the subset that RM marks non-privileged.

    ./rmallow_extract.py --ogkm ~/forks/ogkm-615.71.09 --version 615.71.09 \\
        > src/rmallow/v615_71_09.rs

Through this project the caller RM checks is the *backend*, which runs as a
service account on the host. A control RM would have refused to a guest's
own uid it will happily run for the backend. The backend therefore has to
apply RM's own privilege rule itself, before forwarding, which is what this
table is for.

The rule is a passlist's opposite, and it is written to fail closed:

  * A control is listed only if *every* exported-method entry naming it sets
    RMCTRL_FLAGS_NON_PRIVILEGED and none sets PRIVILEGED, INTERNAL or
    PRIVILEGED_IF_RS_ACCESS_DISABLED. One command can be exported by several
    classes with different flags; the strictest wins.
  * A class is listed only if it sets RS_FLAGS_ALLOC_NON_PRIVILEGED and none
    of INTERNAL_ONLY, ALLOC_PRIVILEGED, ALLOC_KERNEL_PRIVILEGED, and its
    required access rights are RS_ACCESS_NONE.

The trap this file exists to avoid: RMCTRL_FLAGS_KERNEL_PRIVILEGED and
RS_FLAGS_NONE are both 0. An entry with no bits set is not unrestricted --
it is kernel-only, the most privileged thing in the table. A rule written as
"not privileged" rather than "non-privileged" would admit all 32 of them.
Both halves below test for the positive bit and never for the absence of a
negative one.

Sizes are not transcribed. The script writes a C probe that includes the
release's own headers and prints `sizeof` for each parameter and allocation
struct, and compiles it with the host's cc. Needs: python3, cc.
"""

import argparse
import contextlib
import glob
import io
import os
import re
import shutil
import subprocess
import sys
import tempfile

GEN = "src/nvidia/generated"
INC = "src/common/sdk/nvidia/inc"
RMAPI = "src/nvidia/src/kernel/rmapi"

# The four control flags the rule turns on, by name. Their *values* are read
# out of each release's own header: 535.129.03 numbers them differently from
# 615.71.09 (NON_PRIVILEGED is 0x10 there, 0x8 here, and INTERNAL 0x400 against
# 0x80). A rule carrying the newer numbers would have tested bits that mean
# something else in the older release.
CTRL_FLAG_NAMES = (
    "RMCTRL_FLAGS_NON_PRIVILEGED",
    "RMCTRL_FLAGS_PRIVILEGED",
    "RMCTRL_FLAGS_INTERNAL",
    "RMCTRL_FLAGS_PRIVILEGED_IF_RS_ACCESS_DISABLED",
)

ENTRY = re.compile(
    r"/\*flags=\*/\s*(0x[0-9a-fA-F]+)u?,\s*"
    r"/\*accessRight=\*/\s*(0x[0-9a-fA-F]+)u?,\s*"
    r"/\*methodId=\*/\s*(0x[0-9a-fA-F]+)u?,\s*"
    r"/\*paramSize=\*/\s*(sizeof\((\w+)\)|0)\s*",
    re.S,
)

# `} NAME;` closing a typedef'd struct or union, and the one-line
# `typedef OLD NEW;` aliases -- 54 parameter types in 615.71.09 are the
# second kind, NV2080_CTRL_GR_GET_CAPS_V2_PARAMS among them.
DECL_CLOSE = re.compile(r"^\}\s*(\w+)\s*;", re.M)
DECL_ALIAS = re.compile(r"^typedef\s+(?:struct|union|enum)?\s*[\w ]*?(\w+)\s*;", re.M)


def ctrl_flags(ogkm):
    """The flag values this release uses, read from its own control.h."""
    text = open(os.path.join(ogkm, "src/nvidia/inc/kernel/rmapi/control.h")).read()
    out = {}
    for name in CTRL_FLAG_NAMES:
        m = re.search(rf"#define\s+{name}\s+(0x[0-9a-fA-F]+)", text)
        if not m:
            # Not a flag this release has, so there is no bit to test for it.
            # Refusing to guess: the rule below needs NON_PRIVILEGED to exist,
            # and the others only ever add refusals.
            if name == "RMCTRL_FLAGS_NON_PRIVILEGED":
                sys.exit(f"{name} is not in control.h; this release does not mark controls the way the rule expects")
            out[name] = 0
            continue
        out[name] = int(m.group(1), 16)
    # The one that is worth nothing numerically and everything semantically: an
    # entry with no bits set is kernel-only, not unrestricted. The rule below
    # tests for the NON_PRIVILEGED bit being *set* and never for a negative
    # bit being clear, so a zero flags word is refused like any other.
    m = re.search(r"#define\s+RMCTRL_FLAGS_KERNEL_PRIVILEGED\s+(0x[0-9a-fA-F]+)", text)
    if m and int(m.group(1), 16) != 0:
        sys.exit("RMCTRL_FLAGS_KERNEL_PRIVILEGED is no longer 0; re-read the rule")
    if out["RMCTRL_FLAGS_NON_PRIVILEGED"] == 0:
        sys.exit("RMCTRL_FLAGS_NON_PRIVILEGED is 0 in this release; every control would pass")
    return out


def exported_methods(ogkm):
    """Every exported control method entry, from the NVOC-generated tables."""
    out = []
    for path in sorted(glob.glob(os.path.join(ogkm, GEN, "g_*_nvoc.c"))):
        text = open(path, errors="replace").read()
        for m in ENTRY.finditer(text):
            out.append(
                dict(
                    flags=int(m.group(1), 16),
                    access=int(m.group(2), 16),
                    cmd=int(m.group(3), 16),
                    type=m.group(5),
                    file=os.path.basename(path),
                )
            )
    return out


def allowed_controls(entries, flags):
    """Fold the entries per command. The strictest entry decides."""
    non_priv = flags["RMCTRL_FLAGS_NON_PRIVILEGED"]
    priv = flags["RMCTRL_FLAGS_PRIVILEGED"]
    internal = flags["RMCTRL_FLAGS_INTERNAL"]
    priv_if_rs = flags["RMCTRL_FLAGS_PRIVILEGED_IF_RS_ACCESS_DISABLED"]
    by_cmd = {}
    for e in entries:
        by_cmd.setdefault(e["cmd"], []).append(e)
    allow, refused = {}, {}
    for cmd, es in sorted(by_cmd.items()):
        bad = None
        for e in es:
            f = e["flags"]
            if not f & non_priv:
                bad = "kernel-privileged (flags 0)" if f == 0 else f"flags {f:#x} does not set NON_PRIVILEGED"
            elif f & priv:
                bad = "PRIVILEGED"
            elif f & internal:
                bad = "INTERNAL"
            elif priv_if_rs and f & priv_if_rs:
                bad = "PRIVILEGED_IF_RS_ACCESS_DISABLED"
            elif e["access"]:
                bad = f"accessRight {e['access']:#x}"
            if bad:
                break
        if bad:
            refused[cmd] = bad
            continue
        types = {e["type"] for e in es}
        if len(types) > 1:
            # Two classes export the same command with different parameter
            # structs. The backend checks one size; it cannot check two.
            refused[cmd] = "exported with more than one parameter type: " + ", ".join(sorted(t or "none" for t in types))
            continue
        allow[cmd] = es[0]["type"]
    return allow, refused


def type_index(ogkm):
    """Every type name the SDK headers declare, and the header declaring it."""
    idx = {}
    root = os.path.join(ogkm, INC)
    for dirpath, _, names in os.walk(root):
        for n in names:
            if not n.endswith(".h"):
                continue
            p = os.path.join(dirpath, n)
            rel = os.path.relpath(p, root)
            text = open(p, errors="replace").read()
            for m in DECL_CLOSE.finditer(text):
                idx.setdefault(m.group(1), rel)
            for ln in text.splitlines():
                if ln.startswith("typedef ") and ln.rstrip().endswith(";"):
                    name = ln.rstrip().rstrip(";").split()[-1].lstrip("*")
                    if name.isidentifier():
                        idx.setdefault(name, rel)
    return idx


def define_index(ogkm):
    """Every macro the SDK headers define, and the header defining it."""
    idx = {}
    root = os.path.join(ogkm, INC)
    pat = re.compile(r"^\s*#\s*define\s+(\w+)")
    for dirpath, _, names in os.walk(root):
        for n in names:
            if not n.endswith(".h"):
                continue
            p = os.path.join(dirpath, n)
            rel = os.path.relpath(p, root)
            for ln in open(p, errors="replace"):
                m = pat.match(ln)
                if m:
                    idx.setdefault(m.group(1), rel)
    return idx


RS_ENTRY_RE = re.compile(
    r"RS_ENTRY\(\s*/\*\s*External Class\s*\*/\s*(\w+)\s*,.*?"
    r"/\*\s*Alloc Param Info\s*\*/\s*(RS_NONE|RS_OPTIONAL\((\w+)\)|RS_REQUIRED\((\w+)\))\s*,",
    re.S,
)


def resource_entries(ogkm):
    """Each RS_ENTRY in order: its external class name and its alloc param type."""
    text = open(os.path.join(ogkm, RMAPI, "resource_list.h"), errors="replace").read()
    out = []
    for m in RS_ENTRY_RE.finditer(text):
        out.append(dict(name=m.group(1), param=m.group(3) or m.group(4)))
    return out


def ctrl_probe(allow, idx):
    """A C program printing `K <cmd> <params size>` for each allowed control."""
    hdrs, rows, unresolved = set(), [], {}
    for cmd, t in sorted(allow.items()):
        if t is None:
            rows.append((cmd, None))
            continue
        h = idx.get(t)
        if not h:
            unresolved[cmd] = t
            continue
        hdrs.add(h)
        rows.append((cmd, t))
    out = ["#include <stddef.h>", "#include <stdio.h>", '#include "nvtypes.h"', '#include "nvos.h"']
    out += [f'#include "{h}"' for h in sorted(hdrs)]
    out.append("int main(void) {")
    for cmd, t in rows:
        size = "0" if t is None else f"sizeof({t})"
        out.append(f'  printf("K {cmd:#x} %zu\\n", (size_t)({size}));')
    out.append("  return 0;\n}")
    return "\n".join(out), unresolved


# resource_list.h is an X-macro table: RM includes it several times with a
# different RS_ENTRY each time. The probe does the same, with an RS_ENTRY that
# prints, so the class flags are evaluated by the compiler exactly as RM
# evaluates them rather than transcribed.
CLASS_PROBE = r"""
#include <stddef.h>
#include <stdio.h>
#include "nvtypes.h"
#include "nvos.h"
#include "rs_access.h"
#include "resource_desc_flags.h"
@INCLUDES@
@STUBS@

#define NVBIT(b) (1ULL << (b))
#define RS_ROOT_OBJECT 0
#define RS_ANY_PARENT 0
#define RS_LIST(...) 0
#define classId(x) 0
#define RS_NONE 0, 0
#define RS_OPTIONAL(T) 0, sizeof(T)
#define RS_REQUIRED(T) 1, sizeof(T)
#define RS_ACCESS_NONE 0
#define RS_ACCESS_LIST(...) 1

#define RS_ENTRY(cls, ic, mi, parents, allocParam, prio, flags, rights) \
    printf("L %s %#x %#llx %d %zu %#llx\n", #cls, (unsigned)(cls), \
           (unsigned long long)(flags), allocParam, (unsigned long long)(rights));
int main(void) {
#include "resource_list.h"
  return 0;
}
"""


def class_probe(ogkm, defs, types):
    """The X-macro probe, plus the names this release keeps outside the SDK.

    A handful of RS_ENTRY rows name classes that exist only inside RM -- the
    lock-stress test objects, confidential-compute parameters. Published
    headers do not declare them, so the probe stubs them out and this function
    reports which rows to drop: a class the SDK does not export is not a class
    a guest can be allowed to allocate.
    """
    entries = resource_entries(ogkm)
    hdrs, stubs, drop = set(), [], set()
    for e in entries:
        h = defs.get(e["name"])
        if h:
            hdrs.add(h)
        else:
            stubs.append(f'#define {e["name"]} 0xffffffffu')
            drop.add(e["name"])
        if e["param"]:
            h = types.get(e["param"]) or defs.get(e["param"])
            if h:
                hdrs.add(h)
            else:
                stubs.append(f'typedef char {e["param"]};')
                drop.add(e["name"])
    src = CLASS_PROBE.replace("@INCLUDES@", "\n".join(f'#include "{h}"' for h in sorted(hdrs)))
    src = src.replace("@STUBS@", "\n".join(stubs))
    return src, entries, drop


# resource_desc_flags.h. Asserted against the header in check_class_flags().
RS_ALLOC_NON_PRIVILEGED = 1 << 8
RS_ALLOC_PRIVILEGED = 1 << 9
RS_ALLOC_KERNEL_PRIVILEGED = 1 << 10
RS_INTERNAL_ONLY = 1 << 7


def check_class_flags(ogkm):
    text = open(os.path.join(ogkm, RMAPI, "resource_desc_flags.h")).read()
    want = {
        "RS_FLAGS_ALLOC_NON_PRIVILEGED": 8,
        "RS_FLAGS_ALLOC_PRIVILEGED": 9,
        "RS_FLAGS_ALLOC_KERNEL_PRIVILEGED": 10,
        "RS_FLAGS_INTERNAL_ONLY": 7,
    }
    for name, bit in want.items():
        m = re.search(rf"#define\s+{name}\s+NVBIT\((\d+)\)", text)
        if not m:
            sys.exit(f"{name} is not in resource_desc_flags.h; this release moved the flags")
        if int(m.group(1)) != bit:
            sys.exit(f"{name} is bit {m.group(1)} in this release, not {bit}")
    # The class half's version of the same trap.
    if not re.search(r"#define\s+RS_FLAGS_NONE\s+0\b", text):
        sys.exit("RS_FLAGS_NONE is no longer 0; re-read the rule")


def allowed_classes(rows, drop):
    """RM's own rule, applied to the probe's numbers."""
    out, refused = [], {}
    for r in rows:
        name = r["name"]
        f = r["flags"]
        if name in drop:
            bad = "not declared by the published SDK headers"
        elif not f & RS_ALLOC_NON_PRIVILEGED:
            bad = "no ALLOC_NON_PRIVILEGED bit" + (" (flags 0)" if f == 0 else f" (flags {f:#x})")
        elif f & RS_ALLOC_PRIVILEGED:
            bad = "ALLOC_PRIVILEGED"
        elif f & RS_ALLOC_KERNEL_PRIVILEGED:
            bad = "ALLOC_KERNEL_PRIVILEGED"
        elif f & RS_INTERNAL_ONLY:
            bad = "INTERNAL_ONLY"
        elif r["rights"]:
            bad = f"requires access rights {r['rights']:#x}"
        else:
            out.append(dict(class_=r["class_"], size=r["size"], required=r["required"], name=name))
            continue
        refused[name] = bad
    out.sort(key=lambda c: c["class_"])
    return out, refused


def emit(rows, classes, version, stream):
    ident = "v" + version.replace(".", "_")
    print("// Generated by gen/rmallow_extract.py -- do not edit by hand.", file=stream)
    print("//", file=stream)
    print(f"//   ./rmallow_extract.py --ogkm <open-gpu-kernel-modules at {version}> \\", file=stream)
    print(f"//       --version {version} > src/rmallow/{ident}.rs", file=stream)
    print("//", file=stream)
    print(f"// The RM controls and classes driver {version} exports to an unprivileged", file=stream)
    print("// caller. A command or class that is not here is refused under every cap.", file=stream)
    print(file=stream)
    print("use super::{AllowClass, AllowCtrl};", file=stream)
    print(file=stream)
    # One row per line. These are lists a person has to be able to read down,
    # so they are built through a const constructor rather than as struct
    # literals, which rustfmt breaks across four lines each.
    print("pub static CTRL: &[AllowCtrl] = &[", file=stream)
    for cmd, size in rows:
        print(f"    AllowCtrl::new({cmd:#010x}, {size}),", file=stream)
    print("];", file=stream)
    print(file=stream)
    print("pub static CLASS: &[AllowClass] = &[", file=stream)
    for c in classes:
        print(
            f"    AllowClass::new({c['class_']:#010x}, {c['size']}, "
            f"{str(c['required']).lower()}), // {c['name']}",
            file=stream,
        )
    print("];", file=stream)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ogkm", required=True, help="open-gpu-kernel-modules checkout at the release tag")
    ap.add_argument("--version", required=True)
    ap.add_argument("--cc", default=os.environ.get("CC", "cc"))
    a = ap.parse_args()

    flags = ctrl_flags(a.ogkm)
    check_class_flags(a.ogkm)
    entries = exported_methods(a.ogkm)
    if not entries:
        sys.exit(f"no exported-method tables under {GEN}; is this an open-gpu-kernel-modules checkout?")
    allow, refused = allowed_controls(entries, flags)
    types = type_index(a.ogkm)
    defs = define_index(a.ogkm)
    src, unresolved = ctrl_probe(allow, types)
    csrc, rs_entries, drop = class_probe(a.ogkm, defs, types)

    incs = ["-I", os.path.join(a.ogkm, INC), "-I", os.path.join(a.ogkm, "src/common/inc"),
            "-I", os.path.join(a.ogkm, RMAPI), "-I", os.path.join(a.ogkm, "src/common/sdk/nvidia/inc")]
    with tempfile.TemporaryDirectory() as d:
        rows = []
        c = os.path.join(d, "ctrl.c")
        open(c, "w").write(src)
        exe = os.path.join(d, "ctrl")
        r = subprocess.run([a.cc, "-w", *incs, c, "-o", exe], capture_output=True, text=True)
        if r.returncode:
            sys.stderr.write(r.stderr[-4000:])
            sys.exit("the control probe did not compile")
        for ln in subprocess.run([exe], capture_output=True, text=True, check=True).stdout.split("\n"):
            f = ln.split()
            if f and f[0] == "K":
                rows.append((int(f[1], 16), int(f[2])))

        raw = []
        c = os.path.join(d, "class.c")
        open(c, "w").write(csrc)
        exe = os.path.join(d, "class")
        r = subprocess.run([a.cc, "-w", *incs, c, "-o", exe], capture_output=True, text=True)
        if r.returncode:
            sys.stderr.write(r.stderr[-6000:])
            sys.exit("the class probe did not compile")
        for ln in subprocess.run([exe], capture_output=True, text=True, check=True).stdout.split("\n"):
            f = ln.split()
            if f and f[0] == "L":
                raw.append(
                    dict(name=f[1], class_=int(f[2], 16), flags=int(f[3], 16),
                         required=f[4] == "1", size=int(f[5]), rights=int(f[6], 16))
                )

    # A row the parser saw and the probe did not is one this release compiles
    # out -- resource_list.h has a #if around NV_CE_UTILS for debug builds.
    # Left out, which is the safe direction for an allowlist, and named.
    skipped = sorted({e["name"] for e in rs_entries} - {r["name"] for r in raw})
    classes, class_refused = allowed_classes(raw, drop)

    buf = io.StringIO()
    emit(rows, classes, a.version, buf)
    text = buf.getvalue()
    fmt = shutil.which("rustfmt")
    if fmt:
        r = subprocess.run([fmt, "--edition", "2024"], input=text, capture_output=True, text=True)
        if r.returncode == 0:
            text = r.stdout
        else:
            sys.stderr.write("rustfmt refused this table; writing it unformatted\n")
    sys.stdout.write(text)

    if unresolved:
        # Left out of the allowlist, which is the safe direction, but named:
        # a control RM exports to anyone and this script cannot size is a
        # control the backend will refuse and somebody will have to explain.
        sys.stderr.write("non-privileged, no parameter type in the SDK headers (left out):\n")
        for cmd, t in sorted(unresolved.items()):
            sys.stderr.write(f"  {cmd:#010x} {t}\n")
    sys.stderr.write(
        f"{len(entries)} exported methods over {len(refused) + len(rows) + len(unresolved)} commands: "
        f"{len(rows)} allowed, {len(refused)} refused\n"
    )
    if skipped:
        sys.stderr.write("in resource_list.h but not compiled by this release (left out):\n")
        for n in skipped:
            sys.stderr.write(f"  {n}\n")
    sys.stderr.write(
        f"{len(rs_entries)} classes: {len(classes)} allowed, "
        f"{len(class_refused)} refused, {len(skipped)} not compiled\n"
    )


if __name__ == "__main__":
    main()
