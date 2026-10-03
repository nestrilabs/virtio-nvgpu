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
RS_SERVER = "src/nvidia/src/libraries/resserv/src/rs_server.c"
DEPREC = "src/nvidia/interface/deprecated"
ENTRY_POINTS = "src/nvidia/src/kernel/rmapi/entry_points.c"
PARAM_COPY = "src/nvidia/inc/kernel/rmapi/param_copy.h"

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


def resolve_type(name, types, defs, aliases):
    """The header declaring a type, chasing `#define OLD NEW` aliases.

    54 parameter structs in 615.71.09 are spelled as an alias of another --
    `NV0080_CTRL_BSP_GET_CAPS_PARAMS` is a macro for the NVDEC one, not a
    typedef -- so a name missing from the type index is not a name missing
    from the headers.
    """
    hdrs, seen = [], set()
    while name and name not in seen:
        seen.add(name)
        if name in types:
            hdrs.append(types[name])
            return hdrs
        if name in defs:
            hdrs.append(defs[name])
            name = aliases.get(name)
            continue
        return None
    return None


def alias_index(ogkm):
    """One-line `#define NAME OTHER` aliases, NAME -> OTHER."""
    out = {}
    root = os.path.join(ogkm, INC)
    pat = re.compile(r"^\s*#\s*define\s+(\w+)\s+(\w+)\s*$")
    for dirpath, _, names in os.walk(root):
        for n in names:
            if not n.endswith(".h"):
                continue
            for ln in open(os.path.join(dirpath, n), errors="replace"):
                m = pat.match(ln)
                if m:
                    out.setdefault(m.group(1), m.group(2))
    return out


RS_ENTRY_RE = re.compile(
    r"RS_ENTRY\(\s*/\*\s*External Class\s*\*/\s*(?P<name>\w+)\s*,\s*"
    r"/\*\s*Internal Class\s*\*/\s*(?P<internal>\w+)\s*,.*?"
    r"/\*\s*Alloc Param Info\s*\*/\s*"
    r"(?:RS_NONE|RS_OPTIONAL\((?P<opt>\w+)\)|RS_REQUIRED\((?P<req>\w+)\))\s*,",
    re.S,
)


# `NvBool bClientAlloc = (pParams->externalClassId == NV01_ROOT || ...)` in
# serverAllocResource. The classes it names are allocated by serverAllocClient,
# which never reaches _serverAllocValidatePrivilege, so they carry no privilege
# flag in RS_ENTRY and the rule below must not ask them for one.
CLIENT_ALLOC = re.compile(
    r"bClientAlloc\s*=\s*\((?P<body>[^;]*?)\)\s*;", re.S
)


def client_classes(ogkm):
    """The classes RM allocates as clients rather than as resources.

    Read out of rs_server.c rather than listed here. Every process that opens
    the device allocates one of these first, so getting this wrong refuses
    everything -- which is exactly what it did the first time, when the rule
    asked a client object for a privilege flag that RM never gives it.
    """
    text = open(os.path.join(ogkm, RS_SERVER), errors="replace").read()
    m = CLIENT_ALLOC.search(text)
    if not m:
        sys.exit(f"no bClientAlloc in {RS_SERVER}; this release decides client allocation differently")
    names = set(re.findall(r"externalClassId\s*==\s*(\w+)", m.group("body")))
    if not names:
        sys.exit(f"bClientAlloc in {RS_SERVER} names no classes")
    return names


# Three more ways into RM, none of which carries a control flag.
#
# `_nv04ControlWithSecInfo` in entry_points.c asks RmDeprecatedGetControlHandler
# first and only falls through to the ordinary resource-server dispatch if it
# comes back NULL. So before the NVOC tables are consulted at all:
#
#   1. rmDeprecatedControlTable -- eight commands, each rewritten by a
#      V2 converter into a modern command and reissued with the caller's own
#      security info. The modern command's flags are the privilege rule; the
#      legacy command is a spelling of it with an older parameter struct.
#   2. IsGssLegacyCall(cmd), i.e. cmd & RM_GSS_LEGACY_MASK -- forwarded whole
#      to GSP firmware, which RM no longer tries to understand. RM's privilege
#      rule for these is in the command number: bit 0x4000 means root.
#
# And once dispatch does reach the resource server, a class whose Control is a
# catch-all forwarder (BinaryApi) serves any command sent to it without an
# exported-method entry. There the privilege is the *class's*, which is why RM
# has two of them -- NV2081_BINAPI non-privileged, NV2082_BINAPI_PRIVILEGED not.

