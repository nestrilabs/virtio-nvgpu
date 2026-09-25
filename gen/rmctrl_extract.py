#!/usr/bin/env python3
"""Measure where RM follows user pointers inside RM_CONTROL parameters.

The backend makes every RM_CONTROL itself, so to RM every pointer inside a
control's parameter block is an address in the backend's own process. RM
copies from and to such pointers for a fixed set of commands: the ones
embeddedParamCopyIn() knows (src/nvidia/src/kernel/rmapi/
embedded_param_copy.c), the deprecated V1 controls it converts with the
caller's pointers (src/nvidia/interface/deprecated/
rmapi_deprecated_control.c), and a handful of handlers that copy to or from
user memory themselves. device/src/guestptr.rs zeroes every such pointer the
guest did not send the data for, and refuses the commands whose pointers
have no fixed place. This script is where that table comes from.

For one driver release it:

1. fetches the release (a tag tarball streamed through a filter: the SDK
   headers, the two RM sources whose tables are the ground truth, and every
   RM .c file that calls a user-copy primitive at all);
2. takes the commands and pointer fields from those sources as RM itself
   uses them -- the RMAPI_PARAM_COPY_INIT calls of each `case` of
   embeddedParamCopyIn, and the user copies of each deprecated converter --
   plus the hand-written SELF_COPY and REFUSED entries below, and refuses to
   write anything if the set of RM files that copy from user memory is not
   the one those entries account for (a new release that adds such a
   handler fails here, loudly);
3. compiles and runs a C probe against the release's own SDK headers for
   every command number, every offset and every field width (each must be an
   8-byte NvP64): nothing is transcribed.

    ./rmctrl_extract.py all                # every version in VERSIONS, then render
    ./rmctrl_extract.py extract 610.57.04  # one version (fetches if needed)
    ./rmctrl_extract.py extract 610.57.04 --src ../../nvidia-driver
    ./rmctrl_extract.py render             # gen/rmctrl/*.json -> gen/src/rmctrl/generated.rs
    ./rmctrl_extract.py check              # re-measure everything; fail if stale

`render` needs nothing but Python and is what the Rust staleness test runs
(gen/src/rmctrl/generated.rs, the_checked_in_table_is_what_the_extractor_renders).
`extract`, `all` and `check` need Python 3.8+, gcc, and network access the
first time (sources are cached in $RMCTRL_EXTRACT_CACHE, default
$TMPDIR/ogkm-rm; --cache overrides). Output: gen/rmctrl/<version>.json per
release, and gen/src/rmctrl/generated.rs, the union the backend uses. x86_64 (LP64).
"""

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT_DIR = HERE / "rmctrl"
RUST_OUT = HERE / "src" / "rmctrl" / "generated.rs"
REPO = "NVIDIA/open-gpu-kernel-modules"
FORMAT = "virtio-nvgpu/rmctrl-pointers/1"
DEFAULT_CACHE = Path(os.environ.get("RMCTRL_EXTRACT_CACHE",
                                    Path(tempfile.gettempdir()) / "ogkm-rm"))

# The releases measured: the same set as gen/nvkms (see its README for why
# each is there).
VERSIONS = [
    "535.129.03",
    "580.178.04",
    "595.71.05",
    "595.99.02",
    "610.57.04",
    "615.71.09",
]

EPC = "src/nvidia/src/kernel/rmapi/embedded_param_copy.c"
DEP = "src/nvidia/interface/deprecated/rmapi_deprecated_control.c"

# Paths (relative to the repository root) kept from a release tarball. "**"
# crosses directories, "*" does not.
FETCH = [
    "version.mk",
    "src/common/sdk/nvidia/inc/**",
    "src/common/inc/**",
    EPC,
    DEP,
]
# Of these, only the files that call a user-copy primitive are kept.
FETCH_IF_COPYING = ["src/nvidia/**.c"]
USER_COPY = re.compile(
    rb"portMemExCopy(?:To|From)User|os_memcpy_(?:to|from)_user|"
    rb"rmapiParamsAcquire|RmCopyUserForDeprecatedApi|RMAPI_PARAM_COPY_INIT|"
    rb"\bCopyUser\s*\(")

