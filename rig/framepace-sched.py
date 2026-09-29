#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""How the host scheduled a VM's threads over an interval (rig/rig-framepace.sh).

Usage: framepace-sched.py SECS PID [PID ...]

Reads /proc/PID/task/*/schedstat (time on a CPU, time runnable but waiting for
one, timeslices) and /proc/PID/task/*/status (context switches) for every
thread of each process, twice, SECS apart, and prints per thread, grouped by
name with digits folded (vcpu0..3 -> vcpu#): CPU use, the time spent waiting
for a CPU while runnable (the host scheduler's delay: a vCPU that waits is a
guest that stops), the mean wait per timeslice, and involuntary switches (the
thread was preempted). Threads that did nothing are left out.
"""
import os
import re
import sys
import time


def threads(pid):
    out = {}
    base = f"/proc/{pid}/task"
    try:
        tids = os.listdir(base)
    except OSError:
        return out
    for tid in tids:
        try:
            with open(f"{base}/{tid}/comm") as f:
                comm = f.read().strip()
            with open(f"{base}/{tid}/schedstat") as f:
                run, wait, slices = (int(x) for x in f.read().split()[:3])
            nvcs = 0
            with open(f"{base}/{tid}/status") as f:
                for line in f:
                    if line.startswith("nonvoluntary_ctxt_switches"):
                        nvcs = int(line.split()[1])
            cpus = ""
            with open(f"{base}/{tid}/status") as f:
                for line in f:
                    if line.startswith("Cpus_allowed_list"):
                        cpus = line.split()[1]
        except OSError:
            continue
        out[(pid, tid)] = (comm, run, wait, slices, nvcs, cpus)
    return out


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    secs = float(argv[0])
    pids = argv[1:]
    a = {}
    for p in pids:
        a.update(threads(p))
    t0 = time.monotonic()
    time.sleep(secs)
    b = {}
    for p in pids:
        b.update(threads(p))
    dt = time.monotonic() - t0
    groups = {}
    for k, (comm, run, wait, sl, nv, cpus) in b.items():
        if k not in a:
            continue
        _, run0, wait0, sl0, nv0, _ = a[k]
        d = (run - run0, wait - wait0, sl - sl0, nv - nv0)
        if d[0] == 0 and d[2] == 0:
            continue
        name = re.sub(r"\d+", "#", comm)
        g = groups.setdefault(name, [0, 0, 0, 0, 0, set()])
        for i in range(4):
            g[i] += d[i]
        g[4] += 1
        g[5].add(cpus)
    print(f"# {dt:.1f}s; per thread group: threads, cpu%, runq wait ms/s, wait/slice us, preempted/s, allowed cpus")
    for name, (run, wait, sl, nv, n, cpus) in sorted(groups.items(), key=lambda x: -x[1][0]):
        print(
            f"{name:<20} n={n:<3} cpu={100 * run / 1e9 / dt:6.1f}% wait={wait / 1e6 / dt:7.3f}ms/s "
            f"wait/slice={(wait / 1e3 / sl) if sl else 0:7.1f}us preempt={nv / dt:7.1f}/s cpus={','.join(sorted(cpus))}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
