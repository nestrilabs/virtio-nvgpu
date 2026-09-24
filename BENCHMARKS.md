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
| 8000 | 38.991 ms | 38.847 ms | **−0.4%** | 0.0% / 0.0% |
| 2000 | 9.893 | 9.821 | **−0.7%** | 0.0% / 0.1% |
| 400 | 1.978 | 2.011 | **+1.7%** | 0.1% / 0.0% |
| 100 | 0.510 | 0.546 | +7.1% | 0.0% / 0.4% |
| 0 | 0.049 | 0.069 | +40.8% | 0.0% / 2.9% |

A frame that takes the host 2 ms or more is within 2% in the guest. A frame that
takes 0.05 ms is not, and the reason is not forwarding — it is that the guest
sleeps waiting for the GPU, and the wake costs about 0.02 ms whatever the frame
cost. A 60 Hz frame is 16.7 ms.

## The CPU

A shared GPU is only worth sharing if its guests are cheap. Unpaced at ~100 fps,
12 s, one guest:

| | frames | CPU used | CPU per frame |
|---|---|---|---|
| host, bare metal | 1009 | 0.40 s (3.3%) | 0.40 ms |
| **guest** | 1016 | **0.37 s (3.1%)** | **0.36 ms** |

Nothing is spent forwarding a render loop because nothing is forwarded. Over a
full sweep the guest drew **813,691 frames** and the backend served **13,792
messages** — one crossing per 59 frames, nearly all of it device setup. The
count is the backend's own tally, not an estimate from a log.

> **Two bugs of our own are worth knowing about, because both were invisible in
> an ordinary benchmark.**
>
> The backend was serving the buffers a guest posts on the *event* queue as
> though they were requests — answering each with an error, handing it back, and
> being kicked again: 7.4 million callbacks in five seconds. It surfaced only
> when two guests ran at once and one of them failed to start its client, so the
> storm had the machine to itself. It had been running under every measurement
> taken since the event queue landed, and cost the guest +726% at cost 0 (now
> +41%) and every one of the 53 late frames above.
>
> Before that, an earlier build had no `.poll` on its character devices. A `file_operations`
> with a NULL `.poll` is reported ready by the VFS every time it is asked, so
> the user-mode driver's wait never waited and the guest spun: **12.26 s of CPU
> for the same 12 s of work**, a whole core per guest. The fix is a real `.poll`
> and an event queue that carries the host's readiness across. It is worth
> knowing that this class of bug is invisible in a frame-rate benchmark — the
> spinning guest was *faster* than bare metal.

## Several guests on one card

The claim the whole design rests on, and the last one to be checked. Each guest
gets its own backend and its own socket; they share one read-only root image.

`nesprobe --cost 2000` in each, 30 s after an 8 s discard:

| guests | each | total | p50 each |
|---|---|---|---|
| 1 | 102.9 fps | 102.9 | 9.72 ms |
| 2 | 51.41, 50.95 | 102.36 | 19.576, 19.579 ms |
| 4 | 25.84, 26.49, 25.57, 25.79 | 103.69 | 39.165, 39.164, 39.168, 39.165 ms |

**The total does not move as guests are added, and the split is even to four
decimal places.** Bare metal on the same card and load is 100.9 fps, so four
guests together take nothing off the card that one process does not.

Rendering stays correct under contention — the offscreen regression in four
guests at once gives `red=11858 blue=53678 other=0` in every one, identical to a
single guest.

And four of them **encoding** at the same time, which is what the product
actually does: 575, 576, 577 and 573 frames, each paced at 16.67 ms — 60 Hz
exactly — with no late frames and no NVENC session limit reached at four.

Four is what was run, not a limit. Each guest has 2 vCPUs on an 8-core host, so
four is also where the host's CPUs are fully committed, and vkcube at 720p is a
small load: four guests running a real game is a different measurement.

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

Frames arriving more than 25 ms after the one before, out of ~600: **zero**,
with p99 spacing of 18.0 ms. An earlier version of this file reported 53 and
called it an open question; that was a bug of ours in the event queue, described
below, and not a property of the design.

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
- **Four guests, not "many".** Four ran; eight has not been tried, and neither
  has a guest doing something heavier than vkcube at 720p.
- **No claim about a real game.** `nesprobe` is synthetic; the only real
  pipeline here is our own encode chain.
- **One card, one driver.** RTX 3060 at 595.99.02. An RTX A2000 at 615.71.09
  renders but has not been benchmarked.