INCLUDE_DIRS = ["src/common/sdk/nvidia/inc", "src/common/inc"]


class ExtractError(Exception):
    """A source, table or field the extractor relies on is not what it expects."""


# --------------------------------------------------------------------------
# The hand-written part. Names only; every number comes from the compiler.
# --------------------------------------------------------------------------

# Handlers that copy to or from user memory themselves, outside the two
# tables: (source file, params type, pointer fields).
SELF_COPY = {
    "NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO": (
        "src/nvidia/src/kernel/gpu/mem_mgr/mem_mgr_ctrl.c",
        "NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO_PARAMS",
        ["pAllocInfo", "pClientInfo"],
    ),
}

# Commands whose pointers have no fixed place, refused outright.
REFUSED = {
    "NV402C_CTRL_CMD_I2C_TRANSACTION":
        "pMessage sits at a different offset in each arm of a union selected "
        "by transType, over plain fields of the other arms (_i2cTransactionCopyIn)",
    "NV83DE_CTRL_CMD_READ_SURFACE":
        "an array of up to MAX_ACCESS_OPS ops, each with its own pCpuVA "
        "(kernel_sm_debugger_session_ctrl.c)",
    "NV83DE_CTRL_CMD_WRITE_SURFACE":
        "an array of up to MAX_ACCESS_OPS ops, each with its own pCpuVA "
        "(kernel_sm_debugger_session_ctrl.c)",
}

# Every RM file that calls a user-copy primitive, and what accounts for it.
# A release where this set differs fails: a new file may be a new handler
# that follows a pointer in a control's parameters.
KNOWN_USER_COPY_FILES = {
    EPC: "the embedded-pointer table (parsed)",
    DEP: "the deprecated V1 controls (parsed)",
    "src/nvidia/src/kernel/gpu/mem_mgr/mem_mgr_ctrl.c": "SELF_COPY",
    "src/nvidia/src/kernel/gpu/gr/kernel_sm_debugger_session_ctrl.c":
        "REFUSED (READ/WRITE_SURFACE)",
    "src/nvidia/src/kernel/rmapi/control.c":
        "the top-level params block, which dispatch_nested replaces",
    "src/nvidia/src/kernel/rmapi/alloc_free.c":
        "RM_ALLOC's top-level params block, which dispatch_nested replaces",
    "src/nvidia/src/kernel/rmapi/param_copy.c": "the copy primitives themselves",
    "src/nvidia/src/kernel/rmapi/deprecated_context.c": "the copy primitive for DEP",
    "src/nvidia/interface/deprecated/rmapi_gss_legacy_control.c":
        "the top-level params block of GSP legacy controls",
    "src/nvidia/src/kernel/gpu/fifo/kernel_idle_channels.c":
        "NV_ESC_RM_IDLE_CHANNELS, refused by guestptr::rm_escape",
    "src/nvidia/arch/nvalloc/unix/src/escape.c":
        "escape-level blocks (guestptr::rm_escape)",
    "src/nvidia/arch/nvalloc/unix/src/osapi.c":
        "NV_ESC_RM_GET_EVENT_DATA, refused by guestptr::rm_escape",
    "src/nvidia/src/libraries/nvport/memory/memory_unix_kernel_os.c":
        "the portMemExCopy primitives themselves",
    "src/nvidia/interface/deprecated/rmapi_deprecated_misc.c":
        "I2C_ACCESS and IDLE_CHANNELS escapes, refused by guestptr::rm_escape",
    "src/nvidia/interface/deprecated/rmapi_deprecated_allocmemory.c":
        "ALLOC_MEMORY of the NV01_MEMORY_LIST classes, refused by guestptr::rm_escape",
}


# --------------------------------------------------------------------------
# Fetching
# --------------------------------------------------------------------------

