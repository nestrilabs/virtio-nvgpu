# `gen/` — generated ABI tables

Checked in **and** reproducible. Both properties matter: a contributor must be
able to read the tables without running anything, and regenerate them without
asking anyone.

NVIDIA's kernel driver ABI is not stable — ioctl struct layouts change between
releases. The tables here map driver versions to struct layouts and to the set
of commands that exist and are safe to forward.

Two halves, with different risk:

- **The struct half is derived mechanically** from NVIDIA's published
  `open-gpu-kernel-modules` at each tag, by compiling a probe per field and
  reading back `sizeof`/`offsetof`. Nothing is transcribed by hand.
- **The judgement half** — which commands exist, and which are safe to expose —
  follows gVisor's `nvproxy` upstream.

Profiles key off **ranges, not points**: a driver release between two known
versions selects the lower profile rather than requiring a new row. This is why
the per-release cost is small rather than open-ended.

The generator must stay runnable by someone who does not work on this project.
