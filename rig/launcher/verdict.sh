# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's verdict: what the probe said, from the console, as the
# launcher's exit status (its header, "Exit status"). Sourced by the
# launcher.

# What the probe said. The rig's probes (rig/guest-image/probes) end with one
# verdict line,
#   NVGPU_PROBE_DONE probe=<name> result=PASS|FAIL pass=N fail=N skip=N
# and that line is the result: the tools they run print PASS/FAIL lines of
# their own, some of them expected (cuda-smoke says FAIL where a probe wants
# it to fail). The console comes through a terminal, so lines end in \r;
# what is echoed from it goes through clean first (the guest wrote it).
verdict() {
    # console_watch stopped the VM: whatever the console says, the run failed.
    if grep -a -q -F "$CON_CAP_LINE" "$CONSOLE"; then
        echo "result: FAIL (the console log passed NVGPU_LOG_MAX_MIB=$LOG_MAX_MIB; the VM was stopped)"
        exit 1
    fi
    DONE=$(grep -a -E '^NVGPU_PROBE_DONE ' "$CONSOLE" | tail -n 1 | clean || true)
    if [ -n "$DONE" ]; then
        grep -a -E '^\[[A-Za-z0-9_-]+\] FAIL ' "$CONSOLE" | clean | head -n 20 | sed 's/^/  /' || true
        [ "$BACKEND_ALIVE" = 1 ] || echo "note: the backend had exited by the end of the run; see $BLOG"
        echo "probe:   ${DONE#NVGPU_PROBE_DONE }"
        if [ "$RC" = 124 ] || [ "$RC" = 137 ]; then
            echo "result: TIMEOUT -- the probe finished but the guest did not power off within ${TIMEOUT}s"
            exit 124
        fi
        [ "$RC" = 0 ] || echo "note: the VMM exited with status $RC"
        case $DONE in
            *" result=PASS"*) echo "result: PASS"; exit 0 ;;
            *"reason=timeout"*) echo "result: FAIL (the probe's own watchdog)"; exit 1 ;;
            *) echo "result: FAIL"; exit 1 ;;
        esac
    fi
    # Older probes (the root layout's) print only PASS and FAIL lines; a word
    # match, so "FAILED" in a kernel message is not counted, but the probe's own
    # "FAIL: ..." is.
    NPASS=$(grep -c -E '(^|[^[:alnum:]_])PASS([^[:alnum:]_]|$)' "$CONSOLE" || true)
    NFAIL=$(grep -c -E '(^|[^[:alnum:]_])FAIL([^[:alnum:]_]|$)' "$CONSOLE" || true)
    grep -a -E '(^|[^[:alnum:]_])FAIL([^[:alnum:]_]|$)' "$CONSOLE" | clean | head -n 20 | sed 's/^/  /' || true
    [ "$BACKEND_ALIVE" = 1 ] || echo "note: the backend had exited by the end of the run; see $BLOG"
    if [ "$RC" = 124 ] || [ "$RC" = 137 ]; then
        echo "result: TIMEOUT -- the guest did not power off within ${TIMEOUT}s ($NPASS PASS, $NFAIL FAIL)"
        exit 124
    fi
    [ "$RC" = 0 ] || echo "note: the VMM exited with status $RC"
    if [ "$NFAIL" -gt 0 ]; then
        echo "result: FAIL ($NPASS PASS, $NFAIL FAIL)"
        exit 1
    elif [ "$NPASS" -eq 0 ]; then
        echo "result: NO RESULT -- no PASS or FAIL line on the console"
        exit 2
    fi
    echo "result: PASS ($NPASS PASS)"
}