def _match(rel, patterns):
    for pat in patterns:
        if pat.endswith("/**"):
            if rel.startswith(pat[:-2]):
                return True
        elif "**" in pat:
            head, tail = pat.split("**", 1)
            if rel.startswith(head) and fnmatch.fnmatchcase(os.path.basename(rel), "*" + tail):
                return True
        elif os.path.dirname(rel) == os.path.dirname(pat) and \
                fnmatch.fnmatchcase(os.path.basename(rel), os.path.basename(pat)):
            return True
    return False


class _HashingReader:
    def __init__(self, f, h):
        self.f, self.h = f, h

    def read(self, n=-1):
        b = self.f.read(n)
        self.h.update(b)
        return b


def fetch(version, cache):
    """Unpack what the extractor reads of `version` into cache/<version>."""
    dest = cache / version
    if (dest / "SOURCE.json").exists():
        return dest
    url = f"https://codeload.github.com/{REPO}/tar.gz/refs/tags/{version}"
    try:
        resp = urllib.request.urlopen(url, timeout=120)
    except urllib.error.HTTPError as e:
        raise ExtractError(f"{url}: {e}") from e
    tmp = cache / f".{version}.partial"
    shutil.rmtree(tmp, ignore_errors=True)
    h = hashlib.sha256()
    commit, nfiles = None, 0
    with resp:
        reader = _HashingReader(resp, h)
        with tarfile.open(fileobj=reader, mode="r|gz") as tf:
            for m in tf:
                if commit is None:
                    commit = tf.pax_headers.get("comment")
                parts = m.name.split("/", 1)
                if len(parts) < 2 or not m.isfile():
                    continue
                rel = parts[1]
                keep = _match(rel, FETCH)
                maybe = not keep and _match(rel, FETCH_IF_COPYING)
                if not keep and not maybe:
                    continue
                with tf.extractfile(m) as src:
                    data = src.read()
                if maybe and not USER_COPY.search(data):
                    continue
                target = tmp / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                nfiles += 1
        while reader.read(1 << 16):
            pass
    if not nfiles:
        raise ExtractError(f"{url}: no wanted files in the tarball")
    (tmp / "SOURCE.json").write_text(json.dumps({
        "tag": version, "commit": commit, "url": url,
        "tarball_sha256": h.hexdigest(), "files": nfiles,
    }, indent=2) + "\n")
    shutil.rmtree(dest, ignore_errors=True)
    tmp.rename(dest)
    return dest


def local_source(src):
    """Describe a local tree (for `extract --src`)."""
    return {"tag": None, "commit": None, "url": None, "tarball_sha256": None,
            "files": None, "local": True}


def user_copy_files(root):
    """Every RM .c file under root that calls a user-copy primitive."""
    found = set()
    for p in (root / "src" / "nvidia").rglob("*.c"):
        if USER_COPY.search(p.read_bytes()):
            found.add(str(p.relative_to(root)))
    return found


# --------------------------------------------------------------------------
# Reading the RM sources
# --------------------------------------------------------------------------

def strip_comments(s):
    s = re.sub(r"/\*.*?\*/", lambda m: " " * len(m.group(0)), s, flags=re.S)
    s = re.sub(r"//[^\n]*", "", s)
    return s


def match_paren(s, i, open_="(", close=")"):
    """Index just past the bracket closing the one at s[i]."""
    depth = 0
    for j in range(i, len(s)):
        if s[j] == open_:
            depth += 1
        elif s[j] == close:
            depth -= 1
            if depth == 0:
                return j + 1
    raise ExtractError(f"unbalanced {open_} at {i}")


def split_args(s):
    """Top-level comma split of an argument list (without the parens)."""
    out, depth, cur = [], 0, []
    for ch in s:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    out.append("".join(cur).strip())
    return out


def calls(s, name):
    """Argument lists of every call of `name` in s."""
    for m in re.finditer(r"\b" + re.escape(name) + r"\s*\(", s):
        start = m.end() - 1
        end = match_paren(s, start)
        yield split_args(s[start + 1:end - 1])


