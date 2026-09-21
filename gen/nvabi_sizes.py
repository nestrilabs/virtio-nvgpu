#!/usr/bin/env python3
"""Compute NVIDIA driver struct sizes from gVisor's pkg/abi/nvgpu.

gVisor is a Bazel project: `go build` on it fails without generated code, so
this reads the Go struct declarations directly and applies C layout rules.
That keeps the generator runnable by anyone with Python and a gVisor checkout,
which is the requirement in gen/README.md.

The structs are `structs.HostLayout`, i.e. deliberately laid out like the C
structs they mirror, so natural alignment reproduces the driver's ABI.

    ./nvabi_sizes.py --gvisor ~/forks/gvisor NVOS46_PARAMETERS_V580
    ./nvabi_sizes.py --gvisor ~/forks/gvisor --json NVOS54_PARAMETERS ...
"""

import argparse
import json
import re
import sys
from pathlib import Path

# (size, alignment) for types that are not declared as structs.
PRIMITIVES = {
    "uint8": (1, 1), "int8": (1, 1), "byte": (1, 1), "bool": (1, 1),
    "uint16": (2, 2), "int16": (2, 2),
    "uint32": (4, 4), "int32": (4, 4),
    "uint64": (8, 8), "int64": (8, 8),
    "uintptr": (8, 8),
    # nvgpu aliases
    "Handle": (4, 4), "ClassID": (4, 4), "P64": (8, 8),
    "NvUUID": (16, 1),
    # zero-sized layout marker
    "structs.HostLayout": (0, 1),
}

ARRAY_RE = re.compile(r"^\[([A-Za-z0-9_]+)\](.+)$")
FIELD_RE = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s+([][A-Za-z0-9_.*]+)\s*$")
EMBED_RE = re.compile(r"^\s*(_|structs\.HostLayout)\s+(structs\.HostLayout)?\s*$")
CONST_RE = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(0[xX][0-9a-fA-F]+|\d+)\s*$")


def load(gvisor: Path):
    """Return (structs, consts) parsed from pkg/abi/nvgpu/*.go."""
    src = gvisor / "pkg" / "abi" / "nvgpu"
    if not src.is_dir():
        sys.exit(f"not a gVisor checkout: {src} missing")

    structs, consts = {}, {}
    for path in sorted(src.glob("*.go")):
        if path.name.endswith("_test.go"):
            continue
        text = path.read_text()

        for line in text.splitlines():
            m = CONST_RE.match(line.replace("const ", ""))
            if m and m.group(1).isupper():
                consts[m.group(1)] = int(m.group(2), 0)

        for m in re.finditer(r"^type\s+(\w+)\s+struct\s*\{(.*?)^\}", text, re.S | re.M):
            name, body = m.group(1), m.group(2)
            fields = []
            for line in body.splitlines():
                line = line.split("//")[0].rstrip()
                if not line.strip() or EMBED_RE.match(line):
                    continue
                fm = FIELD_RE.match(line)
                if fm:
                    fields.append((fm.group(1), fm.group(2)))
            structs[name] = fields

        # type X uint32  -> primitive alias
        for m in re.finditer(r"^type\s+(\w+)\s+(uint\d+|int\d+|uintptr)\s*$", text, re.M):
            PRIMITIVES.setdefault(m.group(1), PRIMITIVES[m.group(2)])
        # type X [N]byte
        for m in re.finditer(r"^type\s+(\w+)\s+\[(\w+)\](\w+)\s*$", text, re.M):
            consts.setdefault("__alias_" + m.group(1), 0)
            structs.setdefault(m.group(1), [("Elems", f"[{m.group(2)}]{m.group(3)}")])

    return structs, consts


def layout(name, structs, consts, stack=()):
    """Return (size, alignment) for a type name, applying C layout rules."""
    if name in PRIMITIVES:
        return PRIMITIVES[name]

    am = ARRAY_RE.match(name)
    if am:
        count_tok, elem = am.group(1), am.group(2)
        count = int(count_tok) if count_tok.isdigit() else consts.get(count_tok)
        if count is None:
            raise KeyError(f"unknown array length {count_tok!r} in {name!r}")
        esz, eal = layout(elem, structs, consts, stack)
        return esz * count, eal

    if name.startswith("*"):
        return 8, 8

    if name not in structs:
        raise KeyError(f"unknown type {name!r}")
    if name in stack:
        raise ValueError(f"recursive type {name!r}")

    off, align = 0, 1
    for _fname, ftype in structs[name]:
        fsz, fal = layout(ftype, structs, consts, stack + (name,))
        if fal > 1 and off % fal:
            off += fal - (off % fal)          # pad to field alignment
        off += fsz
        align = max(align, fal)
    if align > 1 and off % align:
        off += align - (off % align)          # tail padding
    return off, align


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gvisor", required=True, type=Path)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("names", nargs="+")
    args = ap.parse_args()

    structs, consts = load(args.gvisor)
    out = {}
    for n in args.names:
        try:
            out[n] = layout(n, structs, consts)[0]
        except (KeyError, ValueError) as e:
            out[n] = None
            print(f"warning: {n}: {e}", file=sys.stderr)

    if args.json:
        print(json.dumps(out, indent=2))
    else:
        for n, sz in out.items():
            print(f"{sz if sz is not None else '?':>6}  {n}")


if __name__ == "__main__":
    main()
