# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's helpers: a string made safe for the config, a
# terminal or a pkill pattern, a log with a cap, CPU lists, and whether a pid
# is still the run's. Sourced by the launcher (its header lists the pieces);
# nothing here runs on its own.

# A string as a JSON string literal. Paths and the command line are the only
# things that go into the config from outside, and a quote in either would
# otherwise make it say something else.
json_str() {
    local s=$1
    case $s in *[[:cntrl:]]*) die "control character in \"$s\"" ;; esac
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '"%s"' "$s"
}

# Text from the guest, or from a log it can write into, made fit for a
# terminal: control characters but tab and newline dropped, so no escape
# sequence (a title, an OSC 52 clipboard write) reaches root's terminal.
clean() {
    LC_ALL=C tr -d '\000-\010\013-\037\177'
}

# capped FILE: append standard input to FILE, at most LOG_MAX bytes of it;
# the rest is read and dropped (the writer is never blocked or killed for
# it), with one line saying so. The backend's log goes through it: a
# compromised backend may not fill the filesystem. (The console log, which
# the guest writes, is held to the same cap by console_watch: guest.sh.)
capped() {
    head -c "$LOG_MAX" >> "$1"
    if [ "$(head -c 1 | wc -c)" -gt 0 ]; then
        printf '\n[run-guest: this log reached NVGPU_LOG_MAX_MIB=%s; the rest was dropped]\n' \
            "$LOG_MAX_MIB" >> "$1"
        cat > /dev/null
    fi
}

# A literal string as an extended regular expression, for pkill -f.
re() {
    printf '%s' "$1" | sed 's/[][\\.*^$+?(){}|]/\\&/g'
}

# A CPU list (the header's "Placement") -- digits, commas and ranges -- as
# the list of numbers the VMM's config takes.
cpu_list() { # cpu_list NAME VALUE -> "8, 9, 10"
    local v=$2 out=() parts part a b i
    [[ $v =~ ^[0-9]+(-[0-9]+)?(,[0-9]+(-[0-9]+)?)*$ ]] || die "$1=$v: CPUs, as 0-3,8"
    IFS=, read -r -a parts <<<"$v"
    for part in "${parts[@]}"; do
        a=${part%-*} b=${part#*-}
        a=$((10#$a)) b=$((10#$b))
        [ "$a" -le "$b" ] && [ "$b" -lt 4096 ] || die "$1=$v: $part is not a range of CPUs"
        for ((i = a; i <= b; i++)); do out+=("$i"); done
    done
    local IFS=,
    echo "${out[*]}" | sed 's/,/, /g'
}

# Whether $1 is still this run's process: its command line names $2 (the
# run's config for the VMM, its socket for the backend). A child that exited
# during the run has been reaped, and its pid may since be someone else's.
still_ours() {
    local cmd
    [ -n "$2" ] || return 1
    # The redirection's own failure (the process gone) is the shell's to
    # report, so it is inside the braces the 2>/dev/null covers.
    cmd=$({ tr '\0' ' ' < "/proc/$1/cmdline"; } 2>/dev/null) || return 1
    [[ $cmd == *"$2"* ]]
}