DEPREC_ROW = re.compile(
    r"\{\s*(?P<cmd>NV\w*_CTRL_CMD_\w+)\s*,\s*V2_CONVERTER\(\s*(?P<conv>_\w+)\s*\)\s*"
    r"(?:,\s*(?P<skip_vgpu>\w+)\s*)?\}"
)
DEPREC_BODY = re.compile(
    r"static\s+NV_STATUS\s+V2_CONVERTER\(\s*(?P<conv>_\w+)\s*\)\s*\n\(" r"(?P<body>.*?)\n\}",
    re.S,
)
# A params struct this release names for the older form of the call: a
# `*_PARAMS` type that is not the V2 one the converter builds.
DEPREC_OLD_PARAMS = re.compile(r"\b(NV\w*_CTRL_\w*_PARAMS)\b")
DEPREC_NEW_CMD = re.compile(r"\b(NV\w*_CTRL_CMD_\w+_V2)\b")


def deprecated_controls(ogkm):
    """The legacy commands RM rewrites, each with the command it rewrites to.

    A converter reissues the call with the caller's own security info, so the
    modern command's flags decide whether the legacy spelling is allowed. The
    size, though, is the *old* struct's: that is what the guest sends.
    """
    path = os.path.join(ogkm, DEPREC, "rmapi_deprecated_control.c")
    text = open(path, errors="replace").read()
    if "rmDeprecatedControlTable" not in text:
        sys.exit(f"no rmDeprecatedControlTable in {path}; this release routes legacy controls differently")
    bodies = {m.group("conv"): m.group("body") for m in DEPREC_BODY.finditer(text)}
    out = {}
    for m in DEPREC_ROW.finditer(text):
        conv, cmd = m.group("conv"), m.group("cmd")
        skip = m.group("skip_vgpu")
        if skip is not None and skip != "NV_FALSE":
            sys.exit(
                f"{path}: {cmd} is converted conditionally (bSkipVGPU {skip}); "
                "which rule applies depends on the GPU and this table cannot say"
            )
        body = bodies.get(conv)
        if body is None:
            sys.exit(f"{path}: no body for the converter {conv} that handles {cmd}")
        target = DEPREC_NEW_CMD.search(body)
        if not target:
            sys.exit(f"{path}: the converter for {cmd} names no V2 command; read it before trusting this table")
        old = [t for t in DEPREC_OLD_PARAMS.findall(body) if "_V2" not in t]
        if not old:
            sys.exit(f"{path}: the converter for {cmd} names no legacy parameter struct")
        out[cmd] = (target.group(1), old[0])
    if not out:
        sys.exit(f"{path}: rmDeprecatedControlTable parsed to nothing")
    return out


