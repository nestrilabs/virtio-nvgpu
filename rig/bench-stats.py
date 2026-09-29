#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The table of BENCHMARKS.md from rig/rig-bench.sh's runs.

Usage: rig/bench-stats.py LABEL=GLOB [LABEL=GLOB ...] [--ref LABEL] [--only REGEX]

Each LABEL is a column, made from the .bench files GLOB matches (one per
run): the mean of the runs, and +- half their range. The first label (or
--ref) is the reference; every other column also gets its ratio to it,
oriented so that above 1 is slower (for a time) or less (for a rate).
Rows are every figure any column has, in the order the first file lists
them; --only keeps those matching REGEX. Markdown on stdout.
"""
import glob
import re
import sys
from collections import OrderedDict

SLOWER_IS_BIGGER = {"ms", "us", "s", "%"}


def load(pattern):
    runs = []
    for path in sorted(glob.glob(pattern)):
        figs = OrderedDict()
        with open(path, errors="replace") as f:
            for line in f:
                p = line.split()
                if len(p) >= 4 and p[0] == "BENCH":
                    try:
                        figs[p[1]] = (float(p[2]), p[3])
                    except ValueError:
                        pass
        runs.append(figs)
    return runs


def fmt(v):
    if v == 0:
        return "0"
    a = abs(v)
    if a >= 1000:
        return f"{v:,.0f}"
    if a >= 100:
        return f"{v:.0f}"
    if a >= 10:
        return f"{v:.1f}"
    if a >= 1:
        return f"{v:.2f}"
    return f"{v:.3g}"


def main(argv):
    cols, ref, only = [], None, None
    it = iter(argv)
    for a in it:
        if a == "--ref":
            ref = next(it)
        elif a == "--only":
            only = re.compile(next(it))
        elif "=" in a:
            label, pat = a.split("=", 1)
            cols.append((label, load(pat)))
        else:
            sys.exit(__doc__)
    if not cols:
        sys.exit(__doc__)
    ref = ref or cols[0][0]
    names = OrderedDict()
    for _, runs in cols:
        for r in runs:
            for k, (_, unit) in r.items():
                names.setdefault(k, unit)
    head = ["figure", "unit"]
    for label, runs in cols:
        head.append(f"{label} (n={len(runs)})")
        if label != ref:
            head.append(f"{label}/{ref}")
    print("| " + " | ".join(head) + " |")
    print("|" + "---|" * len(head))
    refstats = {}
    for label, runs in cols:
        if label == ref:
            for k in names:
                vs = [r[k][0] for r in runs if k in r]
                if vs:
                    refstats[k] = sum(vs) / len(vs)
    for k, unit in names.items():
        if only and not only.search(k):
            continue
        row = [k, unit]
        for label, runs in cols:
            vs = [r[k][0] for r in runs if k in r]
            if not vs:
                row.append("--")
                if label != ref:
                    row.append("")
                continue
            m = sum(vs) / len(vs)
            half = (max(vs) - min(vs)) / 2
            row.append(f"{fmt(m)} ± {fmt(half)}" if len(vs) > 1 else fmt(m))
            if label != ref:
                r0 = refstats.get(k)
                if r0 and m:
                    ratio = m / r0 if unit in SLOWER_IS_BIGGER else r0 / m
                    row.append(f"{ratio:.2f}")
                else:
                    row.append("")
        print("| " + " | ".join(row) + " |")


if __name__ == "__main__":
    main(sys.argv[1:])
