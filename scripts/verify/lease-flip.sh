#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build lease-flip if needed, then light up a leased output and measure flip
# pacing. Run inside the guest.
#
# Give it the DRM file the guest owns the output through: an adopted lease fd's
# device path, or a guest card node in compositor-VM mode.
#
# Usage: lease-flip.sh --device /dev/dri/cardN [--frames N]
#        lease-flip.sh --fd 5   [--frames N]     (a lease fd already open)
#
# PASS is the modeset completing and every flip returning within 3 s; the
# reported mean interval should sit at the monitor's refresh period.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="$here/bin/lease-flip"

[ -x "$bin" ] || "$here/build.sh" >&2

exec "$bin" "$@"