def gss_rule(ogkm):
    """RM's privilege rule for the commands it forwards to GSP firmware.

    These carry no flags anywhere: RM stopped implementing them and passes them
    through. The rule is in the command number, and RmGssLegacyRpcCmd is the
    only place that says so, so the shape of its check is asserted here rather
    than remembered.
    """
    hdr = open(os.path.join(ogkm, DEPREC, "rmapi_deprecated.h"), errors="replace").read()
    masks = {}
    for name in ("RM_GSS_LEGACY_MASK", "RM_GSS_LEGACY_MASK_PRIVILEGED"):
        m = re.search(rf"#define\s+{name}\s+(0x[0-9a-fA-F]+)", hdr)
        if not m:
            sys.exit(f"{name} is not in rmapi_deprecated.h; this release marks GSS-legacy commands differently")
        masks[name] = int(m.group(1), 16)
    if not masks["RM_GSS_LEGACY_MASK"]:
        sys.exit("RM_GSS_LEGACY_MASK is 0; every command would look GSS-legacy")
    if masks["RM_GSS_LEGACY_MASK_PRIVILEGED"] & masks["RM_GSS_LEGACY_MASK"] != masks["RM_GSS_LEGACY_MASK"]:
        sys.exit("RM_GSS_LEGACY_MASK_PRIVILEGED no longer contains RM_GSS_LEGACY_MASK; re-read the rule")

    # `IsGssLegacyCall` is the gate, in the other file.
    gate = open(os.path.join(ogkm, DEPREC, "rmapi_deprecated_control.c"), errors="replace").read()
    if not re.search(r"IsGssLegacyCall.*?return\s*!!\(\s*cmd\s*&\s*RM_GSS_LEGACY_MASK\s*\)", gate, re.S):
        sys.exit("IsGssLegacyCall is no longer a test of RM_GSS_LEGACY_MASK; read it again")

    rpc = open(os.path.join(ogkm, DEPREC, "rmapi_gss_legacy_control.c"), errors="replace").read()
    if not re.search(
        r"\(\s*pArgs->cmd\s*&\s*RM_GSS_LEGACY_MASK_PRIVILEGED\s*\)\s*==\s*RM_GSS_LEGACY_MASK_PRIVILEGED\s*\)\s*&&\s*"
        r"\(\s*pSecInfo->privLevel\s*<\s*RS_PRIV_LEVEL_USER_ROOT\s*\)",
        rpc,
    ):
        sys.exit(
            "RmGssLegacyRpcCmd no longer refuses the privileged mask to a non-root caller; "
            "the backend's rule for GSP-forwarded commands is a copy of that check and has to be re-read"
        )

    # RM does not size these -- it forwards whatever it is given, up to a cap
    # that is itself different for the privileged half.
    cap = re.search(r"#define\s+RMAPI_PARAM_COPY_MAX_PARAMS_SIZE\s+\(([^)]*)\)", open(os.path.join(ogkm, PARAM_COPY), errors="replace").read())
    if not cap:
        sys.exit("RMAPI_PARAM_COPY_MAX_PARAMS_SIZE is not in param_copy.h; GSP-forwarded commands would be uncapped")
    max_params = eval(cap.group(1), {"__builtins__": {}}, {})  # e.g. (2*1024*1024)
    if not isinstance(max_params, int) or max_params <= 0:
        sys.exit(f"RMAPI_PARAM_COPY_MAX_PARAMS_SIZE did not read as a size: {cap.group(1)!r}")
    # 535.129.03 applies no ceiling here at all: it allocates whatever size the
    # caller names and copies into it. The backend applies RM's own constant
    # anyway, which is stricter than that release and no looser than any other
    # -- an uncapped size on this path is a host allocation a guest chooses.
    capped_by_rm = bool(re.search(r"RMAPI_PARAM_COPY_MAX_PARAMS_SIZE\b(?!_PRIVILEGED)", rpc))
    return dict(mask=masks["RM_GSS_LEGACY_MASK"],
                capped_by_rm=capped_by_rm,
                privileged=masks["RM_GSS_LEGACY_MASK_PRIVILEGED"],
                max_params=max_params)


def check_dispatch_order(ogkm):
    """The deprecated and GSS paths are tried before the resource server.

    If that ever stops being true the backend's ordering is wrong, and wrong in
    the admitting direction: it would hand a command to the GSS rule that RM
    had already sized and flagged.
    """
    text = open(os.path.join(ogkm, ENTRY_POINTS), errors="replace").read()
    m = re.search(r"_nv04ControlWithSecInfo\s*\([^;{]*?\)\s*\n\{(.*?)\n\}", text, re.S)
    if not m:
        sys.exit(f"no _nv04ControlWithSecInfo in {ENTRY_POINTS}; the control entry point moved")
    body = m.group(1)
    dep = body.find("RmDeprecatedGetControlHandler")
    norm = body.find("ControlWithSecInfo(pRmApi")
    if dep < 0 or norm < 0 or dep > norm:
        sys.exit(
            f"{ENTRY_POINTS}: the deprecated handler is no longer consulted before the resource server; "
            "the backend applies the two rules in RM's order and that order just changed"
        )


CATCH_ALL = ("BinaryApi",)


def catch_all_classes(rows, allowed):
    """Classes whose Control forwards any command, and the ranges that implies.

    binapiControl is a passthrough: it hands the command and the parameter
    block to GSP without an exported-method entry, so no control flag exists
    for anything sent to one. What exists instead is the class split --
    NV2081_BINAPI is RS_FLAGS_ALLOC_NON_PRIVILEGED, NV2082_BINAPI_PRIVILEGED is
    RS_FLAGS_ALLOC_PRIVILEGED -- and the class allowlist already decides it.
    A command is numbered with its class in the high half, so an allowed
    catch-all class allows that half.
    """
    ok = {c["class_"] for c in allowed}
    out = []
    for r in rows:
        if r["internal"] not in CATCH_ALL:
            continue
        cls = r.get("class_")
        if cls is None or cls not in ok:
            continue
        out.append(dict(name=r["name"], class_=cls))
    out.sort(key=lambda c: c["class_"])
    return out


def resource_entries(ogkm):
    """Each RS_ENTRY in order: its external class name and its alloc param type."""
    text = open(os.path.join(ogkm, RMAPI, "resource_list.h"), errors="replace").read()
    out = []
    for m in RS_ENTRY_RE.finditer(text):
        out.append(dict(name=m.group("name"), internal=m.group("internal"),
                        param=m.group("opt") or m.group("req")))
    return out