def function_body(s, name):
    m = re.search(r"\b" + re.escape(name) + r"\s*\([^;{]*\)\s*\{", s)
    if not m:
        raise ExtractError(f"no function {name}")
    start = m.end() - 1
    return s[start:match_paren(s, start, "{", "}")]


def field_of(expr):
    """The member designator an argument such as `((T*)pParams)->a.b` names."""
    expr = expr.strip()
    m = re.search(r"->\s*([A-Za-z_][\w.]*)\s*\)*$", expr)
    if not m:
        raise ExtractError(f"cannot read a field from {expr!r}")
    return m.group(1)


def switch_cases(body):
    """(labels, block text) for each case group of the body's switch on the
    command, at the switch's own depth (a nested switch's labels are not
    commands). Labels that fall through to one block share it."""
    m = re.search(r"switch\s*\(\s*pRmCtrlParams->cmd\s*\)\s*\{", body)
    if not m:
        raise ExtractError("no switch (pRmCtrlParams->cmd)")
    start = m.end() - 1
    block = body[start:match_paren(body, start, "{", "}")]
    out, depth, labels, cur = [], 0, [], 0
    for t in re.finditer(r"[{}]|\bcase\s+(\w+)\s*:|\bdefault\s*:", block):
        tok = t.group(0)
        if tok == "{":
            depth += 1
            continue
        if tok == "}":
            depth -= 1
            continue
        if depth != 1:
            continue
        # A label after code ends the group before it; after only
        # whitespace (and preprocessor lines) it falls through to join it.
        between = re.sub(r"^\s*#.*$", "", block[cur:t.start()], flags=re.M)
        if labels and between.strip():
            out.append((labels, block[cur:t.start()]))
            labels = []
        if tok.startswith("case"):
            labels.append(t.group(1))
        else:
            labels = []
        cur = t.end()
    if labels:
        out.append((labels, block[cur:]))
    return out


def parse_epc(text):
    """{command name: (params type, [fields])} from embeddedParamCopyIn."""
    s = strip_comments(text)
    body = function_body(s, "embeddedParamCopyIn")
    found = {}
    for labels, block in switch_cases(body):
        types = [a[1] for a in calls(block, "CHECK_PARAMS_OR_RETURN")]
        inits = list(calls(block, "RMAPI_PARAM_COPY_INIT"))
        if not inits:
            helpers = re.findall(r"\b(_\w+CopyIn)\s*\(", block)
            for name in labels:
                if name not in REFUSED:
                    raise ExtractError(
                        f"{name}: no RMAPI_PARAM_COPY_INIT in its case"
                        + (f" (it calls {helpers})" if helpers else "")
                        + "; add it to REFUSED with the reason, or teach the parser")
            continue
        if len(set(types)) != 1:
            raise ExtractError(f"{labels}: params type is not one of {types}")
        fields = [field_of(a[1]) for a in inits]
        for name in labels:
            found[name] = (types[0], fields)
    if not found:
        raise ExtractError("embeddedParamCopyIn: no commands found")
    return found


