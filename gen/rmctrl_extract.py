#!/usr/bin/env python3
"""Generate the table of RM controls whose parameters carry a user pointer.

RM dereferences an embedded pointer in an RM_CONTROL's parameters only for
the controls listed in `embeddedParamCopyIn()`
(src/nvidia/src/kernel/rmapi/embedded_param_copy.c in NVIDIA's
open-gpu-kernel-modules). For each one it copies `count * element size`
bytes from the caller's address space. Through this project the caller is
the backend, so a guest's pointer would be read in the backend's memory.
The backend uses this table to size and supply every such buffer itself.

    ./rmctrl_extract.py --ogkm ~/forks/ogkm-615.71.09 --version 615.71.09 \\
        > src/rmctrl/v615_71_09.rs

Offsets and sizes are not transcribed: the script writes a C probe that
includes the release's own control headers and prints `offsetof` and
`sizeof` for every field, and compiles it with the host's cc.

It fails closed. A case whose shape the parser does not recognise is
emitted as `refuse: true`, so the backend refuses that control rather than
forwarding it with a pointer it cannot vouch for. Needs: python3, cc.
"""

import argparse
import contextlib
import io
import os
import shutil
import re
import subprocess
import sys
import tempfile

SRC = "src/nvidia/src/kernel/rmapi/embedded_param_copy.c"
INC = "src/common/sdk/nvidia/inc"

# `((T*)pParams)->a.b.c`, the shape almost every case uses.
FIELD = re.compile(r"^\(\(\s*(\w+)\s*\*\s*\)\s*pParams\s*\)\s*->\s*([\w.\[\]]+)$")
SIZEOF = re.compile(r"^sizeof\s*\(\s*(\w+)\s*\)$")
INIT = re.compile(r"RMAPI_PARAM_COPY_INIT\s*\(")
FLAG = re.compile(r"paramCopies\[(\d)\]\.flags\s*\|=\s*RMAPI_PARAM_COPY_FLAGS_(\w+)")