def ctrl_probe(allow, deprecated, idx, defs, aliases):
    """A C program printing the numbers the headers decide.

    `K <cmd> <params size>` for each allowed control, and `D <legacy cmd>
    <modern cmd> <legacy params size>` for each deprecated one -- the command
    numbers are macros, so the compiler resolves them rather than this script.
    """
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
    dep, dep_unresolved = [], {}
    for legacy, (target, old_params) in sorted(deprecated.items()):
        want = {legacy: defs.get(legacy) and [defs[legacy]],
                target: defs.get(target) and [defs[target]],
                old_params: resolve_type(old_params, idx, defs, aliases)}
        missing = sorted(n for n, h in want.items() if not h)
        if missing:
            # Left out, which refuses the legacy spelling -- and named, because
            # a deprecated control RM still serves is one some userspace still
            # calls.
            dep_unresolved[legacy] = ", ".join(missing)
            continue
        for h in want.values():
            hdrs.update(h)
        dep.append((legacy, target, old_params))
    out = ["#include <stddef.h>", "#include <stdio.h>", '#include "nvtypes.h"', '#include "nvos.h"']
    out += [f'#include "{h}"' for h in sorted(hdrs)]
    out.append("int main(void) {")
    for cmd, t in rows:
        size = "0" if t is None else f"sizeof({t})"
        out.append(f'  printf("K {cmd:#x} %zu\\n", (size_t)({size}));')
    for legacy, target, old_params in dep:
        out.append(f'  printf("D %#x %#x %zu\\n", (unsigned)({legacy}), (unsigned)({target}), sizeof({old_params}));')
    out.append("  return 0;\n}")
    return "\n".join(out), unresolved, dep_unresolved


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


def allowed_classes(rows, drop, clients):
    """RM's own rule, applied to the probe's numbers.

    _serverAllocValidatePrivilege in alloc_free.c, which rejects a class with
    no privilege flag outright ("missing its privilege flag in RS_ENTRY"),
    requires root for ALLOC_PRIVILEGED and kernel for ALLOC_KERNEL_PRIVILEGED.
    The client classes never reach it; see client_classes().
    """
    out, refused = [], {}
    for r in rows:
        name = r["name"]
        f = r["flags"]
        if name in clients:
            # Allocated by serverAllocClient. Every workload allocates one
            # before it can do anything at all.
            out.append(dict(class_=r["class_"], size=r["size"], required=r["required"], name=name))
            continue
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


