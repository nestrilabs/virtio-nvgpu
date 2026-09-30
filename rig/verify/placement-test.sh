#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The launcher's placement (rig/launcher/placement.sh) against fake hosts'
# sysfs CPU trees: the layouts NVGPU_PIN works out, the pieces given by
# name winning over them, and what it refuses. No VM, no privilege; run by
# scripts/ci.sh fast.
#
# Hosts: "zen5", a Ryzen 9 9950X as the rig has it (16 cores, 32 threads,
# siblings i and i+16, two L3 domains of 8 cores, the rig's
# amd_pstate_prefcore_ranking), and "flat", 8 CPUs with no SMT, no L3 in
# sysfs and no ranking.
#
# Usage: rig/verify/placement-test.sh
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
LIB=$HERE/../launcher
W=$(mktemp -d "${TMPDIR:-/tmp}/placement-test.XXXXXX")
trap 'rm -rf "$W"' EXIT

# zen5: CPU c is core c%16, thread c/16; L3 of cores 0-7 and 8-15.
Z=$W/zen5
mkdir -p "$Z"
echo 0-31 >"$Z/online"
RANKS=(231 216 226 236 221 236 206 211 191 196 176 201 186 181 171 166)
for c in $(seq 0 31); do
    d=$Z/cpu$c
    mkdir -p "$d/topology" "$d/cache/index3" "$d/cpufreq"
    k=$((c % 16))
    echo "$k,$((k + 16))" >"$d/topology/thread_siblings_list"
    echo 3 >"$d/cache/index3/level"
    if [ "$k" -lt 8 ]; then echo 0-7,16-23; else echo 8-15,24-31; fi >"$d/cache/index3/shared_cpu_list"
    echo "${RANKS[$k]}" >"$d/cpufreq/amd_pstate_prefcore_ranking"
done
F=$W/flat
mkdir -p "$F"
echo 0-7 >"$F/online"
for c in $(seq 0 7); do mkdir -p "$F/cpu$c/topology" && echo "$c" >"$F/cpu$c/topology/thread_siblings_list"; done

# One placement, as the launcher computes it: a line of what it chose, or
# the refusal.
place() { # place HOST VCPUS [VAR=value...]
    local host=$1 vcpus=$2
    shift 2
    (
        # As the launcher's: said on stderr, and the end of the run (a
        # refusal inside a substitution ends only it, and the caller then
        # ends the run).
        die() {
            echo "refused: $*" >&2
            exit 1
        }
        # shellcheck source=../launcher/common.sh
        . "$LIB/common.sh"
        # shellcheck source=../launcher/placement.sh
        . "$LIB/placement.sh"
        PRIV=${PRIV:-user}
        for a in "$@"; do export "${a?}"; done
        export NVGPU_SYSFS_CPU=$W/$host
        # shellcheck disable=SC2034 # placement.sh's to read
        VCPUS=$vcpus
        placement_settings
        local s sets=
        for s in ${VCPU_SETS_J[@]+"${VCPU_SETS_J[@]}"}; do sets+="${sets:+ }${s// /}"; done
        echo "pins=[$sets] aff=$CPU_AFFINITY io=$IO_AFFINITY be=$BACKEND_CPUS smt=$GUEST_SMT cs=${CORE_SCHED_DEFAULT:-} said=${PLACEMENT:+yes}"
    ) 2>&1
}

FAIL=0
check() { # check NAME WANT HOST VCPUS [VAR=value...]
    local name=$1 want=$2 got
    shift 2
    got=$(place "$@")
    if [[ $got == *"$want"* ]]; then
        echo "ok   $name"
    else
        echo "FAIL $name: wanted \"$want\", got \"$got\""
        FAIL=1
    fi
}

# Nothing asked: nothing placed, nothing said, the VMM's own topology.
check "unset places nothing" "pins=[] aff= io= be= smt= cs= said=" zen5 8
check "off places nothing" "pins=[] aff= io= be= smt= cs= said=" zen5 8 NVGPU_PIN=off
# The layouts on the 9950X: CCD1 (the scheduler's last choice) first, the
# core of CPU 0 avoided.
check "cores: one thread of 8 cores of CCD1" "pins=[8 9 10 11 12 13 14 15] aff= io= be= smt=1" zen5 8 NVGPU_PIN=cores
check "smt: both threads of CCD1's 4 least preferred cores" \
    "pins=[10 26 13 29 14 30 15 31] aff= io= be= smt=2 cs=vm" zen5 8 NVGPU_PIN=smt
