#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Frame-time statistics from MangoHud CSV logs (rig/rig-framepace.sh).

Usage: framepace-stats.py [--hz HZ] [--json] [--label L] FILE.csv [FILE.csv ...]

Each file is one run. MangoHud 0.8 writes two lines of system information,
then a header naming the columns (fps, frametime, ..., elapsed), then one row
per frame with log_interval=0. Only `frametime` (milliseconds) is read.

Per run: frames, mean, p50, p99, p99.9 and max frame time (ms), and two stutter
counts: frames longer than 1.5x the refresh period (--hz, the monitor's rate;
for a workload that keeps up with the display, each is a missed vblank) and
frames longer than 2x the run's own median (for one that does not). With
several files, a last line aggregates them all (the frames pooled) and gives
the spread of the per-run p99 and stutter counts.
"""
import json
import math
import sys


def load(path):
    rows = []
    with open(path, newline="") as f:
        lines = f.read().splitlines()
    col = None
    for i, line in enumerate(lines):
        cells = line.split(",")
        if col is None:
            if "frametime" in cells:
                col = cells.index("frametime")
            continue
        if len(cells) <= col:
            continue
        try:
            rows.append(float(cells[col]))
        except ValueError:
            continue
    if col is None:
        raise SystemExit(f"{path}: no frametime column")
    return rows


def pct(sorted_v, p):
    if not sorted_v:
        return float("nan")
    k = (len(sorted_v) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return sorted_v[lo]
    return sorted_v[lo] + (sorted_v[hi] - sorted_v[lo]) * (k - lo)


def stats(v, hz):
    s = sorted(v)
    n = len(s)
    mean = sum(s) / n if n else float("nan")
    p50 = pct(s, 50)
    period = 1000.0 / hz if hz else None
    return {
        "frames": n,
        "mean": mean,
        "fps": 1000.0 / mean if mean else float("nan"),
        "p50": p50,
        "p99": pct(s, 99),
        "p999": pct(s, 99.9),
        "max": s[-1] if s else float("nan"),
        "stdev": math.sqrt(sum((x - mean) ** 2 for x in s) / n) if n else float("nan"),
        "stutter_refresh": sum(1 for x in s if period and x > 1.5 * period),
        "stutter_median": sum(1 for x in s if x > 2 * p50),
    }


def fmt(label, st):
    return (
        f"{label:<28} n={st['frames']:>6} fps={st['fps']:7.1f} mean={st['mean']:6.3f} "
        f"p50={st['p50']:6.3f} p99={st['p99']:6.3f} p99.9={st['p999']:7.3f} max={st['max']:7.2f} "
        f"sd={st['stdev']:5.3f} >1.5T={st['stutter_refresh']:>4} >2xp50={st['stutter_median']:>4}"
    )


def main(argv):
    hz, as_json, label = 240.0, False, None
    files = []
    it = iter(argv)
    for a in it:
        if a == "--hz":
            hz = float(next(it))
        elif a == "--json":
            as_json = True
        elif a == "--label":
            label = next(it)
        else:
            files.append(a)
    if not files:
        print(__doc__, file=sys.stderr)
        return 2
    runs, pooled = [], []
    for f in files:
        v = load(f)
        pooled += v
        st = stats(v, hz)
        st["file"] = f
        runs.append(st)
    agg = stats(pooled, hz)
    if len(runs) > 1:
        agg["runs"] = len(runs)
        for k in ("p99", "p999", "stutter_refresh", "stutter_median", "fps"):
            vals = [r[k] for r in runs]
            agg[k + "_min"], agg[k + "_max"] = min(vals), max(vals)
        # Stutter per 1000 frames, per run, so runs of different lengths compare.
        agg["stutter_refresh_per_k"] = 1000.0 * agg["stutter_refresh"] / max(1, agg["frames"])
    if as_json:
        print(json.dumps({"label": label, "hz": hz, "runs": runs, "all": agg}, indent=1))
        return 0
    for r in runs:
        print(fmt(r["file"].rsplit("/", 1)[-1][-28:], r))
    if len(runs) > 1:
        print(fmt(f"{label or 'all'} ({len(runs)} runs)", agg))
        print(
            f"{'':<28} per-run p99 {agg['p99_min']:.3f}-{agg['p99_max']:.3f} ms, "
            f">1.5T {agg['stutter_refresh_min']}-{agg['stutter_refresh_max']}, "
            f"fps {agg['fps_min']:.1f}-{agg['fps_max']:.1f}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
