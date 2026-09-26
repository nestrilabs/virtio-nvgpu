#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
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
   8-byte NvP64): nothing is transcribed;
4. reads, for each embedded pointer, how much RM copies through it -- the
   count and element-size arguments of its RMAPI_PARAM_COPY_INIT, the counts'
   offsets and widths and the element's size from the same probe -- and the
   same for NV_ESC_RM_IDLE_CHANNELS' arrays from RmDeprecatedIdleChannels.
   Those sizes are what the backend checks the guest's deep segments against
   (device/src/deepseg.rs), and the guest's copy of the rows it sends as
   segments is rendered alongside (driver/gen/nvgpu_rm_deep.h).

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
C_OUT = HERE.parent / "driver" / "gen" / "nvgpu_rm_deep.h"
REPO = "NVIDIA/open-gpu-kernel-modules"
FORMAT = "virtio-nvgpu/rmctrl-pointers/2"
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

# Controls whose pointers are zeroed and never relocated, whatever the guest
# sends: the backend refuses a deep block for any of them. Every control with
# more than one pointer is either here or has RM's size read for each
# pointer (parse_size_rule); `extract` fails otherwise.
LEFT_ZEROED = {
    "NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION":
        "answered in the guest (nvgpu_rm_intercepts.h); RM's deprecated "
        "converter sizes the strings itself",
    "NV0000_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD":
        "runs an ACPI method on the host's firmware; not a VM's to call",
    "NV0073_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD":
        "runs an ACPI method on the host's firmware; not a VM's to call",
    "NV2080_CTRL_CMD_FB_GET_AMAP_CONF":
        "copied only under USE_AMAPLIB, which no open build defines, with "
        "sizes of amaplib types the SDK does not have",
    "NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO":
        "served only by DEBUG/DEVELOP builds (mem_mgr_ctrl.c), and it lists "
        "every client's host PID",
    "NV2080_CTRL_CMD_GSP_CRYPTO_CONTROL":
        "PRIVILEGED and RM_TEST_ONLY_CODE (0x100044)",
}

