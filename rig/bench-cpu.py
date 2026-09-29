#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""rig/rig-bench.sh's CPU per suite group, as BENCH lines.

Usage: rig/bench-cpu.py SAMPLES SECTIONS

SAMPLES:  <host time> <backend ticks> <VMM ticks>, every 100 ms
SECTIONS: <host time of arrival> begin|end <group> <guest time>

Each section line carries the guest's time as well as the host's time of
arrival, which the console delays: the guest's clock, moved by the least
delay seen (an upper bound on the offset, to within the console's latency),
says when a group began and ended on the host's. The backend's and the VMM's
CPU over that interval is given as a percentage of one CPU.
"""
import os
import sys


def main(samples_path, sections_path):
    hz = os.sysconf("SC_CLK_TCK")
    samples = []
    with open(samples_path) as f:
        for line in f:
            p = line.split()
            if len(p) == 3:
                samples.append((float(p[0]), int(p[1]), int(p[2])))
    sections = []
    with open(sections_path) as f:
        for line in f:
            p = line.split()
            if len(p) == 4 and p[1] in ("begin", "end"):
                sections.append((float(p[0]), p[1], p[2], float(p[3])))
    if not samples or not sections:
        return
    off = min(a - g for a, _, _, g in sections)

    # A process not yet found, or already gone, reads as 0 ticks: those
    # samples say nothing.
    cols = {i: [(s[0], s[i]) for s in samples if s[i] > 0] for i in (1, 2)}

    def at(t, i):
        col = cols[i]
        for ts, v in col:
            if ts >= t:
                return v
        return col[-1][1] if col else 0

    begun = {}
    for _, kind, name, g in sections:
        h = g + off
        if kind == "begin":
            begun[name] = h
        elif name in begun and h > begun[name]:
            t0, dt = begun[name], h - begun[name]
            for i, who in ((1, "backend"), (2, "vmm")):
                pct = (at(h, i) - at(t0, i)) / hz / dt * 100
                print(f"BENCH cpu.{name}_{who} {pct:.1f} %")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