def emit(rows, classes, deprec, catch_all, gss, version, stream):
    ident = "v" + version.replace(".", "_")
    print("// Generated by gen/rmallow_extract.py -- do not edit by hand.", file=stream)
    print("//", file=stream)
    print(f"//   ./rmallow_extract.py --ogkm <open-gpu-kernel-modules at {version}> \\", file=stream)
    print(f"//       --version {version} > src/rmallow/{ident}.rs", file=stream)
    print("//", file=stream)
    print(f"// The RM controls and classes driver {version} exports to an unprivileged", file=stream)
    print("// caller. A command or class that is not here is refused under every cap.", file=stream)
    print(file=stream)
    print("use super::{AllowClass, AllowCtrl, AllowRange, DeprecCtrl, GssRule};", file=stream)
    print(file=stream)
    # One row per line. These are lists a person has to be able to read down,
    # so they are built through a const constructor rather than as struct
    # literals, which rustfmt breaks across four lines each.
    print("pub static CTRL: &[AllowCtrl] = &[", file=stream)
    for cmd, size in rows:
        print(f"    AllowCtrl::new({cmd:#010x}, {size}),", file=stream)
    print("];", file=stream)
    print(file=stream)
    print("/// Commands RM rewrites into a modern one before anything else looks at", file=stream)
    print("/// them. The modern command's flags decide; the size is the old struct's.", file=stream)
    print("/// A hit here is final either way, because it is final in RM.", file=stream)
    print("pub static DEPREC: &[DeprecCtrl] = &[", file=stream)
    for d in deprec:
        print(
            f"    DeprecCtrl::new({d['cmd']:#010x}, {d['size']}, {str(d['allowed']).lower()}), "
            f"// -> {d['target']:#010x}",
            file=stream,
        )
    print("];", file=stream)
    print(file=stream)
    print("/// RM stopped implementing these and forwards them to GSP firmware. It", file=stream)
    print("/// reads no parameter struct, so there is no size to check, only a cap.", file=stream)
    print(f"pub static GSS: GssRule = GssRule::new({gss['mask']:#010x}, {gss['privileged']:#010x}, {gss['max_params']});", file=stream)
    print(file=stream)
    print("/// Classes whose control entry point forwards whatever it is handed. The", file=stream)
    print("/// privilege is the class's, and the class table above already applied it.", file=stream)
    print("pub static CATCH_ALL: &[AllowRange] = &[", file=stream)
    for c in catch_all:
        print(f"    AllowRange::new({c['class_']:#06x}), // {c['name']}", file=stream)
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
    check_dispatch_order(a.ogkm)
    gss = gss_rule(a.ogkm)
    deprecated = deprecated_controls(a.ogkm)
    types = type_index(a.ogkm)
    defs = define_index(a.ogkm)
    aliases = alias_index(a.ogkm)
    src, unresolved, dep_unresolved = ctrl_probe(allow, deprecated, types, defs, aliases)
    csrc, rs_entries, drop = class_probe(a.ogkm, defs, types)
    clients = client_classes(a.ogkm)
    missing = clients - {e["name"] for e in rs_entries}
    if missing:
        sys.exit("bClientAlloc names classes resource_list.h does not: " + ", ".join(sorted(missing)))

    incs = ["-I", os.path.join(a.ogkm, INC), "-I", os.path.join(a.ogkm, "src/common/inc"),
            "-I", os.path.join(a.ogkm, RMAPI), "-I", os.path.join(a.ogkm, "src/common/sdk/nvidia/inc")]
    with tempfile.TemporaryDirectory() as d:
        rows, deprec_raw = [], []
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
            elif f and f[0] == "D":
                deprec_raw.append((int(f[1], 16), int(f[2], 16), int(f[3])))

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
    classes, class_refused = allowed_classes(raw, drop, clients)

    # A legacy command is allowed exactly when the command it is rewritten into
    # is. A target in no exported-method table at all is not a reading of
    # anything, so it refuses -- and says so.
    by_name = {e["name"]: e for e in rs_entries}
    for r in raw:
        r["internal"] = by_name.get(r["name"], {}).get("internal")
    catch_all = catch_all_classes(raw, classes)
    deprec, deprec_note = [], []
    for cmd, target, size in sorted(deprec_raw):
        if target in allow:
            ok, why = True, None
        elif target in refused:
            ok, why = False, refused[target]
        else:
            ok, why = False, "the command it is rewritten into is in no exported-method table"
        deprec.append(dict(cmd=cmd, target=target, size=size, allowed=ok))
        if not ok:
            deprec_note.append((cmd, target, why))

    # A command with the GSS-legacy bit set never reaches the exported-method
    # tables: RM takes the GSP branch first. Its flags, if it has any, are dead.
    shadowed = sorted(c for c in set(allow) | set(refused) if c & gss["mask"])

    buf = io.StringIO()
    emit(rows, classes, deprec, catch_all, gss, a.version, buf)
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
    if dep_unresolved:
        sys.stderr.write("deprecated, names this script could not resolve in the SDK headers (left out):\n")
        for name, missing in sorted(dep_unresolved.items()):
            sys.stderr.write(f"  {name}: {missing}\n")
    if deprec_note:
        sys.stderr.write("deprecated commands refused (RM rewrites them into something privileged):\n")
        for cmd, target, why in deprec_note:
            sys.stderr.write(f"  {cmd:#010x} -> {target:#010x}: {why}\n")
    sys.stderr.write(
        f"{len(deprec)} deprecated commands: {sum(1 for d in deprec if d['allowed'])} allowed; "
        f"GSP-forwarded commands keyed by {gss['mask']:#x}, root above {gss['privileged']:#x}, "
        f"capped at {gss['max_params']} bytes; {len(catch_all)} catch-all classes\n"
    )
    if not gss["capped_by_rm"]:
        sys.stderr.write(
            "RmGssLegacyRpcCmd in this release applies no ceiling of its own; the backend's cap is "
            "stricter than the driver it is talking to\n"
        )
    if shadowed:
        sys.stderr.write(
            f"{len(shadowed)} exported commands carry the GSS-legacy bit and never reach their own "
            "table; RM forwards them to GSP and so does the backend:\n"
        )
        for c in shadowed:
            sys.stderr.write(f"  {c:#010x}\n")
    sys.stderr.write(
        f"{len(rs_entries)} classes: {len(classes)} allowed ({len(clients)} of them clients), "
        f"{len(class_refused)} refused, {len(skipped)} not compiled\n"
    )


if __name__ == "__main__":
    main()
