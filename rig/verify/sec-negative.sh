#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run the security negative tests inside the guest, and show the guest survived.
#
# Each check fires one ioctl a hostile guest would use to reach past its VM and
# asserts the backend turns it away (see sec-negative.c). This wrapper builds the
# binary if it is not already built, runs it, and prints the guest dmesg tail so
# a guest oops shows up. The other half of "never crashes the host" is on the
# host: after this run, confirm host dmesg is clean and the backend is still
# serving (TESTING.md §"Security negative tests").
#
# Usage: sec-negative.sh [-- args passed to sec-negative]
#   e.g. sec-negative.sh -- --kms /dev/dri/card1     (to include the KMS tests)
#
# Exit status is the number of tests that FAILED (a dangerous request accepted).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="$here/bin/sec-negative"

[ -x "$bin" ] || "$here/build.sh" >&2

echo "== guest dmesg before =="
dmesg 2>/dev/null | tail -n 1 || true

set +e
"$bin" "$@"
rc=$?
set -e

echo "== guest dmesg after =="
# Anything the run added, and in particular any WARN/BUG/oops.
dmesg 2>/dev/null | tail -n 20 || true

if dmesg 2>/dev/null | tail -n 40 | grep -Eiq 'oops|BUG:|general protection|call trace'; then
    echo "sec-negative: guest kernel complained -- capture full dmesg" >&2
    rc=$((rc == 0 ? 1 : rc))
fi

echo
echo "Now on the HOST: check dmesg is clean (no nvidia-drm/NVKMS WARN, no oops)"
echo "and the backend is still serving. A refusal should appear in the backend"
echo "log under RUST_LOG=debug; a host oops is a FAIL of the fix, not a pass."
exit "$rc"