def parse_dep(text):
    """{command name: (params type, [fields])} from the deprecated table."""
    s = strip_comments(text)
    m = re.search(r"rmDeprecatedControlTable\s*\[\s*\]\s*=\s*\{", s)
    if not m:
        raise ExtractError("no rmDeprecatedControlTable")
    start = m.end() - 1
    table = s[start:match_paren(s, start, "{", "}")]
    found = {}
    for cmd, handler in re.findall(r"\{\s*(NV\w+)\s*,\s*([^}]*?)\s*\}", table):
        handler = split_args(handler)[0]
        hm = re.fullmatch(r"V2_CONVERTER\((\w+)\)", handler)
        if not hm:
            raise ExtractError(f"{cmd}: handler {handler!r} is not a V2_CONVERTER")
        body = function_body(s, f"V2_CONVERTER({hm.group(1)})")
        ptype, fields = None, []
        for macro in ("CONVERT_TO_V2", "CONVERT_TO_V2_EX", "CTRL_PARAMS_TOKEN_ADD_EMBEDDED"):
            for a in calls(body, macro):
                t, f = a[1], a[3]
                if ptype not in (None, t):
                    raise ExtractError(f"{cmd}: two params types, {ptype} and {t}")
                ptype = t
                fields.append(f)
        decl = re.findall(r"(\w+)\s*\*\s*pParams\s*=", body)
        for fn in ("rmapiParamsCopyOut", "rmapiParamsCopyIn", "RmCopyUserForDeprecatedApi"):
            for a in calls(body, fn):
                for arg in a:
                    fm = re.fullmatch(r"pParams->(\w+)", arg)
                    if fm:
                        if not decl:
                            raise ExtractError(f"{cmd}: pParams used, never declared")
                        if ptype not in (None, decl[0]):
                            raise ExtractError(f"{cmd}: two params types")
                        ptype = decl[0]
                        if fm.group(1) not in fields:
                            fields.append(fm.group(1))
        if not fields:
            raise ExtractError(f"{cmd}: its converter copies through no pointer the parser sees")
        found[cmd] = (ptype, fields)
    return found


def check_self_copy(root):
    for name, (rel, _, fields) in SELF_COPY.items():
        text = strip_comments((root / rel).read_text(errors="replace"))
        for f in fields:
            if not re.search(r"portMemExCopy(?:To|From)User\s*\([^;]*pParams->" + f + r"\b",
                             text, flags=re.S):
                raise ExtractError(f"{name}: {rel} no longer copies through pParams->{f}")


# --------------------------------------------------------------------------
# The probe
# --------------------------------------------------------------------------

def defining_header(root, symbol, kind):
    """The SDK header under ctrl/ or class/ that defines `symbol`."""
    inc = root / INCLUDE_DIRS[0]
    pat = (re.compile(r"#define\s+" + symbol + r"\b") if kind == "macro"
           else re.compile(r"\}\s*" + symbol + r"\s*;"))
    hits = []
    for p in sorted(inc.rglob("*.h")):
        if pat.search(p.read_text(errors="replace")):
            hits.append(str(p.relative_to(inc)))
    return hits


def probe(root, rows, workdir):
    """Compile and run the offset probe; returns {name: {...}}."""
    headers = set()
    for name, ptype, fields in rows:
        for sym, kind in ((name, "macro"), (ptype, "type")):
            h = defining_header(root, sym, kind)
            if h:
                headers.add(h[0])
    lines = ["#include <stdio.h>", "#include <stddef.h>", '#include "nvtypes.h"']
    lines += [f'#include "{h}"' for h in sorted(headers)]
    lines.append("int main(void) {")
    for name, ptype, fields in rows:
        lines.append(f"#ifdef {name}")
        lines.append(f'  printf("ROW {name} %u %zu\\n", (unsigned)({name}), sizeof({ptype}));')
        for f in fields:
            lines.append(f'  printf("F {name} {f} %zu %zu\\n", offsetof({ptype}, {f}), '
                         f'sizeof((({ptype} *)0)->{f}));')
        lines.append("#endif")
    lines.append("  return 0;\n}")
    c = workdir / "probe.c"
    c.write_text("\n".join(lines) + "\n")
    exe = workdir / "probe"
    cmd = ["gcc", "-w", "-DNV_LINUX", "-o", str(exe), str(c)] + \
          [f"-I{root / d}" for d in INCLUDE_DIRS]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode:
        raise ExtractError("probe does not compile:\n" + r.stderr[-4000:])
    out = subprocess.run([str(exe)], capture_output=True, text=True, check=True).stdout
    got = {}
    for line in out.splitlines():
        p = line.split()
        if p[0] == "ROW":
            got[p[1]] = {"cmd": int(p[2]), "size": int(p[3]), "fields": []}
        else:
            name, field, off, width = p[1], p[2], int(p[3]), int(p[4])
            if width != 8 or off % 8:
                raise ExtractError(f"{name}.{field}: {width} bytes at {off}, not an NvP64")
            got[name]["fields"].append({"field": field, "offset": off})
    return got


