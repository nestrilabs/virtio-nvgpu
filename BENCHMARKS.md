# Benchmarks

**Every number here was produced by running something**, on hardware named by
the GPU in it. Where a figure is derived rather than observed, it says so.

The short version: **above about 2 ms a frame, a guest renders within 2% of the
same machine's bare metal, and costs the same CPU.** Below that, the cost of
waiting for the GPU dominates a frame that barely exists.

## What was measured, and how

`nesprobe` — the calibrated headless load probe from
[nesbox](https://github.com/nestrilabs/nesbox) (`tools/nesprobe`), used
unchanged. No compositor, no swapchain, no encode: it renders to an offscreen
attachment and never presents, so what it reports is the stack's cost and not a
window system's. `--cost` dials the per-frame fragment shader load, which is
what lets one probe be submission-bound at one end and GPU-bound at the other.

- 1920×1080, 30 s per run after an **8 s discard**, **three reps** of every point
- the figure is the **median of three**, with the rep-to-rep spread beside it;
  a difference smaller than the spread is reported as noise, because it is
- the guest is compared **only against its own host**, on the same machine,
  minutes apart

The warm-up is not optional. An idle GPU sits in a low clock state and takes
seconds to ramp; a p99 taken without discarding that window is a measurement of
the clock ramp. This project has made that mistake and written it up.

## The frame time

**RTX 3060**, driver 595.99.02, host Ubuntu 26.04 on a Ryzen 7 9850X3D. Guest:
2 vCPUs, Linux 7.2, module at its defaults.

| `--cost` | host p50 | guest p50 | Δ | rep spread h/g |
|---|---|---|---|---|
| 8000 | 38.991 ms | 38.890 ms | **−0.3%** | 0.0% / 0.1% |
| 2000 | 9.893 | 9.814 | **−0.8%** | 0.0% / 0.5% |
| 400 | 1.978 | 2.016 | **+1.9%** | 0.1% / 4.7% |
| 100 | 0.510 | 1.125 | +120.6% | 0.0% / 6.9% |
| 0 | 0.049 | 0.405 | +726.5% | 0.0% / 10.4% |

A frame that takes the host 2 ms or more is within 2% in the guest. A frame that
takes 0.05 ms is not, and the reason is not forwarding — it is that the guest
sleeps waiting for the GPU and the wake costs ~0.35 ms, whatever the frame cost.
A 60 Hz frame is 16.7 ms.

## The CPU

A shared GPU is only worth sharing if its guests are cheap. Unpaced at ~100 fps,
12 s, one guest:

| | frames | CPU used | CPU per frame |
|---|---|---|---|
| host, bare metal | 1009 | 0.40 s (3.3%) | 0.40 ms |
| **guest** | 1007 | **0.39 s (3.2%)** | **0.39 ms** |

Nothing is spent forwarding a render loop because nothing is forwarded. Over a
full sweep the guest drew **813,691 frames** and the backend served **13,792
messages** — one crossing per 59 frames, nearly all of it device setup. The
count is the backend's own tally, not an estimate from a log.

> An earlier build had no `.poll` on its character devices. A `file_operations`
> with a NULL `.poll` is reported ready by the VFS every time it is asked, so
> the user-mode driver's wait never waited and the guest spun: **12.26 s of CPU
> for the same 12 s of work**, a whole core per guest. The fix is a real `.poll`
> and an event queue that carries the host's readiness across. It is worth
> knowing that this class of bug is invisible in a frame-rate benchmark — the
> spinning guest was *faster* than bare metal.

## The whole chain

Not a synthetic load: a Wayland client presenting through a compositor in the
guest, captured and encoded on the client's own device by Vulkan Video, with a
receiver on the far end of the video socket.

```
receiver: frames=617 bytes=23643487
gap_ms mean 16.63  p50 16.67  p99 26.03  max 26.16   (60 Hz is 16.67)
ffmpeg -f null -: exit 0, no frame errors
```

Arrival spacing rather than a frame count, because a count cannot tell a slower
pipeline from an on-time one that starts a second later — and here it was the
second.

**The known cost:** frames arriving more than 25 ms after the one before, out of
~600 — **53**, against 4 for a build that spun instead of waiting. Nothing is
dropped and the mean does not move, but the tail is real, it is not understood,
and it is the open question.

## Re-taking these

The harnesses are in the private engineering notes rather than here, because
they drive specific machines. What they do is simple enough to restate:

```sh
# bare metal: the probe, three reps of each cost, 30 s after an 8 s discard
for rep in 1 2 3; do
  for cost in 0 100 400 2000 8000; do
    nesprobe --device <n> --cost $cost --seconds 30 --warmup 8
  done
done

# guest: the same, inside, with the module at its defaults
```

Then compare each guest median against the host median **from the same
machine**. Do not compare a millisecond figure from one box with a millisecond
figure from another; compare the ratios.

**Check for a stray VM before believing anything.** A benchmark taken beside a
guest somebody forgot to shut down looks like an ordinary result — slightly
slower each run — and that is how a harness bug here cost a full set of numbers.

## What these numbers do not support

- **No comparison with another hypervisor.** None was run. "Faster than X" is
  not a claim this data can carry, and neither is "the fastest".
- **No claim about several guests at once.** One guest, always. The measurement
  that decides how many boxes a card can hold has not been taken.
- **No claim about a real game.** `nesprobe` is synthetic; the only real
  pipeline here is our own encode chain.
- **One card, one driver.** RTX 3060 at 595.99.02. An RTX A2000 at 615.71.09
  renders but has not been benchmarked.