def function_body(text, name):
    """The text of one C function, braces balanced."""
    start = text.index(name + "(")
    start = text.index("{", start)
    depth = 0
    for i in range(start, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
    raise ValueError("unbalanced " + name)


def split_args(s):
    """Split a macro's argument list on top-level commas."""
    out, depth, cur = [], 0, ""
    for ch in s:
        if ch in "([":
            depth += 1
        elif ch in ")]":
            if depth == 0:
                break
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    out.append(cur.strip())
    return out


def macro_calls(block):
    """Every RMAPI_PARAM_COPY_INIT(...) in a case block, as argument lists."""
    calls = []
    for m in INIT.finditer(block):
        calls.append(split_args(block[m.end() :]))
    return calls


def cases(body):
    """Yield (label, guard, block) for each case of the function's switch.

    `guard` is the #ifdef the case sits under, if any. Cases under a guard
    the release does not define are compiled out of RM and are skipped by
    the probe the same way.
    """
    lines = body.split("\n")
    guard_stack, cur, cur_guard, buf = [], None, None, []
    for line in lines:
        s = line.strip()
        m = re.match(r"#\s*if(n?)def\s+(\w+)", s)
        if m:
            guard_stack.append(("!" if m.group(1) else "") + m.group(2))
            continue
        if re.match(r"#\s*endif", s):
            if guard_stack:
                guard_stack.pop()
            continue
        m = re.match(r"case\s+(\w+)\s*:", s)
        if m:
            if cur:
                yield cur, cur_guard, "\n".join(buf)
            cur, cur_guard, buf = m.group(1), (guard_stack[-1] if guard_stack else None), []
            continue
        if re.match(r"default\s*:", s):
            if cur:
                yield cur, cur_guard, "\n".join(buf)
            cur = None
            continue
        if cur:
            buf.append(line)
    if cur:
        yield cur, cur_guard, "\n".join(buf)


def parse_case(label, block):
    """Describe one case, or return a reason it cannot be described."""
    calls = macro_calls(block)
    if not calls:
        return None, "no RMAPI_PARAM_COPY_INIT (helper or conditional copy)"
    flags = {}
    for m in FLAG.finditer(block):
        flags.setdefault(int(m.group(1)), set()).add(m.group(2))
    ptrs, ptype = [], None
    for args in calls:
        if len(args) != 5:
            return None, f"RMAPI_PARAM_COPY_INIT with {len(args)} arguments"
        idx = re.match(r"paramCopies\[(\d)\]", args[0])
        src = FIELD.match(args[1])
        if not idx or not src:
            return None, f"pointer {args[1]!r} is not ((T*)pParams)->field"
        t, field = src.groups()
        if ptype and t != ptype:
            return None, "two parameter types in one case"
        ptype = t
        count = FIELD.match(args[3])
        if count:
            if count.group(1) != t:
                return None, "count read through another type"
            cnt = ("field", count.group(2))
        elif re.fullmatch(r"(0x[0-9a-fA-F]+|\d+)", args[3]):
            cnt = ("fixed", int(args[3], 0))
        elif args[3] == "numEntries" and "gpuCount *" in block:
            # SYSTEM_GET_P2P_CAPS: gpuCount squared, when the pointer is set.
            cnt = ("squared", "gpuCount")
        else:
            return None, f"count {args[3]!r} is computed"
        args = [re.sub(r'/\*.*?\*/', '', x).strip() for x in args]
        el = SIZEOF.match(args[4])
        if el:
            elem = ("sizeof", el.group(1))
        elif re.fullmatch(r"(0x[0-9a-fA-F]+|\d+)", args[4]):
            elem = ("fixed", int(args[4], 0))
        else:
            return None, f"element size {args[4]!r} is computed"
        f = flags.get(int(idx.group(1)), set())
        ptrs.append(
            dict(
                field=field,
                count=cnt,
                elem=elem,
                copy_in="SKIP_COPYIN" not in f,
                copy_out="SKIP_COPYOUT" not in f,
            )
        )
    return dict(type=ptype, ptrs=ptrs), None


def header_index(ogkm):
    """Map every macro the control headers define to the header defining it.

    The probe asks each case label for its value, under `#ifdef`, so a label
    whose header is not included compiles to nothing and the control is left
    out of the table -- forwarded, with the guest's pointer still in it. The
    include list is therefore derived from the labels rather than from the
    `#include` lines of `embedded_param_copy.c`, which name only some of them.
    """
    base = os.path.join(ogkm, INC)
    idx = {}
    for dirpath, _, names in os.walk(os.path.join(base, "ctrl")):
        for n in names:
            if not n.endswith(".h"):
                continue
            path = os.path.join(dirpath, n)
            rel = os.path.relpath(path, base)
            with open(path, errors="replace") as f:
                for line in f:
                    m = re.match(r"\s*#\s*define\s+(\w+)[\s(]", line)
                    if m:
                        idx.setdefault(m.group(1), rel)
    return idx


def probe_source(entries, hdrs):
    """A C program printing each entry's numbers, one line per pointer."""
    out = ["#include <stddef.h>", "#include <stdio.h>", '#include "nvtypes.h"']
    out += [f'#include "{h}"' for h in hdrs]
    out.append("int main(void) {")
    for e in entries:
        lab = e["label"]
        g = [f"#ifdef {lab}"]
        if e.get("refuse"):
            g.append(f'  printf("R %#x {lab}\\n", (unsigned){lab});')
        else:
            t = e["type"]
            g.append(f'  printf("C %#x %zu %zu {lab}\\n", (unsigned){lab}, sizeof({t}), (size_t){len(e["ptrs"])});')
            for i, p in enumerate(e["ptrs"]):
                kind, val = p["count"]
                if kind == "fixed":
                    co, cw = "0", "0"
                else:
                    co = f"offsetof({t}, {val})"
                    cw = f"sizeof((({t}*)0)->{val})"
                ek, ev = p["elem"]
                es = f"sizeof({ev})" if ek == "sizeof" else str(ev)
                g.append(
                    f'  printf("P %#x {i} %zu {kind} %zu %zu %zu {int(p["copy_in"])} {int(p["copy_out"])} {val if kind == "fixed" else 0}\\n", '
                    f'(unsigned){lab}, offsetof({t}, {p["field"]}), (size_t)({co}), (size_t)({cw}), (size_t)({es}));'
                )
        g.append("#endif")
        out += g
    out.append("  return 0;\n}")
    return "\n".join(out)


def emit_c(rows, version):
    """The same table, for the guest driver.

    Both halves of a call have to agree about where a pointer sits and how
    much it addresses -- the guest gathers the bytes, the backend sizes the
    buffer -- so both read a table from this one probe rather than from two
    descriptions that can drift apart.
    """
    # Same order as the Rust table: both halves look a control up by command,
    # and a reader comparing the two files should see the same sequence.
    rows = sorted(rows, key=lambda r: r["cmd"])
    ident = "v" + version.replace(".", "_")
    print("/* SPDX-License-Identifier: GPL-2.0 */")
    print("/*")
    print(" * Generated by gen/rmctrl_extract.py -- do not edit by hand.")
    print(" *")
    print(f" *   ./rmctrl_extract.py --ogkm <open-gpu-kernel-modules at {version}> \\")
    print(f" *       --version {version} --lang c > driver/rmctrl/{ident}.h")
    print(" *")
    print(f" * RM controls whose parameters RM dereferences a user pointer in, for")
    print(f" * driver {version}. See driver/nvgpu_rmctrl.h.")
    print(" */")
    print()
    print(f"static const struct nvgpu_rmctrl_ptr nvgpu_rmctrl_ptrs_{ident}[] = {{")
    flat = []
    for r in rows:
        first = len(flat)
        for p in r.get("ptrs", []):
            kind = {"fixed": "NVGPU_RMCTRL_FIXED", "field": "NVGPU_RMCTRL_FIELD",
                    "squared": "NVGPU_RMCTRL_SQUARED"}[p["kind"]]
            print(
                f"    {{ {p['off']}, {kind}, {p['coff']}, {p['cw']}, {p['fixed']}, "
                f"{p['es']}, {str(p['cin']).lower()}, {str(p['cout']).lower()} }},"
            )
            flat.append(p)
        r["first"] = first
    if not flat:
        print("    { 0, NVGPU_RMCTRL_FIXED, 0, 0, 0, 0, false, false },")
    print("};")
    print()
    print(f"static const struct nvgpu_rmctrl_entry nvgpu_rmctrl_entries_{ident}[] = {{")
    for r in rows:
        n = len(r.get("ptrs", []))
        print(
            f"    {{ {r['cmd']:#010x}, {r.get('size', 0)}, {str(bool(r.get('refuse'))).lower()}, "
            f"{r['first']}, {n} }},"
        )
    print("};")


def main():
    global open_src_text
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ogkm", required=True, help="open-gpu-kernel-modules checkout at the release tag")
    ap.add_argument("--version", required=True)
    ap.add_argument(
        "--lang",
        choices=("rust", "c"),
        default="rust",
        help="rust: a table for the backend. c: the same table for the guest driver.",
    )
    ap.add_argument("--cc", default=os.environ.get("CC", "cc"))
    a = ap.parse_args()

    open_src_text = open(os.path.join(a.ogkm, SRC)).read()
    body = function_body(open_src_text, "NV_STATUS embeddedParamCopyIn")

    entries = []
    for label, guard, block in cases(body):
        desc, why = parse_case(label, block)
        if guard:
            # The case compiles into RM only when an RM build flag is set, and
            # nothing in the published headers says whether the shipped driver
            # was built with it. Whether RM dereferences these pointers is
            # therefore unknown, so the control is refused rather than
            # described. NV2080_CTRL_CMD_FB_GET_AMAP_CONF is the one that
            # reaches a guest: it sits under USE_AMAPLIB.
            entries.append(
                dict(label=label, guard=guard, refuse=True, why=f"case is under #ifdef {guard}")
            )
        elif desc:
            entries.append(dict(label=label, guard=guard, **desc))
        else:
            entries.append(dict(label=label, guard=guard, refuse=True, why=why))

    # Every label the probe will ask for, and the header that defines it. A
    # label no header defines is one this release does not have; a label a
    # header defines must reach the table, and the check after the probe runs
    # says so.
    idx = header_index(a.ogkm)
    hdrs = sorted(set(re.findall(r'#include\s+"(ctrl/[^"]+)"', open_src_text)))
    for e in entries:
        h = idx.get(e["label"])
        if h:
            hdrs.append(h)
        if not e.get("refuse"):
            t = idx.get(e["type"] + "_MESSAGE_ID")
            if t:
                hdrs.append(t)
    hdrs = sorted(set(hdrs))
    defined = {e["label"] for e in entries if e["label"] in idx}

    with tempfile.TemporaryDirectory() as d:
        c = os.path.join(d, "probe.c")
        open(c, "w").write(probe_source(entries, hdrs))
        exe = os.path.join(d, "probe")
        cc = [a.cc, "-w", "-I", os.path.join(a.ogkm, INC), "-I", os.path.join(a.ogkm, "src/common/inc"), c, "-o", exe]
        r = subprocess.run(cc, capture_output=True, text=True)
        if r.returncode:
            sys.stderr.write(r.stderr[-4000:])
            sys.exit("probe did not compile")
        lines = subprocess.run([exe], capture_output=True, text=True, check=True).stdout.split("\n")

    rows, cur = [], None
    for ln in lines:
        f = ln.split()
        if not f:
            continue
        if f[0] == "R":
            rows.append(dict(cmd=int(f[1], 16), label=f[2], refuse=True))
        elif f[0] == "C":
            cur = dict(cmd=int(f[1], 16), size=int(f[2]), label=f[4], ptrs=[])
            rows.append(cur)
        elif f[0] == "P":
            cur["ptrs"].append(
                dict(
                    off=int(f[3]),
                    kind=f[4],
                    coff=int(f[5]),
                    cw=int(f[6]),
                    es=int(f[7]),
                    cin=f[8] == "1",
                    cout=f[9] == "1",
                    fixed=int(f[10]),
                )
            )
    # A control the release defines and the probe did not print is one the
    # table would be missing and the backend would forward with the guest's
    # pointer still in it. That is the failure this whole table exists to
    # prevent, so it stops here rather than producing a table with a hole in
    # it. It happened: NV2080_CTRL_CMD_FB_GET_AMAP_CONF (0x20801336) was in
    # none of the first five tables, because no header the probe included
    # defined its label.
    seen = {r["label"] for r in rows}
    missing = [l for l in sorted(defined) if l not in seen]
    if missing:
        sys.exit(
            "these controls are defined by the release and did not reach the "
            "table:\n  " + "\n  ".join(missing)
        )

    # The tables are checked in, so they are formatted the way the rest of the
    # tree is: a regenerated table has to be byte-identical to the one in git,
    # or `cargo fmt --check` fails on a file nobody edited.
    if a.lang == "c":
        emit_c(rows, a.version)
        return

    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        ident = "v" + a.version.replace(".", "_")
        print("// Generated by gen/rmctrl_extract.py -- do not edit by hand.")
        print("//")
        print(f"//   ./rmctrl_extract.py --ogkm <open-gpu-kernel-modules at {a.version}> \\")
        print(f"//       --version {a.version} > src/rmctrl/{ident}.rs")
        print("//")
        print(f"// RM controls whose parameters RM dereferences a user pointer in, for")
        print(f"// driver {a.version}, from embeddedParamCopyIn(). Offsets and sizes come from a")
        print("// probe compiled against that release's control headers.")
        print()
        print("use super::{Count, EmbeddedPtr, RmCtrlEntry};")
        print()
        print("pub static TABLE: &[RmCtrlEntry] = &[")
        for r in sorted(rows, key=lambda r: r["cmd"]):
            if r.get("refuse"):
                print(f"    RmCtrlEntry {{ cmd: {r['cmd']:#010x}, params_size: 0, ptrs: &[], refuse: true }},")
                continue
            print(f"    RmCtrlEntry {{ cmd: {r['cmd']:#010x}, params_size: {r['size']}, refuse: false, ptrs: &[")
            for p in r["ptrs"]:
                if p["kind"] == "fixed":
                    cnt = f"Count::Fixed({p['fixed']})"
                elif p["kind"] == "squared":
                    cnt = f"Count::Squared {{ offset: {p['coff']}, width: {p['cw']} }}"
                else:
                    cnt = f"Count::Field {{ offset: {p['coff']}, width: {p['cw']} }}"
                print(
                    f"        EmbeddedPtr {{ ptr_offset: {p['off']}, count: {cnt}, elem_size: {p['es']}, "
                    f"copy_in: {str(p['cin']).lower()}, copy_out: {str(p['cout']).lower()} }},"
                )
            print("    ] },")
        print("];")
    text = buf.getvalue()
    fmt = shutil.which("rustfmt")
    if fmt:
        r = subprocess.run([fmt, "--edition", "2024"], input=text, capture_output=True, text=True)
        if r.returncode == 0:
            text = r.stdout
        else:
            sys.stderr.write("rustfmt refused this table; writing it unformatted\n")
    else:
        sys.stderr.write("no rustfmt on PATH; writing the table unformatted\n")
    sys.stdout.write(text)
    unparsed = [e for e in entries if e.get("refuse")]
    if unparsed:
        sys.stderr.write("refused (could not describe):\n")
        for e in unparsed:
            sys.stderr.write(f"  {e['label']}: {e['why']}\n")
    sys.stderr.write(f"{len(rows)} controls, {sum(len(r.get('ptrs', [])) for r in rows)} pointers\n")


if __name__ == "__main__":
    main()