check "spread: alternating CCDs" "pins=[15 6 14 7 10 1 13 4]" zen5 8 NVGPU_PIN=spread
check "l3: one CCD as a set" "pins=[] aff=8-15,24-31 io= be= smt=" zen5 8 NVGPU_PIN=l3
check "core-sets: a core per vCPU" "pins=[14,30 15,31] aff= io= be= smt=1" zen5 2 NVGPU_PIN=core-sets
check "io=siblings" "io=24-31 be=24-31" zen5 8 NVGPU_PIN=cores:io=siblings
check "io=other" "io=0-7,16-23 be=0-7,16-23" zen5 8 NVGPU_PIN=cores:io=other
check "io=LIST" "io=2,3 be=2,3" zen5 4 NVGPU_PIN=cores:io=2,3
check "io=rest: CCD1's cores smt leaves" "io=8-9,11-12,24-25,27-28 be=8-9,11-12,24-25,27-28" zen5 8 NVGPU_PIN=smt:io=rest
check "io=rest with every core taken" "refused: NVGPU_PIN=cores:io=rest: io=rest: the vCPUs take every core" zen5 8 NVGPU_PIN=cores:io=rest
check "l3=CPU takes that CCD first" "pins=[1 17 4 20 6 22 7 23]" zen5 8 NVGPU_PIN=smt:l3=0
check "16 cores need CPU 0's" "refused: NVGPU_PIN=cores: 16 whole cores wanted, this host has 15" zen5 16 NVGPU_PIN=cores
check "avoid=none" "pins=[0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15]" zen5 16 NVGPU_PIN=cores:avoid=none
check "avoid=LIST leaves those cores out" "pins=[8 11 12 13]" zen5 4 NVGPU_PIN=cores:avoid=9-10,14-15
check "smt at 16: all of CCD1" "pins=[8 24 9 25 10 26 11 27 12 28 13 29 14 30 15 31] aff= io= be= smt=2" zen5 16 NVGPU_PIN=smt
check "smt wants an even count" "refused: NVGPU_PIN=smt: an even NVGPU_VCPUS" zen5 3 NVGPU_PIN=smt
check "io=siblings with smt" "refused: NVGPU_PIN=smt:io=siblings: io=siblings: smt leaves no thread" zen5 4 NVGPU_PIN=smt:io=siblings
check "io=other with spread" "refused: NVGPU_PIN=spread:io=other: io=other: the vCPUs take every L3 domain" zen5 4 NVGPU_PIN=spread:io=other
check "an unknown layout" "refused: NVGPU_PIN=fast: cores, smt, spread, l3 or core-sets" zen5 4 NVGPU_PIN=fast
check "an unknown option" "refused: NVGPU_PIN: fast=1:" zen5 4 NVGPU_PIN=cores:fast=1
check "a bad list" "refused: NVGPU_PIN:avoid=0-x: CPUs" zen5 4 NVGPU_PIN=cores:avoid=0-x
# A piece given by name wins over the layout's.
check "NVGPU_IO_AFFINITY wins" "io=2-3 be=2-3" zen5 8 NVGPU_PIN=cores:io=other NVGPU_IO_AFFINITY=2-3
check "NVGPU_BACKEND_CPUS wins" "io=0-7,16-23 be=5" zen5 8 NVGPU_PIN=cores:io=other NVGPU_BACKEND_CPUS=5
check "NVGPU_GUEST_SMT wins" "smt=2" zen5 2 NVGPU_PIN=cores NVGPU_VCPU_PINS=8,24 NVGPU_GUEST_SMT=2
# The pieces by name alone.
check "pins, one CPU each" "pins=[8 9 10 11] aff= io= be= smt= cs= said=yes" zen5 4 NVGPU_VCPU_PINS=8,9,10,11
check "pins, a list each" "pins=[8,24 9,25]" zen5 2 NVGPU_VCPU_PINS=8,24:9,25
check "pins, one per vCPU" "refused: NVGPU_VCPU_PINS=8,9: one entry per vCPU (4), not 2" zen5 4 NVGPU_VCPU_PINS=8,9
check "pins, one CPU twice" "refused: NVGPU_VCPU_PINS: one CPU twice" zen5 2 NVGPU_VCPU_PINS=8,8
check "the guest told siblings it does not have" \
    "refused: NVGPU_GUEST_SMT=2: vCPUs 0 and 1 are pinned to 8 and 9, which are not one core's threads" \
    zen5 2 NVGPU_VCPU_PINS=8,9 NVGPU_GUEST_SMT=2
