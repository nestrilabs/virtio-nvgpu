# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's guest: the hook, then the VM run to its end with the
# console log held to its cap. Sourced by the launcher.

# The hook (NVGPU_BEFORE_VMM): what must happen between the backend and the
# guest. The capture test's helper imports its buffers now, and its ids and
# tokens reach the guest as kernel command-line words, added to the config
# already written.
run_hook() {
    [ $PRIV = user ] || die "NVGPU_BEFORE_VMM: unprivileged runs only"
    HOOK_WORDS=$(NVGPU_RUN_DIR=$RUN NVGPU_INJECT_SOCKET=$INJECT_SOCK NVGPU_HOOK_LOG=$LOGS/$TAG.hook.log \
        bash -c "$NVGPU_BEFORE_VMM" {TAG_FD}>&- | tail -n 1) || die "NVGPU_BEFORE_VMM failed; see $LOGS/$TAG.hook.log"
    [[ $HOOK_WORDS =~ ^[A-Za-z0-9_.,:=\ -]*$ ]] || die "NVGPU_BEFORE_VMM printed characters a command line cannot take"
    if [ -n "$HOOK_WORDS" ]; then
        echo "hook:    adds to the guest command line: $HOOK_WORDS" >&2
        # nesbox's config, and crosvm's argv log, carry the boot line whole;
        # crosvm's own argv carries it after -p.
        sed -i "0,/init=\/opt\/nvgpu\/$PROBE/s//init=\/opt\/nvgpu\/$PROBE $HOOK_WORDS/" "$CFG"
        for i in "${!VMM_CMD[@]}"; do
            if [ "${VMM_CMD[$i]}" = -p ]; then
                VMM_CMD[i + 1]+=" $HOOK_WORDS"
            fi
        done
    fi
}

# console_over: whether the console log has passed LOG_MAX bytes; if so it
# is cut back to them and says so, once.
console_over() {
    [ "$(stat -c %s "$CONSOLE" 2>/dev/null || echo 0)" -gt "$LOG_MAX" ] || return 1
    truncate -s "$LOG_MAX" "$CONSOLE"
    printf '\n%s\n' "$CON_CAP_LINE" >> "$CONSOLE"
}

# The guest, run to its end ($RC is the VMM's status). stdin is /dev/null
# unless the run is interactive: nesbox puts a terminal on stdin into raw
# mode for the guest console, and a VMM killed by the timeout never puts it
# back. It runs in the background and is waited for, so that an interrupt
# here reaches the cleanup, which stops it, instead of leaving it to run out
# its timeout.
run_guest() {
    echo "guest:   $PROBE on $(basename -- "$DISK"), $VCPUS vCPU / $MEM_MIB MiB, ${TIMEOUT}s" >&2
    [ -z "$PLACEMENT" ] || echo "placement: $PLACEMENT" >&2
    if [ "$VMM_JAIL" = on ]; then
        echo "vmm:     as $VMM_USER under $JAILER, jail $JAIL_ROOT, own network namespace" >&2
    fi
    if [ "$VMM_KIND" = crosvm ]; then
        echo "vmm:     crosvm, sandbox $CROSVM_SANDBOX$([ "$CROSVM_SANDBOX" = on ] && [ "$CROSVM_UVM" = 1 ] && echo ', nvgpu frontend jailed')$([ "$VMM_OWN_NETNS" = true ] && echo ', own user and network namespace')" >&2
    fi
    # The console log starts with what the reader must know before the guest's
    # first line: a VMM without its sandbox. The VMM appends to it directly: a
    # pipe through a capped writer, as the backend's log has, stalled guests
    # (their console output slowed, and a guest thread writing to hvc0 held up
    # a GPU client's calls; mpv hung within a second). Its size is bounded
    # instead by console_watch, which stops the VM past NVGPU_LOG_MAX_MIB.
    : > "$CONSOLE"
    [ -z "$CROSVM_NOTE" ] || printf '%s\n' "$CROSVM_NOTE" >> "$CONSOLE"
    exec {CON_W}>>"$CONSOLE"
    if [ "$INTERACTIVE" = 1 ]; then
        # The terminal is the guest's console; the log still gets everything.
        # --foreground: timeout otherwise moves itself and nesbox into a process
        # group of their own, which the terminal stops (SIGTTIN/SIGTTOU) as soon
        # as nesbox reads it or makes it raw. An explicit <&0 because a
        # background job of a script otherwise reads /dev/null.
        echo "guest console on this terminal; exit the guest's shell to power off" >&2
        TTY_STATE=$(stty -g 2>/dev/null) || TTY_STATE=
        timeout --foreground -k 10 "$TIMEOUT" "${VMM_CMD[@]}" {SLOT_FD}>&- {TAG_FD}>&- <&0 \
            >&"$CON_W" 2>&1 {CON_W}>&- &
        VMM_PID=$!
        tail -n +1 -f --pid="$VMM_PID" "$CONSOLE" {CON_W}>&- &
        TAIL_PID=$!
    else
        timeout -k 10 "$TIMEOUT" "${VMM_CMD[@]}" {SLOT_FD}>&- {TAG_FD}>&- < /dev/null \
            >&"$CON_W" 2>&1 {CON_W}>&- &
        VMM_PID=$!
        TAIL_PID=
    fi
    exec {CON_W}>&-
    CON_CAP_LINE="[run-guest: the console log passed NVGPU_LOG_MAX_MIB=$LOG_MAX_MIB; the VM was stopped]"
    # console_watch: the guest writes the console log; past LOG_MAX bytes the VM
    # is stopped (TERM to the VMM's timeout) and the log cut back, so no guest
    # fills the filesystem. What a guest writes between two looks is cut too
    # (console_over again once the VMM is gone). The VMM is this shell's
    # child, not the watcher's: once it has exited and been reaped its pid
    # may be another process's, which the watcher neither watches nor
    # signals (still_ours).
    (
        exec {SLOT_FD}>&- {TAG_FD}>&-
        while still_ours "$VMM_PID" "$VMM_MARK"; do
            if console_over; then
                still_ours "$VMM_PID" "$VMM_MARK" && kill -TERM "$VMM_PID" 2>/dev/null
                break
            fi
            sleep 0.5
        done
    ) &
    CON_WATCH=$!
    RC=0
    wait "$VMM_PID" || RC=$?
    VMM_PID=
    kill "$CON_WATCH" 2>/dev/null || true
    wait "$CON_WATCH" 2>/dev/null || true
    grep -a -q -F "$CON_CAP_LINE" "$CONSOLE" || console_over || true
    [ -z "$TAIL_PID" ] || wait "$TAIL_PID" 2>/dev/null || true
    if [ -n "$TTY_STATE" ]; then
        stty "$TTY_STATE" 2>/dev/null || true
        TTY_STATE=
        echo
    fi
}