def extract(version, root, source):
    epc = parse_epc((root / EPC).read_text(errors="replace"))
    dep = parse_dep((root / DEP).read_text(errors="replace"))
    check_self_copy(root)
    copying = user_copy_files(root)
    unknown = sorted(copying - set(KNOWN_USER_COPY_FILES))
    if unknown:
        raise ExtractError(
            f"{version}: RM files that copy user memory and nothing here accounts for: "
            f"{unknown}. Read them; a handler that follows a pointer in a control's "
            f"parameters belongs in SELF_COPY or REFUSED.")
    rows, origin = [], {}
    for table, what in ((epc, "embedded"), (dep, "deprecated")):
        for name, (ptype, fields) in table.items():
            if name in origin:
                # In both (BIOS_GET_INFO): whichever path RM takes, each
                # pointer either names is followed.
                i = next(k for k, r in enumerate(rows) if r[0] == name)
                if rows[i][1] != ptype:
                    raise ExtractError(f"{name}: {rows[i][1]} in one table, {ptype} in the other")
                rows[i] = (name, ptype, rows[i][2] + [f for f in fields if f not in rows[i][2]])
                origin[name] += "+" + what
                continue
            rows.append((name, ptype, fields))
            origin[name] = what
    for name, (_, ptype, fields) in SELF_COPY.items():
        rows.append((name, ptype, fields))
        origin[name] = "self-copy"
    with tempfile.TemporaryDirectory() as d:
        got = probe(root, rows, Path(d))
        refused = {}
        with tempfile.TemporaryDirectory() as d2:
            c = Path(d2) / "r.c"
            hs = set()
            for n in REFUSED:
                h = defining_header(root, n, "macro")
                if h:
                    hs.add(h[0])
            c.write_text("#include <stdio.h>\n#include \"nvtypes.h\"\n"
                         + "".join(f'#include "{h}"\n' for h in sorted(hs))
                         + "int main(void) {\n"
                         + "".join(f'#ifdef {n}\n  printf("{n} %u\\n", (unsigned)({n}));\n#endif\n'
                                   for n in REFUSED)
                         + "  return 0;\n}\n")
            exe = Path(d2) / "r"
            r = subprocess.run(["gcc", "-w", "-DNV_LINUX", "-o", str(exe), str(c)]
                               + [f"-I{root / d}" for d in INCLUDE_DIRS],
                               capture_output=True, text=True)
            if r.returncode:
                raise ExtractError("refusal probe does not compile:\n" + r.stderr[-4000:])
            for line in subprocess.run([str(exe)], capture_output=True, text=True,
                                       check=True).stdout.splitlines():
                n, v = line.split()
                refused[n] = int(v)
    controls = []
    for name, _, _ in rows:
        if name not in got:
            continue  # not defined by this release's headers (an #ifdef'd case)
        g = got[name]
        controls.append({"name": name, "cmd": g["cmd"], "from": origin[name],
                         "params_size": g["size"], "pointers": g["fields"]})
    controls.sort(key=lambda c: (c["cmd"], c["name"]))
    return {
        "format": FORMAT,
        "version": version,
        "abi": "x86_64 LP64",
        "source": source,
        "controls": controls,
        "refused": [{"name": n, "cmd": v, "why": REFUSED[n]}
                    for n, v in sorted(refused.items(), key=lambda x: x[1])],
    }


# --------------------------------------------------------------------------
# Rendering the union for the backend
# --------------------------------------------------------------------------