check "siblings need pins" "refused: NVGPU_GUEST_SMT=2 needs one CPU per vCPU" zen5 2 NVGPU_GUEST_SMT=2
check "siblings need one CPU each" "refused: NVGPU_GUEST_SMT=2 needs one CPU per vCPU" zen5 2 NVGPU_VCPU_PINS=8,24:9,25 NVGPU_GUEST_SMT=2
check "NVGPU_GUEST_SMT is 1 or 2" "refused: NVGPU_GUEST_SMT=3: 1 or 2" zen5 2 NVGPU_GUEST_SMT=3
check "a shared set" "pins=[] aff=8-15 io= be= smt= cs= said=yes" zen5 4 NVGPU_CPU_AFFINITY=8-15
# A host with no SMT, no L3 and no ranking: the higher-numbered cores first.
check "flat: cores" "pins=[4 5 6 7] aff= io= be= smt=1" flat 4 NVGPU_PIN=cores
check "flat: smt has no second thread" "refused: NVGPU_PIN=smt: 2 whole cores wanted, this host has 0" flat 4 NVGPU_PIN=smt
check "flat: spread needs two L3 domains" "refused: NVGPU_PIN=spread: this host has one L3 domain" flat 4 NVGPU_PIN=spread
check "flat: io=siblings with no siblings" "refused: NVGPU_PIN=cores:io=siblings: io=siblings: these cores have no second thread" \
    flat 4 NVGPU_PIN=cores:io=siblings
# Another sysfs tree is for the tests, never for a root run.
check "NVGPU_SYSFS_CPU as root" "refused: NVGPU_SYSFS_CPU: for the tests, unprivileged only" zen5 4 NVGPU_PIN=cores PRIV=root

# The core-scheduling check (tuning.sh settles CORE_SCHED): sibling vCPUs
# under a cookie each are refused, and nothing else is.
cs() { # cs GUEST_SMT CORE_SCHED
    (
        die() {
            echo "refused: $*"
            exit 1
        }
        # shellcheck source=../launcher/placement.sh
        . "$LIB/placement.sh"
        # shellcheck disable=SC2034 # placement.sh's to read
        GUEST_SMT=$1 CORE_SCHED=$2
        placement_core_sched_check
        echo passed
    )
}
for c in "2 per-vcpu refused" "2 vm passed" "2 off passed" "2 shared passed" "2 '' passed" "1 per-vcpu passed" "'' per-vcpu passed"; do
    eval "set -- $c"
    got=$(cs "$1" "$2")
    if [[ $got == "$3"* ]]; then echo "ok   core sched: smt=$1 sched=$2 $3"; else
        echo "FAIL core sched: smt=$1 sched=$2: wanted $3, got $got"
        FAIL=1
    fi
done

# rig/pin-layout.sh says the same layout in each place's words.
out=$(NVGPU_SYSFS_CPU=$Z "$HERE/../pin-layout.sh" 8 smt 2>&1)
for w in "--cpu-affinity 0=10:1=26:2=13:3=29:4=14:5=30:6=15:7=31" '"vcpu_pins": [10, 26, 13, 29, 14, 30, 15, 31],' \
    '"threads_per_core": 2,' "NVGPU_GUEST_SMT=2" "--per-vm-core-scheduling"; do
    if grep -qF -- "$w" <<<"$out"; then echo "ok   pin-layout.sh: $w"; else
        echo "FAIL pin-layout.sh: no \"$w\" in:"
        echo "$out"
        FAIL=1
    fi
done
out=$(NVGPU_SYSFS_CPU=$Z "$HERE/../pin-layout.sh" 2 core-sets:io=other 2>&1)
for w in '"vcpu_pins": [[14, 30], [15, 31]],' "--cpu-affinity 0=14,30:1=15,31" "--no-smt" \
    "vhost-user-nvgpu@N.service: CPUAffinity=0-7,16-23"; do
    if grep -qF -- "$w" <<<"$out"; then echo "ok   pin-layout.sh: $w"; else
        echo "FAIL pin-layout.sh: no \"$w\" in:"
        echo "$out"
        FAIL=1
    fi
done

[ "$FAIL" = 0 ] && echo "placement: all passed"
exit "$FAIL"