# Controls RM's pointer tables name whose command macro a release's headers
# do not define: left out of the table (there is no number to key them by),
# which is safe only because nothing can send a control that has no number
# in the release it runs on. Each checked by hand; any other name missing
# stops `extract`.
UNDEFINED_IN_HEADERS = {
    "NV0000_CTRL_CMD_OS_GET_CAPS":
        "embedded_param_copy.c guards its cases with #ifdef NV0000_CTRL_CMD_OS_GET_CAPS "
        "and no header defines it: RM compiles no such case and has no such control",
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


INT_LITERAL = re.compile(r"(0[xX][0-9a-fA-F]+|[0-9]+)[uU]?")


def _one_line(expr):
    return " ".join(expr.split())


def _count_field(expr):
    """The parameter field a count expression reads, or None: an argument of
    the form `((T*)pParams)->f` or `pParams->f` (callers require pParams)."""
    try:
        return field_of(expr)
    except ExtractError:
        return None


def parse_size_rule(block, dest, field, num, size):
    """How much RM copies through the pointer `field`, as RM computes it:
    `RMAPI_PARAM_COPY_INIT(dest, .., .., num, size)` sets paramsSize to
    num * size (portSafeMulU32; an overflow is RM's NV_ERR_INVALID_ARGUMENT),
    with num evaluated in NvU32. Returns None for any form not read here --
    the pointer is then never relocated, only zeroed.

    num is a parameter field, a literal, or a local set from a product of
    fields when the pointer is set (GET_P2P_CAPS's gpuCount * gpuCount);
    size is sizeof(T) or a literal. The directions are the SKIP_COPYIN and
    SKIP_COPYOUT flags set on `dest` in the same case."""
    num, size = _one_line(num), _one_line(size)
    rule = {"scale": 1, "count": [], "elem": None, "elem_type": None,
            "if_nonnull": False}
    m = INT_LITERAL.fullmatch(num)
    if m:
        rule["scale"] = int(m.group(1), 0)
    elif re.fullmatch(r"[A-Za-z_]\w*", num):
        # A local: accepted only as `if (NvP64_VALUE(<this pointer>) != NULL)
        # { num = a * b ...; }`, zero otherwise (RM initialises it to 0).
        pat = (r"if\s*\(\s*NvP64_VALUE\s*\((.*?)\)\s*!=\s*NULL\s*\)\s*\{\s*"
               + re.escape(num) + r"\s*=\s*([^;]+);\s*\}")
        hit = [h for h in re.finditer(pat, block, flags=re.S)
               if _count_field(h.group(1)) == field]
        if len(hit) != 1:
            return None
        factors, depth, cur = [], 0, []
        for ch in hit[0].group(2):
            depth += ch in "([{"
            depth -= ch in ")]}"
            if ch == "*" and depth == 0:
                factors.append("".join(cur))
                cur = []
            else:
                cur.append(ch)
        factors.append("".join(cur))
        parts = [_count_field(p) for p in factors]
        if not parts or None in parts:
            return None
        rule["count"] = parts
        rule["if_nonnull"] = True
    else:
        f = _count_field(num)
        if f is None or "pParams" not in num:
            return None
        rule["count"] = [f]
    m = INT_LITERAL.fullmatch(size)
    if m:
        rule["elem"] = int(m.group(1), 0)
    else:
        m = re.fullmatch(r"sizeof\s*\(\s*([A-Za-z_]\w*)\s*\)", size)
        if not m:
            return None
        rule["elem_type"] = m.group(1)
    flags = set(re.findall(re.escape(dest) + r"\.flags\s*\|=\s*(RMAPI_PARAM_COPY_FLAGS_\w+)",
                           block))
    if re.search(re.escape(dest) + r"\.flags\s*=", block):
        return None
    rule["copy_in"] = "RMAPI_PARAM_COPY_FLAGS_SKIP_COPYIN" not in flags
    rule["copy_out"] = "RMAPI_PARAM_COPY_FLAGS_SKIP_COPYOUT" not in flags
    return rule


def parse_epc(text):
    """{command name: (params type, [fields], {field: size rule or None})}
    from embeddedParamCopyIn."""
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
        rules = {}
        for a in inits:
            if len(a) != 5:
                raise ExtractError(f"{labels}: RMAPI_PARAM_COPY_INIT with {len(a)} arguments")
            f = field_of(a[1])
            if field_of(a[2]) != f:
                raise ExtractError(f"{labels}: copies {f} to {field_of(a[2])}")
            rules[f] = parse_size_rule(block, a[0].strip(), f, a[3], a[4])
        for name in labels:
            found[name] = (types[0], fields, rules)
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
    """Compile and run the offset probe; returns {name: {...}}.

    rows are (name, params type, [pointer fields], {field: size rule}); for
    each rule the probe also measures its count fields (offset and width)
    and its element type's size."""
    headers = set()
    for name, ptype, fields, rules in rows:
        syms = [(name, "macro"), (ptype, "type")]
        syms += [(r["elem_type"], "type") for r in rules.values()
                 if r and r["elem_type"] and not r["elem_type"].startswith("Nv")]
        for sym, kind in syms:
            h = defining_header(root, sym, kind)
            if h:
                headers.add(h[0])
    lines = ["#include <stdio.h>", "#include <stddef.h>", '#include "nvtypes.h"']
    lines += [f'#include "{h}"' for h in sorted(headers)]
    lines.append("int main(void) {")
    for name, ptype, fields, rules in rows:
        lines.append(f"#ifdef {name}")
        lines.append(f'  printf("ROW {name} %u %zu\\n", (unsigned)({name}), sizeof({ptype}));')
        for f in fields:
            lines.append(f'  printf("F {name} {f} %zu %zu\\n", offsetof({ptype}, {f}), '
                         f'sizeof((({ptype} *)0)->{f}));')
            r = rules.get(f)
            if not r:
                continue
            for c in r["count"]:
                lines.append(f'  printf("C {name} {f} {c} %zu %zu\\n", offsetof({ptype}, {c}), '
                             f'sizeof((({ptype} *)0)->{c}));')
            if r["elem_type"]:
                lines.append(f'  printf("E {name} {f} %zu\\n", sizeof({r["elem_type"]}));')
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
            got[p[1]] = {"cmd": int(p[2]), "size": int(p[3]), "fields": [],
                         "counts": {}, "elems": {}}
        elif p[0] == "F":
            name, field, off, width = p[1], p[2], int(p[3]), int(p[4])
            if width != 8 or off % 8:
                raise ExtractError(f"{name}.{field}: {width} bytes at {off}, not an NvP64")
            got[name]["fields"].append({"field": field, "offset": off})
        elif p[0] == "C":
            name, field, cf, off, width = p[1], p[2], p[3], int(p[4]), int(p[5])
            if width not in (1, 2, 4):
                raise ExtractError(f"{name}.{cf}: a count of {width} bytes; RM takes an NvU32")
            got[name]["counts"].setdefault(field, []).append(
                {"field": cf, "offset": off, "width": width})
        else:
            got[p[1]]["elems"][p[2]] = int(p[3])
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
    dep = {n: (t, f, {}) for n, (t, f) in dep.items()}
    for table, what in ((epc, "embedded"), (dep, "deprecated")):
        for name, (ptype, fields, rules) in table.items():
            if name in origin:
                # In both (BIOS_GET_INFO): whichever path RM takes, each
                # pointer either names is followed. How much it copies
                # depends on the path, so no size is recorded.
                i = next(k for k, r in enumerate(rows) if r[0] == name)
                if rows[i][1] != ptype:
                    raise ExtractError(f"{name}: {rows[i][1]} in one table, {ptype} in the other")
                rows[i] = (name, ptype, rows[i][2] + [f for f in fields if f not in rows[i][2]],
                           {})
                origin[name] += "+" + what
                continue
            rows.append((name, ptype, fields, rules))
            origin[name] = what
    for name, (_, ptype, fields) in SELF_COPY.items():
        rows.append((name, ptype, fields, {}))
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
    # A control RM's tables name with a pointer in its parameters, whose
    # command macro this release's headers do not define, would be left out
    # of the table -- and a control left out is one whose pointer reaches RM
    # as the guest's bytes. Only the names below, each checked by hand, may
    # be missing; any other stops the extraction.
    missing = sorted(name for name, *_ in rows if name not in got)
    unexplained = [n for n in missing if n not in UNDEFINED_IN_HEADERS]
    if unexplained:
        raise ExtractError(
            f"{version}: controls with pointer parameters in RM's tables, but no command "
            f"macro in this release's headers: {unexplained}. Find why (an #ifdef, a "
            f"renamed macro) and either probe them or add them to UNDEFINED_IN_HEADERS "
            f"with the reason")
    controls = []
    for name, _, _, rules in rows:
        if name not in got:
            continue  # UNDEFINED_IN_HEADERS: checked above
        g = got[name]
        pointers = []
        for p in g["fields"]:
            r = rules.get(p["field"])
            size = None
            if r:
                size = {
                    "scale": r["scale"],
                    "count": g["counts"].get(p["field"], []),
                    "elem": r["elem"] if r["elem"] is not None else g["elems"][p["field"]],
                    "copy_in": r["copy_in"],
                    "copy_out": r["copy_out"],
                    "if_nonnull": r["if_nonnull"],
                }
                if [c["field"] for c in size["count"]] != r["count"]:
                    raise ExtractError(f"{name}.{p['field']}: counts {r['count']} not measured")
            pointers.append({**p, "size": size})
        c = {"name": name, "cmd": g["cmd"], "from": origin[name],
             "params_size": g["size"], "pointers": pointers}
        # Relocated (the guest may send what each pointer addresses, and the
        # backend checks every length against the size rule) only when RM's
        # size is known for every pointer and nothing here says otherwise.
        # A control with several pointers must be one or the other on purpose.
        if name in LEFT_ZEROED:
            c["relocate"] = False
            c["why"] = LEFT_ZEROED[name]
        elif all(p["size"] for p in pointers):
            c["relocate"] = True
        elif len(pointers) > 1:
            raise ExtractError(
                f"{version}: {name} has {len(pointers)} pointers and RM's size for "
                f"{[p['field'] for p in pointers if not p['size']]} is not read here; "
                f"teach parse_size_rule its form, or add it to LEFT_ZEROED with the reason")
        else:
            c["relocate"] = False
            c["why"] = "RM's size for it is not read here"
        controls.append(c)
    controls.sort(key=lambda c: (c["cmd"], c["name"]))
    return {
        "format": FORMAT,
        "version": version,
        "abi": "x86_64 LP64",
        "source": source,
        "controls": controls,
        "refused": [{"name": n, "cmd": v, "why": REFUSED[n]}
                    for n, v in sorted(refused.items(), key=lambda x: x[1])],
        "idle_channels": idle_channels(root),
    }


# NV_ESC_RM_IDLE_CHANNELS is an escape, not a control: its parameters are the
# top-level NVOS30 block, and RmDeprecatedIdleChannels copies its three handle
# arrays itself.
IDLE_SRC = "src/nvidia/interface/deprecated/rmapi_deprecated_misc.c"
IDLE_TYPE = "NVOS30_PARAMETERS"


def idle_channels(root):
    """The three arrays NV_ESC_RM_IDLE_CHANNELS copies in, and when: read
    from RmDeprecatedIdleChannels (the size is `portSafeMulU32(count,
    sizeof(T))`, each array a COPYIN of that size, only for a channel list
    -- DRF_VAL(OS30, _FLAGS, _CHANNEL, flags) == LIST -- with a nonzero
    count), measured against nvos.h."""
    s = strip_comments((root / IDLE_SRC).read_text(errors="replace"))
    body = function_body(s, "RmDeprecatedIdleChannels")
    muls = [a for a in calls(body, "portSafeMulU32")]
    if len(muls) != 1:
        raise ExtractError(f"RmDeprecatedIdleChannels: {len(muls)} portSafeMulU32 calls")
    m_count = re.fullmatch(r"pArgs->(\w+)", muls[0][0])
    m_elem = re.fullmatch(r"sizeof\s*\(\s*(\w+)\s*\)", muls[0][1])
    m_var = re.fullmatch(r"&\s*(\w+)", muls[0][2])
    if not (m_count and m_elem and m_var):
        raise ExtractError(f"RmDeprecatedIdleChannels: size is {muls[0]}")
    ptrs = []
    for a in calls(body, "CopyUser"):
        if a[1] == "RMAPI_DEPRECATED_COPYRELEASE":
            continue
        pm = re.fullmatch(r"pArgs->(\w+)", a[3])
        if a[1] != "RMAPI_DEPRECATED_COPYIN" or not pm or a[4] != m_var.group(1):
            raise ExtractError(f"RmDeprecatedIdleChannels: a copy {a} not of the form read here")
        ptrs.append(pm.group(1))
    if len(ptrs) != 3:
        raise ExtractError(f"RmDeprecatedIdleChannels: copies {ptrs}")
    if not re.search(r"DRF_VAL\s*\(\s*OS30\s*,\s*_FLAGS\s*,\s*_CHANNEL\s*,\s*pArgs->flags\s*\)"
                     r"\s*==\s*NVOS30_FLAGS_CHANNEL_LIST\s*&&\s*params\.numChannels\s*\)", body):
        raise ExtractError("RmDeprecatedIdleChannels: the copies' condition is not the one "
                           "read here")
    nvos = (root / INCLUDE_DIRS[0] / "nvos.h").read_text(errors="replace")
    rng = re.search(r"#define\s+NVOS30_FLAGS_CHANNEL\s+(\d+):(\d+)", nvos)
    if not rng:
        raise ExtractError("nvos.h: no NVOS30_FLAGS_CHANNEL range")
    hi, lo = int(rng.group(1)), int(rng.group(2))
    fields = ptrs + [m_count.group(1), "flags"]
    lines = ["#include <stdio.h>", "#include <stddef.h>", '#include "nvtypes.h"',
             '#include "nvos.h"', "int main(void) {",
             f'  printf("SIZE %zu\\n", sizeof({IDLE_TYPE}));',
             f'  printf("ELEM %zu\\n", sizeof({m_elem.group(1)}));',
             '  printf("LIST %u\\n", (unsigned)(NVOS30_FLAGS_CHANNEL_LIST));']
    lines += [f'  printf("F {f} %zu %zu\\n", offsetof({IDLE_TYPE}, {f}), '
              f'sizeof((({IDLE_TYPE} *)0)->{f}));' for f in fields]
    lines.append("  return 0;\n}")
    with tempfile.TemporaryDirectory() as d:
        c, exe = Path(d) / "idle.c", Path(d) / "idle"
        c.write_text("\n".join(lines) + "\n")
        r = subprocess.run(["gcc", "-w", "-DNV_LINUX", "-o", str(exe), str(c)]
                           + [f"-I{root / i}" for i in INCLUDE_DIRS],
                           capture_output=True, text=True)
        if r.returncode:
            raise ExtractError("IDLE_CHANNELS probe does not compile:\n" + r.stderr[-4000:])
        out = subprocess.run([str(exe)], capture_output=True, text=True, check=True).stdout
    got, off = {}, {}
    for line in out.splitlines():
        p = line.split()
        if p[0] == "F":
            off[p[1]] = (int(p[2]), int(p[3]))
        else:
            got[p[0]] = int(p[1])
    for f in ptrs:
        if off[f][1] != 8 or off[f][0] % 8:
            raise ExtractError(f"{IDLE_TYPE}.{f}: {off[f][1]} bytes at {off[f][0]}, not an NvP64")
    cnt, flags = off[m_count.group(1)], off["flags"]
    if cnt[1] != 4 or flags[1] != 4:
        raise ExtractError(f"{IDLE_TYPE}: count or flags not 4 bytes")
    size = {"scale": 1, "count": [{"field": m_count.group(1), "offset": cnt[0], "width": 4}],
            "elem": got["ELEM"], "copy_in": True, "copy_out": False, "if_nonnull": False}
    return {
        "name": "NV_ESC_RM_IDLE_CHANNELS",
        "params_size": got["SIZE"],
        "pointers": [{"field": f, "offset": off[f][0], "size": size} for f in ptrs],
        "list_when": {"offset": flags[0], "lo": lo, "hi": hi, "value": got["LIST"]},
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
        "// SPDX-License-Identifier: Apache-2.0",
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
    deep, zeroed = deep_union(data)
    out += [
        "/// A count RM reads to size a copy: `width` bytes, little-endian, at",
        "/// `offset` in the control's parameters.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct CountField {",
        "    pub offset: usize,",
        "    pub width: usize,",
        "}",
        "",
        "/// How much RM copies through the pointer at `ptr`, as its",
        "/// RMAPI_PARAM_COPY_INIT computes it: `scale` times the `counts`, in",
        "/// NvU32 arithmetic, times `elem`, which must not overflow. `copy_in` and",
        "/// `copy_out` are false where RM skips that direction.",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub struct DeepPtr {",
        "    pub ptr: usize,",
        "    pub scale: u32,",
        "    pub counts: &'static [CountField],",
        "    pub elem: u32,",
        "    pub copy_in: bool,",
        "    pub copy_out: bool,",
        "}",
        "",
        "/// A control whose every pointer the guest may send the data for, one",
        "/// deep segment per pointer, sized by the rule for it.",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct DeepControl {",
        "    pub cmd: u32,",
        "    pub name: &'static str,",
        "    pub ptrs: &'static [DeepPtr],",
        "}",
        "",
        "/// Every control in CONTROL_POINTERS whose sizes are read from RM's",
        "/// embeddedParamCopyIn, identically in every release that has it.",
        "#[rustfmt::skip]",
        "pub const DEEP_CONTROLS: &[DeepControl] = &[",
    ]
    for cmd in sorted(deep):
        e = deep[cmd]
        out.append("    DeepControl {")
        out.append(f"        cmd: {cmd:#010x},")
        out.append(f"        name: \"{e['name']}\",")
        out.append("        ptrs: &[")
        for p in e["ptrs"]:
            s = p["size"]
            counts = ", ".join(f"CountField {{ offset: {c['offset']}, width: {c['width']} }}"
                               for c in s["count"])
            out.append("            DeepPtr {")
            out.append(f"                ptr: {p['offset']},")
            out.append(f"                scale: {s['scale']},")
            out.append(f"                counts: &[{counts}],")
            out.append(f"                elem: {s['elem']},")
            out.append(f"                copy_in: {str(s['copy_in']).lower()},")
            out.append(f"                copy_out: {str(s['copy_out']).lower()},")
            out.append("            },")
        out.append("        ],")
        out.append("    },")
    out += [
        "];",
        "",
        "/// Controls whose pointers are always zeroed: no deep block is taken",
        "/// for them, single or segmented (rmctrl_extract.py, LEFT_ZEROED).",
        "pub const ZEROED_CONTROLS: &[(u32, &str)] = &[",
    ]
    for cmd in sorted(zeroed):
        out.append(f"    ({cmd:#010x}, \"{zeroed[cmd]}\"),")
    out += ["];", ""]
    idle = idle_union(data)
    w = idle["list_when"]
    out += [
        "/// NV_ESC_RM_IDLE_CHANNELS, an escape: NVOS30's three handle arrays,",
        "/// which RmDeprecatedIdleChannels copies in only for a channel list --",
        "/// `flags` bits IDLE_CHANNELS_LIST_BITS equal to IDLE_CHANNELS_LIST --",
        "/// with a nonzero count. `cmd` is unused.",
        "#[rustfmt::skip]",
        "pub const IDLE_CHANNELS: DeepControl = DeepControl {",
        "    cmd: 0,",
        f"    name: \"{idle['name']}\",",
        "    ptrs: &[",
    ]
    for p in idle["pointers"]:
        s = p["size"]
        c = s["count"][0]
        out += [
            "        DeepPtr {",
            f"            ptr: {p['offset']},",
            f"            scale: {s['scale']},",
            f"            counts: &[CountField {{ offset: {c['offset']}, width: {c['width']} }}],",
            f"            elem: {s['elem']},",
            f"            copy_in: {str(s['copy_in']).lower()},",
            f"            copy_out: {str(s['copy_out']).lower()},",
            "        },",
        ]
    out += [
        "    ],",
        "};",
        f"pub const IDLE_CHANNELS_SIZE: usize = {idle['params_size']};",
        f"pub const IDLE_CHANNELS_FLAGS: usize = {w['offset']};",
        f"pub const IDLE_CHANNELS_LIST_BITS: (u32, u32) = ({w['lo']}, {w['hi']});",
        f"pub const IDLE_CHANNELS_LIST: u32 = {w['value']};",
        "",
    ]
    return "\n".join(out)


def idle_union(data):
    """IDLE_CHANNELS, which must measure the same in every release."""
    first = data[0]["idle_channels"]
    for d in data[1:]:
        if d["idle_channels"] != first:
            raise ExtractError(f"IDLE_CHANNELS differs between {data[0]['version']} and "
                               f"{d['version']}")
    return first


# The most pointers one control's segments may relocate, and the most count
# fields one size multiplies: protocol::messages::DEEP_SEGS_MAX and the C
# table's array bounds.
DEEP_SEGS_MAX = 4
DEEP_COUNTS_MAX = 2


def deep_union(data):
    """({cmd: {name, ptrs}} of relocatable controls, {cmd: name} of zeroed
    ones) over every release. As for CONTROL_POINTERS, a pointer a later
    release adds is listed with the others (GET_P2P_CAPS's busEgmPeerIds;
    in a release without it the offset is past the block's end). A control
    is relocated only if it is in every release that has it: MSENC_GET_CAPS
    and FB_GET_INFO are embedded in 535 and deprecated after, where RM's
    converter sizes the copy. A pointer with different rules in two releases
    fails: which to believe is a decision, not a union."""
    seen, zeroed = {}, {}
    for d in data:
        for c in d["controls"]:
            if c["name"] in LEFT_ZEROED:
                zeroed[c["cmd"]] = c["name"]
            e = seen.setdefault(c["cmd"], {"name": c["name"], "relocate": True, "ptrs": {}})
            e["relocate"] &= c["relocate"]
            if not c["relocate"]:
                continue
            for p in c["pointers"]:
                prev = e["ptrs"].setdefault(p["offset"], (p, d["version"]))
                if prev[0] != p:
                    raise ExtractError(
                        f"{c['name']} ({c['cmd']:#x}): the pointer at {p['offset']} is "
                        f"{prev[0]} in {prev[1]} and {p} in {d['version']}")
    deep = {}
    for cmd, e in seen.items():
        if not e["relocate"]:
            continue
        ptrs = [p for _, (p, _) in sorted(e["ptrs"].items())]
        if len(ptrs) > DEEP_SEGS_MAX:
            raise ExtractError(f"{e['name']}: {len(ptrs)} pointers, over {DEEP_SEGS_MAX}")
        for p in ptrs:
            if len(p["size"]["count"]) > DEEP_COUNTS_MAX:
                raise ExtractError(f"{e['name']}.{p['field']}: over {DEEP_COUNTS_MAX} counts")
        deep[cmd] = {"name": e["name"], "ptrs": ptrs}
    return deep, zeroed


def render_c(data):
    """driver/gen/nvgpu_rm_deep.h: the guest's copy of the multi-pointer rows
    of DEEP_CONTROLS. Single-pointer controls go by the one deep block
    (nvgpu_main.c) and are not listed."""
    deep, _ = deep_union(data)
    out = [
        "/* SPDX-License-Identifier: GPL-2.0-only */",
        "/*",
        " * @generated by gen/rmctrl_extract.py from gen/rmctrl/<release>.json. DO NOT",
        " * EDIT -- re-measure with `gen/rmctrl_extract.py all`.",
        " *",
        " * RM controls with more than one embedded pointer that the guest sends",
        " * as deep segments (NVGPU_DEEP_SEGMENTED), one per pointer, and how much",
        " * RM copies through each: scale times the counts (NvU32 arithmetic)",
        " * times elem, measured from each release's embeddedParamCopyIn and SDK",
        " * headers. The backend's copy, which it checks every segment against, is",
        " * DEEP_CONTROLS in gen/src/rmctrl/generated.rs, rendered from the same",
        " * measurements.",
        " */",
        "",
        "#ifndef NVGPU_RM_DEEP_H",
        "#define NVGPU_RM_DEEP_H",
        "",
        "#include <linux/types.h>",
        "",
        f"#define NVGPU_RM_DEEP_PTRS_MAX {DEEP_SEGS_MAX}",
        f"#define NVGPU_RM_DEEP_COUNTS_MAX {DEEP_COUNTS_MAX}",
        "",
        "/* RM copies the buffer in / out (no SKIP_COPYIN / SKIP_COPYOUT). */",
        "#define NVGPU_RM_DEEP_IN 1",
        "#define NVGPU_RM_DEEP_OUT 2",
        "",
        "struct nvgpu_rm_deep_count {",
        "  u16 offset; /* in the control's parameters */",
        "  u8 width;   /* bytes, little-endian */",
        "};",
        "",
        "struct nvgpu_rm_deep_ptr {",
        "  u16 ptr; /* offset of the NvP64 */",
        "  u8 flags;",
        "  u8 ncounts;",
        "  struct nvgpu_rm_deep_count counts[NVGPU_RM_DEEP_COUNTS_MAX];",
        "  u32 scale;",
        "  u32 elem;",
        "};",
        "",
        "struct nvgpu_rm_deep_control {",
        "  u32 cmd;",
        "  u32 nptrs;",
        "  struct nvgpu_rm_deep_ptr ptrs[NVGPU_RM_DEEP_PTRS_MAX];",
        "};",
        "",
        "static const struct nvgpu_rm_deep_control nvgpu_rm_deep_table[] = {",
    ]
    for cmd in sorted(deep):
        e = deep[cmd]
        if len(e["ptrs"]) < 2:
            continue
        out.append(f"    /* {e['name']} */")
        out.append(f"    {{{cmd:#010x},")
        out.append(f"     {len(e['ptrs'])},")
        out.append("     {")
        for p in e["ptrs"]:
            s = p["size"]
            flags = " | ".join(n for n, on in (("NVGPU_RM_DEEP_IN", s["copy_in"]),
                                               ("NVGPU_RM_DEEP_OUT", s["copy_out"])) if on) or "0"
            counts = ", ".join(f"{{{c['offset']}, {c['width']}}}" for c in s["count"]) or "{0, 0}"
            out.append(f"         {{{p['offset']}, {flags}, {len(s['count'])}, {{{counts}}}, "
                       f"{s['scale']}, {s['elem']}}}, /* {p['field']} */")
        out.append("     }},")
    idle = idle_union(data)
    w = idle["list_when"]
    out += [
        "};",
        "",
        "/*",
        " * NV_ESC_RM_IDLE_CHANNELS (NVOS30), an escape: its three handle arrays,",
        " * copied in by RmDeprecatedIdleChannels only for a channel list -- flags",
        " * bits LIST_HI:LIST_LO equal to LIST -- with a nonzero count. cmd unused.",
        " */",
        f"#define NVGPU_RM_IDLE_CHANNELS_SIZE {idle['params_size']}",
        f"#define NVGPU_RM_IDLE_CHANNELS_FLAGS {w['offset']}",
        f"#define NVGPU_RM_IDLE_CHANNELS_LIST_LO {w['lo']}",
        f"#define NVGPU_RM_IDLE_CHANNELS_LIST_HI {w['hi']}",
        f"#define NVGPU_RM_IDLE_CHANNELS_LIST {w['value']}",
        "",
        "static const struct nvgpu_rm_deep_control nvgpu_rm_deep_idle_channels = {",
        "    0,",
        f"    {len(idle['pointers'])},",
        "    {",
    ]
    for p in idle["pointers"]:
        s = p["size"]
        c = s["count"][0]
        out.append(f"        {{{p['offset']}, NVGPU_RM_DEEP_IN, 1, "
                   f"{{{{{c['offset']}, {c['width']}}}}}, "
                   f"{s['scale']}, {s['elem']}}}, /* {p['field']} */")
    out += [
        "    }};",
        "",
        "static inline const struct nvgpu_rm_deep_control *nvgpu_rm_deep_find(u32 cmd) {",
        "  unsigned int i;",
        "",
        "  for (i = 0; i < ARRAY_SIZE(nvgpu_rm_deep_table); i++)",
        "    if (nvgpu_rm_deep_table[i].cmd == cmd)",
        "      return &nvgpu_rm_deep_table[i];",
        "  return NULL;",
        "}",
        "",
        "#endif /* NVGPU_RM_DEEP_H */",
        "",
    ]
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
            data = load_all(OUT_DIR)
            RUST_OUT.write_text(render(data))
            C_OUT.write_text(render_c(data))
        elif a.cmd == "render":
            data = load_all(OUT_DIR)
            text, c_text = render(data), render_c(data)
            if a.out:
                a.out.mkdir(parents=True, exist_ok=True)
                (a.out / "generated.rs").write_text(text)
                (a.out / C_OUT.name).write_text(c_text)
            else:
                RUST_OUT.write_text(text)
                C_OUT.write_text(c_text)
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
            data = load_all(OUT_DIR)
            if RUST_OUT.read_text() != render(data):
                stale.append(str(RUST_OUT))
            if not C_OUT.exists() or C_OUT.read_text() != render_c(data):
                stale.append(str(C_OUT))
            if stale:
                print("stale: " + ", ".join(stale) + f"; run {sys.argv[0]} all", file=sys.stderr)
                return 1
            print("gen/rmctrl, gen/src/rmctrl/generated.rs and driver/gen/nvgpu_rm_deep.h "
                  "are up to date")
    except ExtractError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
