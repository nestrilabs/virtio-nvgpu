#!/usr/bin/env bash
# Fail if `unsafe`, or anything that makes a raw address, appears outside the
# modules allowed to hold it -- the `sys` module of each crate:
#
#   device/src/sys/        (the backend: parameter blocks, mappings, ioctls)
#   wlwire/src/sys.rs      (the Wayland proxy's system calls)
#   nvgpu-wl-guest/src/sys.rs  (the guest daemon's)
#
# and fail if a module outside them has lost the attribute that makes the
# compiler say the same: every crate root `#![deny(unsafe_code)]` (with only
# its `sys` allowed), every other source file `#![forbid(unsafe_code)]`.
#
# What counts, in code (comments and string literals are skipped):
#   - the `unsafe` keyword, in any position (blocks, fns, impls, traits,
#     `#[unsafe(..)]` attributes);
#   - a raw address made or taken: `as_ptr()`, `as_mut_ptr()`, a cast to or
#     a type of `*const`/`*mut`, `addr_of!`, `expose_provenance`,
#     `from_raw_parts`, `transmute`;
#   - a descriptor claimed by number: `from_raw_fd`, `borrow_raw`;
#   - `allow(unsafe_code)` anywhere but on a crate root's `sys` declaration.
#
# Why the second and third: the host is handed an address only through an
# arena block (device/src/sys/block.rs). Outside `sys` no code can make an
# address to put in one, so none can reach the host but the arena's own.
#
# Usage: scripts/check-unsafe.sh   (exit 0 clean, 1 with a list of findings)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

exec python3 - "$@" <<'PY'
import os
import re
import sys

CRATES = ["device", "wlwire", "nvgpu-wl-guest", "protocol", "gen", "fuzz"]
ALLOWED = ("device/src/sys/", "wlwire/src/sys.rs", "nvgpu-wl-guest/src/sys.rs")
# Crate roots: `#![deny(unsafe_code)]`, and `#[allow(unsafe_code)]` only on
# `mod sys`.
ROOTS = {
    "device/src/lib.rs",
    "wlwire/src/lib.rs",
    "nvgpu-wl-guest/src/lib.rs",
}
# Files that need no attribute: build scripts, generated or data-only crates
# that have no `unsafe` to forbid (still scanned).
NO_ATTR = re.compile(r"(^|/)build\.rs$|^protocol/|^gen/|^fuzz/")


def strip(src):
    """`src` with comments, string, char and byte literals blanked out
    (newlines kept, so line numbers hold)."""
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(re.sub(r"[^\n]", " ", src[i:j]))
            i = j
        elif re.match(r'(b|c)?r(#*)"', src[i:]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            m = re.match(r'(b|c)?r(#*)"', src[i:])
            end = '"' + m.group(2)
            j = src.find(end, i + m.end())
            j = n if j < 0 else j + len(end)
            out.append(re.sub(r"[^\n]", " ", src[i:j]))
            i = j
        elif c == '"' or (c in "bc" and src.startswith('"', i + 1) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))):
            j = i + (2 if c != '"' else 1)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(re.sub(r"[^\n]", " ", src[i:j]))
            i = j
        elif c == "'":
            m = re.match(r"'(\\(x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'", src[i:])
            if m:
                out.append(" " * m.end())
                i += m.end()
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


PATTERNS = [
    (re.compile(r"\bunsafe\b"), "unsafe"),
    (re.compile(r"\.as_(mut_)?ptr\s*\(\s*\)"), "a raw address (as_ptr/as_mut_ptr)"),
    (re.compile(r"\*\s*(const|mut)\s+[A-Za-z_\[(]"), "a raw pointer type or cast"),
    (re.compile(r"\baddr_of(_mut)?!"), "a raw address (addr_of!)"),
    (re.compile(r"\b(with_)?expose(d)?_provenance\b"), "a raw address (provenance)"),
    (re.compile(r"\bfrom_raw_parts(_mut)?\b"), "a slice from a raw address"),
    (re.compile(r"\btransmute\b"), "transmute"),
    (re.compile(r"\bfrom_raw_fd\b"), "a descriptor claimed by number (from_raw_fd)"),
    (re.compile(r"\bborrow_raw\b"), "a descriptor borrowed by number (borrow_raw)"),
    (re.compile(r"allow\s*\(\s*unsafe_code\s*\)"), "allow(unsafe_code)"),
]

findings = []
checked = 0


def safety_comments(path):
    """Every `unsafe` in an allowed module must say why it is sound: a
    `SAFETY:` comment (or a `# Safety` section, for an unsafe trait or fn)
    in the comment block just above the statement it is in."""
    src = open(path, encoding="utf-8").read()
    raw = src.split("\n")
    code = strip(src).split("\n")
    for i, line in enumerate(code):
        if not re.search(r"\bunsafe\b", line):
            continue
        j, ok = i, False
        while j >= 0 and i - j <= 30:
            text = raw[j]
            if "SAFETY:" in text or "# Safety" in text:
                ok = True
                break
            # Past the start of the statement and the comments above it.
            prev = code[j - 1].strip() if j > 0 else ""
            if j < i and code[j].strip() and prev.endswith((";", "}")) and not raw[j - 1].strip().startswith("//"):
                break
            j -= 1
        if not ok:
            findings.append(f"{path}:{i + 1}: unsafe without a SAFETY comment: {raw[i].strip()}")


for allowed in ALLOWED:
    paths = (
        [os.path.join(allowed, f) for f in sorted(os.listdir(allowed)) if f.endswith(".rs")]
        if allowed.endswith("/")
        else [allowed]
    )
    for path in paths:
        safety_comments(path)

for crate in CRATES:
    for dirpath, dirs, files in os.walk(crate):
        dirs[:] = [d for d in dirs if d not in ("target", "corpus", "artifacts")]
        for f in files:
            if not f.endswith(".rs"):
                continue
            path = os.path.join(dirpath, f)
            if path.startswith(ALLOWED):
                continue
            checked += 1
            src = open(path, encoding="utf-8").read()
            code = strip(src)
            lines = code.split("\n")
            for no, line in enumerate(lines, 1):
                for pat, what in PATTERNS:
                    if not pat.search(line):
                        continue
                    if what == "allow(unsafe_code)" and path in ROOTS:
                        # Only on the line before `pub mod sys;`.
                        nxt = lines[no] if no < len(lines) else ""
                        if re.match(r"\s*(pub\s+)?mod\s+sys\s*;", nxt):
                            continue
                    findings.append(f"{path}:{no}: {what}: {src.split(chr(10))[no - 1].strip()}")
            if path in ROOTS:
                if not re.search(r"^#!\[deny\(unsafe_code\)\]", code, re.M):
                    findings.append(f"{path}: crate root without #![deny(unsafe_code)]")
                if not re.search(r"^#!\[deny\(unsafe_op_in_unsafe_fn\)\]", code, re.M):
                    findings.append(f"{path}: crate root without #![deny(unsafe_op_in_unsafe_fn)]")
            elif not NO_ATTR.search(path) and not re.search(
                r"^#!\[forbid\(unsafe_code\)\]", code, re.M
            ):
                findings.append(f"{path}: no #![forbid(unsafe_code)]")

if findings:
    print("check-unsafe: findings:", file=sys.stderr)
    for f in findings:
        print("  " + f, file=sys.stderr)
    sys.exit(1)
print(
    f"check-unsafe: {checked} files outside the sys modules, none with unsafe code; "
    "every unsafe inside them with its SAFETY comment"
)
PY