def render(data):
    """gen/src/rmctrl/generated.rs from the per-release measurements."""
    by_cmd, refused = {}, {}
    versions = [d["version"] for d in data]
    for d in data:
        for c in d["controls"]:
            e = by_cmd.setdefault(c["cmd"], {"names": [], "ptrs": set(), "in": []})
            if c["name"] not in e["names"]:
                e["names"].append(c["name"])
            e["ptrs"].update(p["offset"] for p in c["pointers"])
            e["in"].append(d["version"])
        for r in d["refused"]:
            refused.setdefault(r["cmd"], r["name"])
    for cmd in refused:
        if cmd in by_cmd:
            raise ExtractError(f"{cmd:#x} is both refused and listed")
    out = [
        "// @generated by gen/rmctrl_extract.py from gen/rmctrl/*.json. DO NOT EDIT --",
        "// re-measure with `gen/rmctrl_extract.py all`.",
        "//",
        "// Every RM control whose parameters hold a user pointer RM follows, and",
        "// the offsets of those pointers: the union over " + ", ".join(versions) + ".",
        "// A command number with different pointers in different releases lists",
        "// all of them; an offset past a release's block is one it lacks.",
        "",
        "/// A control and the offsets of the pointers RM follows in its parameters.",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct ControlPointers {",
        "    pub cmd: u32,",
        "    pub name: &'static str,",
        "    pub ptrs: &'static [usize],",
        "}",
        "",
        "pub const CONTROL_POINTERS: &[ControlPointers] = &[",
    ]
    for cmd in sorted(by_cmd):
        e = by_cmd[cmd]
        ptrs = ", ".join(str(p) for p in sorted(e["ptrs"]))
        note = "" if len(e["in"]) == len(versions) else f" // only {', '.join(e['in'])}"
        out.append("    ControlPointers {")
        out.append(f"        cmd: {cmd:#010x},")
        out.append(f"        name: \"{' / '.join(e['names'])}\",")
        out.append(f"        ptrs: &[{ptrs}],{note}")
        out.append("    },")
    out += [
        "];",
        "",
        "/// Controls whose pointers have no fixed place in their parameters.",
        "pub const REFUSED_CONTROLS: &[(u32, &str)] = &[",
    ]
    for cmd in sorted(refused):
        out.append(f"    ({cmd:#010x}, \"{refused[cmd]}\"),")
    out += ["];", ""]
    return "\n".join(out)


def load_all(out_dir):
    data = []
    for v in VERSIONS:
        p = out_dir / f"{v}.json"
        if not p.exists():
            raise ExtractError(f"{p} missing; run `{sys.argv[0]} extract {v}`")
        data.append(json.loads(p.read_text()))
    return data


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
    r.add_argument("--out", type=Path, help="write generated.rs here instead of gen/src/rmctrl/")
    sub.add_parser("check")
    for p in (e, sub.choices["all"], sub.choices["check"]):
        p.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    a = ap.parse_args()
    try:
        if a.cmd == "extract":
            if a.src:
                root, source = a.src.resolve(), local_source(a.src.resolve())
            else:
                a.cache.mkdir(parents=True, exist_ok=True)
                root = fetch(a.version, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
            write_json(OUT_DIR / f"{a.version}.json", extract(a.version, root, source))
        elif a.cmd == "all":
            a.cache.mkdir(parents=True, exist_ok=True)
            for v in VERSIONS:
                root = fetch(v, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
                write_json(OUT_DIR / f"{v}.json", extract(v, root, source))
            RUST_OUT.write_text(render(load_all(OUT_DIR)))
        elif a.cmd == "render":
            text = render(load_all(OUT_DIR))
            if a.out:
                a.out.mkdir(parents=True, exist_ok=True)
                (a.out / "generated.rs").write_text(text)
            else:
                RUST_OUT.write_text(text)
        elif a.cmd == "check":
            a.cache.mkdir(parents=True, exist_ok=True)
            stale = []
            for v in VERSIONS:
                root = fetch(v, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
                fresh = extract(v, root, source)
                p = OUT_DIR / f"{v}.json"
                if not p.exists() or json.loads(p.read_text()) != fresh:
                    stale.append(str(p))
            if RUST_OUT.read_text() != render(load_all(OUT_DIR)):
                stale.append(str(RUST_OUT))
            if stale:
                print("stale: " + ", ".join(stale) + f"; run {sys.argv[0]} all", file=sys.stderr)
                return 1
            print("gen/rmctrl and gen/src/rmctrl/generated.rs are up to date")
    except ExtractError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
