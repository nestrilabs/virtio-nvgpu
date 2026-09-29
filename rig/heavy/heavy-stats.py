#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Frame-time statistics for rig/rig-heavy.sh.

    heavy-stats.py LABEL=GLOB [LABEL=GLOB ...]      a table, one row a label
    heavy-stats.py --runs LABEL=GLOB ...            and a row per run

Each GLOB names frames.txt files (one frame time a line, ms). Per run: the
average rate (frames over their total time), the median, p99 and p99.9
frame times, the 1% and 0.1% lows (the average rate over the slowest 1% and
0.1% of frames, as MangoHud and most reviewers define them) and the
coefficient of variation of the frame time. A label's row is the mean of its
runs, +- half their range.
"""
import glob
import statistics
import sys


def run_stats(path):
    ft = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                try:
                    v = float(line)
                except ValueError:
                    continue
                if v > 0:
                    ft.append(v)
    if len(ft) < 3:
        return None
    s = sorted(ft)
    n = len(s)

    def pct(p):
        return s[min(n - 1, int(p / 100.0 * n))]

    def low(frac):
        k = max(1, int(n * frac))
        worst = s[-k:]
        return 1000.0 * k / sum(worst)

    mean = sum(ft) / n
    return {
        "frames": n,
        "fps": 1000.0 * n / sum(ft),
        "p50": pct(50),
        "p99": pct(99),
        "p999": pct(99.9),
        "low1": low(0.01),
        "low01": low(0.001),
        "cv": statistics.pstdev(ft) / mean,
    }


COLS = [("fps", "avg fps", "{:.1f}"), ("low1", "1% low", "{:.1f}"), ("low01", "0.1% low", "{:.1f}"),
        ("p50", "p50 ms", "{:.3f}"), ("p99", "p99 ms", "{:.3f}"), ("p999", "p99.9 ms", "{:.3f}"),
        ("cv", "CV", "{:.3f}")]


def fmt(vals, f):
    m = sum(vals) / len(vals)
    h = (max(vals) - min(vals)) / 2
    return (f.format(m) + (" ± " + f.format(h) if len(vals) > 1 else ""))


def main(argv):
    per_run = False
    if argv and argv[0] == "--runs":
        per_run = True
        argv = argv[1:]
    print("| run | n | " + " | ".join(c[1] for c in COLS) + " |")
    print("|---|---|" + "---|" * len(COLS))
    for arg in argv:
        label, _, pat = arg.partition("=")
        rows = []
        for p in sorted(glob.glob(pat)):
            r = run_stats(p)
            if r is None:
                continue
            rows.append(r)
            if per_run:
                print("| " + p + " | 1 | " + " | ".join(c[2].format(r[c[0]]) for c in COLS) + " |")
        if not rows:
            print("| " + label + " | 0 |" + " -- |" * len(COLS))
            continue
        print("| **" + label + "** | " + str(len(rows)) + " | " +
              " | ".join(fmt([r[c[0]] for r in rows], c[2]) for c in COLS) + " |")


if __name__ == "__main__":
    main(sys.argv[1:])
