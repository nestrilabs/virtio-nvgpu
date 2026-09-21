# `protocol/` — shared wire format and ABI definitions

**Licence: BSD-3-Clause OR GPL-2.0+** (`LICENSE-BSD-3-Clause`,
`LICENSE-GPL-2.0`) — dual, deliberately.

Apache-2.0 is not GPL-2.0-compatible, so a GPL kernel module cannot include an
Apache-2.0 header. Dual licensing the definitions **both halves must agree on**
is what lets one repository hold a GPL guest driver and an Apache-2.0 host crate
honestly.

This directory holds only definitions that cross the boundary: virtqueue message
layouts, request and response headers, and the ABI descriptions both sides read.
Nothing here should contain logic.

Files in this directory carry `SPDX-License-Identifier: BSD-3-Clause OR
GPL-2.0+`. The dual licence binds **definitions authored here**; code ported
from other projects keeps its original terms and cannot be relicensed.
