# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's placement (the header's "Placement"): which host CPUs
# the vCPUs, the VMM's other threads and the backend run on, and what CPU
# topology the guest is told. Unset, nothing is placed and the guest is
# told what its VMM tells it by default, as before this piece existed.
#
# The pieces the header lists -- NVGPU_CPU_AFFINITY, NVGPU_VCPU_PINS,
# NVGPU_IO_AFFINITY, NVGPU_BACKEND_CPUS, NVGPU_GUEST_SMT -- say it CPU by
# CPU. NVGPU_PIN names a layout instead, worked out from this host's
# topology in sysfs (NVGPU_SYSFS_CPU, for the tests, reads another tree):
# each SMT core (topology/thread_siblings_list), each L3 domain
# (cache/index3/shared_cpu_list: a CCD on a Ryzen), and which cores the host
# scheduler prefers (cpufreq/amd_pstate_prefcore_ranking, or
# acpi_cppc/highest_perf): those are where the desktop's busiest threads
# go, so a layout takes the least preferred first. A piece given by name
# as well wins over what the layout says for it. Sourced by the launcher.

# expand_cpus LIST: "0-2,8" as "0 1 2 8" (LIST already checked by cpu_list).
expand_cpus() {
    local part a b i out=()
    local IFS=,
    for part in $1; do
        a=${part%-*} b=${part#*-}
        for ((i = 10#$a; i <= 10#$b; i++)); do out+=("$i"); done
    done
    echo "${out[@]}"
}

# compact_cpus "8 9 10 12": the same as "8-10,12", for the terminal and for
# the VMM's command line.
compact_cpus() {
    local c prev= start= out=()
    for c in $(tr ' ' '\n' <<<"$1" | sort -n | uniq); do
        if [ -n "$prev" ] && [ "$c" = $((prev + 1)) ]; then
            prev=$c
            continue
        fi
        [ -z "$start" ] || out+=("$([ "$start" = "$prev" ] && echo "$start" || echo "$start-$prev")")
        start=$c prev=$c
    done
    [ -z "$start" ] || out+=("$([ "$start" = "$prev" ] && echo "$start" || echo "$start-$prev")")
    local IFS=,
    echo "${out[*]}"
}

# topo_read: this host's online CPUs (TOPO_CPUS), each one's core (CORE_OF,
# the core's first CPU), L3 domain (L3_OF, the domain's first CPU) and the
# host scheduler's preference for it (RANK_OF, higher is preferred).
topo_read() {
    local sys=${NVGPU_SYSFS_CPU:-/sys/devices/system/cpu} c d f online
    [ -z "${NVGPU_SYSFS_CPU:-}" ] || [ $PRIV = user ] ||
        die "NVGPU_SYSFS_CPU: for the tests, unprivileged only"
    online=$(cat "$sys/online" 2>/dev/null) || die "NVGPU_PIN: cannot read $sys/online"
    cpu_list "$sys/online" "$online" >/dev/null
    TOPO_CPUS=$(expand_cpus "$online")
    declare -gA CORE_OF=() L3_OF=() RANK_OF=() THREADS_OF=()
    for c in $TOPO_CPUS; do
        d=$sys/cpu$c
        f=$(cat "$d/topology/thread_siblings_list" 2>/dev/null || echo "$c")
        cpu_list "$d/topology/thread_siblings_list" "$f" >/dev/null
        f=$(expand_cpus "$f")
        CORE_OF[$c]=${f%% *}
        THREADS_OF[${f%% *}]=$f
        if [ "$(cat "$d/cache/index3/level" 2>/dev/null)" = 3 ]; then
            f=$(cat "$d/cache/index3/shared_cpu_list")
            cpu_list "$d/cache/index3/shared_cpu_list" "$f" >/dev/null
            f=$(expand_cpus "$f")
            L3_OF[$c]=${f%% *}
        else
            L3_OF[$c]=0
        fi
        f=$(cat "$d/cpufreq/amd_pstate_prefcore_ranking" 2>/dev/null ||
            cat "$d/acpi_cppc/highest_perf" 2>/dev/null || echo 0)
        [[ $f =~ ^[0-9]{1,9}$ ]] || f=0
        RANK_OF[$c]=$((10#$f))
    done
}

# The usable cores, in the order a layout takes them: L3 domains the host
# prefers least first (by their cores' mean rank; the domain of CPU 0 last
# among equals, then the higher-numbered), and within a domain the least
# preferred cores first, the higher-numbered among equals. A core any of
# whose CPUs is in AVOID is left out. Prints "domain core" lines.
topo_cores() {
    local first_l3=$1 c core l3 r
    declare -A seen=() sum=() n=()
    local lines=()
    for c in $TOPO_CPUS; do
        core=${CORE_OF[$c]}
        [ -z "${seen[$core]:-}" ] || continue
        seen[$core]=1
        local t skip=0 rank=0
        for t in ${THREADS_OF[$core]}; do
            [ -z "${AVOID[$t]:-}" ] || skip=1
            [ "${RANK_OF[$t]:-0}" -le "$rank" ] || rank=${RANK_OF[$t]}
        done
        [ "$skip" = 0 ] || continue
        l3=${L3_OF[$c]}
        sum[$l3]=$((${sum[$l3]:-0} + rank))
        n[$l3]=$((${n[$l3]:-0} + 1))
        lines+=("$l3 $core $rank")
    done
    [ ${#lines[@]} -gt 0 ] || return 0
    # Domain key: explicit first, then mean rank, then CPU 0's last, then
    # the higher-numbered.
    local line key
    for line in "${lines[@]}"; do
        read -r l3 core r <<<"$line"
        if [ -n "$first_l3" ] && [ "$l3" = "$first_l3" ]; then key=0; else key=1; fi
        printf '%d %012d %d %06d %09d %06d %s %s\n' "$key" $((sum[$l3] * 1000 / n[$l3])) \
            "$([ "$l3" = "${L3_OF[0]:-}" ] && echo 1 || echo 0)" $((999999 - l3)) "$r" $((999999 - core)) "$l3" "$core"
    done | sort | awk '{ print $7, $8 }'
}

# The NVGPU_PIN value: a layout and its options, checked.
#   cores      one thread of each of NVGPU_VCPUS whole cores, in one L3
#              domain where they fit; the other threads of those cores are
#              left to nothing but what the host schedules there (io=
#              siblings gives them the VMM's other threads and the backend);
#              the guest is told one thread per core
#   smt        NVGPU_VCPUS/2 whole cores, both threads of each: vCPUs 2k and
#              2k+1 on one core's two threads, and the guest told so
#   spread     as cores, one core from each L3 domain in turn
#   l3         no pin per vCPU: every vCPU may run on any CPU of the first
#              L3 domain (and the next, if the vCPUs outnumber it)
#   core-sets  vCPU i on either thread of core i (a set per vCPU); the
#              guest is told one thread per core
# Options, after a colon each (cores:io=siblings:avoid=0-1,16-17):
#   l3=CPU     take the L3 domain of that CPU first
#   avoid=LIST CPUs no vCPU runs on, and every core with one of them in it
#              (default 0: CPU 0's core, where the host's housekeeping
#              runs); avoid=none uses every core
#   io=WHERE   the VMM's other threads and the backend: none (default:
#              where the host puts them), siblings (the idle threads of the
#              vCPUs' cores; cores and spread only), rest (the whole cores
#              of the vCPUs' L3 domains no vCPU has: smt at 8 vCPUs leaves
#              4 of a 9950X CCD's 8), other (every CPU outside the vCPUs'
#              L3 domains), or a CPU list
pin_layout() {
    local spec=$1 layout opt first_l3= io=none avoid=0 want
    layout=${spec%%:*}
    case $layout in cores | smt | spread | l3 | core-sets) ;; *)
        die "NVGPU_PIN=$spec: cores, smt, spread, l3 or core-sets, then options (:l3=CPU :avoid=LIST :io=WHERE)" ;;
    esac
    local opts=()
    IFS=: read -r -a opts <<<"${spec#"$layout"}"
    for opt in ${opts[@]+"${opts[@]}"}; do
        case $opt in
            '') ;;
            l3=*) first_l3=${opt#l3=}; [[ $first_l3 =~ ^[0-9]+$ ]] || die "NVGPU_PIN: $opt: a CPU" ;;
            avoid=*) avoid=${opt#avoid=} ;;
            io=*) io=${opt#io=} ;;
            *) die "NVGPU_PIN: $opt: l3=CPU, avoid=LIST or io=none|siblings|rest|other|LIST" ;;
        esac
    done
    topo_read
    declare -gA AVOID=()
    if [ "$avoid" != none ]; then
        local c
        # cpu_list's refusal ends only the substitution it runs in: said
        # there, and ended here.
        c=$(cpu_list NVGPU_PIN:avoid "$avoid") || exit 1
        for c in $(expand_cpus "${c// /}"); do AVOID[$c]=1; done
    fi
    if [ -n "$first_l3" ]; then
        [ -n "${L3_OF[$first_l3]:-}" ] || die "NVGPU_PIN: l3=$first_l3: no such online CPU"
        first_l3=${L3_OF[$first_l3]}
    fi
    local order=() line l3 core
    while read -r l3 core; do [ -n "$core" ] && order+=("$l3 $core"); done < <(topo_cores "$first_l3")
    local picked=() doms=()
    case $layout in
        cores | core-sets) want=$VCPUS ;;
        smt)
            [ $((VCPUS % 2)) = 0 ] || die "NVGPU_PIN=smt: an even NVGPU_VCPUS (two threads a core), not $VCPUS"
            want=$((VCPUS / 2))
            ;;
        spread) want=$VCPUS ;;
        l3) want=0 ;;
    esac
    if [ "$layout" = spread ]; then
        # One core from each domain in turn, in the domains' order.
        local -A dq=()
        local dlist=()
        for line in "${order[@]}"; do
            read -r l3 core <<<"$line"
            [ -n "${dq[$l3]+x}" ] || dlist+=("$l3")
            dq[$l3]+="$core "
        done
        [ ${#dlist[@]} -ge 2 ] || die "NVGPU_PIN=spread: this host has one L3 domain"
        while [ ${#picked[@]} -lt "$want" ]; do
            local took=0 d rest
            for d in "${dlist[@]}"; do
                [ ${#picked[@]} -lt "$want" ] || break
                read -r core rest <<<"${dq[$d]}"
                [ -n "$core" ] || continue
                dq[$d]=${rest:-}
                picked+=("$core")
                took=1
            done
            [ "$took" = 1 ] || break
        done
    elif [ "$want" -gt 0 ]; then
        for line in "${order[@]}"; do
            [ ${#picked[@]} -lt "$want" ] || break
            read -r l3 core <<<"$line"
            if [ "$layout" = smt ]; then
                local th
                read -r -a th <<<"${THREADS_OF[$core]}"
                [ ${#th[@]} -ge 2 ] || continue
            fi
            picked+=("$core")
        done
    fi
    [ ${#picked[@]} -ge "$want" ] ||
        die "NVGPU_PIN=$spec: $want whole cores wanted, this host has ${#picked[@]} besides those avoided" \
            "(avoid=none uses CPU 0's core; fewer NVGPU_VCPUS; or l3)"
    # The vCPUs' cores in CPU order, so vCPU i's is the i-th.
    local sorted=()
    [ ${#picked[@]} = 0 ] || mapfile -t sorted < <(printf '%s\n' "${picked[@]}" | sort -n)
    [ "$layout" = spread ] && sorted=("${picked[@]}")
    PIN_SETS=() PIN_SMT= PIN_SHARED=
    local th used=() idle=()
    for core in ${sorted[@]+"${sorted[@]}"}; do
        read -r -a th <<<"${THREADS_OF[$core]}"
        doms+=("${L3_OF[$core]}")
        case $layout in
            cores | spread)
                PIN_SETS+=("${th[0]}")
                used+=("${th[0]}")
                idle+=("${th[@]:1}")
                ;;
            smt)
                PIN_SETS+=("${th[0]}" "${th[1]}")
                used+=("${th[0]}" "${th[1]}")
                idle+=("${th[@]:2}")
                ;;
            core-sets)
                PIN_SETS+=("$(compact_cpus "${th[*]}")")
                used+=("${th[@]}")
                ;;
        esac
    done
    case $layout in cores | spread | core-sets) PIN_SMT=1 ;; smt) PIN_SMT=2 ;; esac
    if [ "$layout" = l3 ]; then
        # Whole domains, in order, until they hold a CPU a vCPU.
        local -A indom=()
        local set=() c
        for line in "${order[@]}"; do
            read -r l3 core <<<"$line"
            if [ -z "${indom[$l3]:-}" ]; then
                [ ${#set[@]} -lt "$VCPUS" ] || break
                indom[$l3]=1
                doms+=("$l3")
            fi
            read -r -a th <<<"${THREADS_OF[$core]}"
            set+=("${th[@]}")
        done
        [ ${#set[@]} -ge "$VCPUS" ] ||
            die "NVGPU_PIN=$spec: $VCPUS vCPUs, and ${#set[@]} CPUs besides those avoided"
        PIN_SHARED=$(compact_cpus "${set[*]}")
        used=("${set[@]}")
    fi
    case $io in
        none) PIN_IO= ;;
        siblings)
            case $layout in cores | spread) ;; *) die "NVGPU_PIN=$spec: io=siblings: $layout leaves no thread of its cores idle" ;; esac
            [ ${#idle[@]} -gt 0 ] || die "NVGPU_PIN=$spec: io=siblings: these cores have no second thread"
            PIN_IO=$(compact_cpus "${idle[*]}")
            ;;
        rest)
            # The cores of the vCPUs' L3 domains no vCPU has: whole cores
            # sharing the vCPUs' L3, none of them a vCPU's sibling.
            local -A taken=() indoms=()
            local rest=() d c
            for c in "${used[@]}"; do taken[${CORE_OF[$c]}]=1; done
            for d in "${doms[@]}"; do indoms[$d]=1; done
            for c in $TOPO_CPUS; do
                [ -n "${indoms[${L3_OF[$c]}]:-}" ] && [ -z "${taken[${CORE_OF[$c]}]:-}" ] && rest+=("$c")
            done
            [ ${#rest[@]} -gt 0 ] || die "NVGPU_PIN=$spec: io=rest: the vCPUs take every core of their L3 domains"
            PIN_IO=$(compact_cpus "${rest[*]}")
            ;;
        other)
            local -A mine=()
            local rest=() d c
            for d in "${doms[@]}"; do mine[$d]=1; done
            for c in $TOPO_CPUS; do [ -n "${mine[${L3_OF[$c]}]:-}" ] || rest+=("$c"); done
            [ ${#rest[@]} -gt 0 ] || die "NVGPU_PIN=$spec: io=other: the vCPUs take every L3 domain"
            PIN_IO=$(compact_cpus "${rest[*]}")
            ;;
        *)
            PIN_IO=$(cpu_list NVGPU_PIN:io "$io") || exit 1
            PIN_IO=${PIN_IO// /}
            ;;
    esac
}

# placement_settings: the pieces, from NVGPU_PIN and from their own
# variables (which win), checked; and PLACEMENT, a line for the terminal.
#   CPU_AFFINITY    one set for every vCPU ("8-15"), or empty
#   VCPU_SETS       a CPU list per vCPU, or none
#   IO_AFFINITY     the VMM's other threads ("0-7"), or empty
#   BACKEND_CPUS    the backend's threads, or empty
#   GUEST_SMT       1 or 2 threads per guest core, or empty (the VMM's
#                   default: nesbox one, crosvm two for an even count)
# and their _J forms, as the numbers nesbox's config takes.
placement_settings() {
    local pin=${NVGPU_PIN:-} i s
    PIN_SETS=() PIN_SMT= PIN_SHARED= PIN_IO=
    [ -z "$pin" ] || [ "$pin" = off ] || pin_layout "$pin"
    CPU_AFFINITY=${NVGPU_CPU_AFFINITY-$PIN_SHARED}
    IO_AFFINITY=${NVGPU_IO_AFFINITY-$PIN_IO}
    BACKEND_CPUS=${NVGPU_BACKEND_CPUS-$IO_AFFINITY}
    GUEST_SMT=${NVGPU_GUEST_SMT-$PIN_SMT}
    VCPU_SETS=()
    if [ -n "${NVGPU_VCPU_PINS+x}" ]; then
        # One CPU per vCPU (8,9,10,11), or a list per vCPU, colon-separated
        # (8,24:9,25:10,26:11,27).
        case $NVGPU_VCPU_PINS in
            *:*) IFS=: read -r -a VCPU_SETS <<<"$NVGPU_VCPU_PINS" ;;
            '') ;;
            *) IFS=, read -r -a VCPU_SETS <<<"$NVGPU_VCPU_PINS" ;;
        esac
    else
        VCPU_SETS=(${PIN_SETS[@]+"${PIN_SETS[@]}"})
    fi
    CPU_AFFINITY_J= IO_AFFINITY_J= VCPU_PINS_J= VCPU_SETS_J=()
    # (Each refusal said in its substitution, and the run ended here.)
    [ -z "$CPU_AFFINITY" ] || CPU_AFFINITY_J=$(cpu_list NVGPU_CPU_AFFINITY "$CPU_AFFINITY") || exit 1
    [ -z "$IO_AFFINITY" ] || IO_AFFINITY_J=$(cpu_list NVGPU_IO_AFFINITY "$IO_AFFINITY") || exit 1
    [ -z "$BACKEND_CPUS" ] || cpu_list NVGPU_BACKEND_CPUS "$BACKEND_CPUS" >/dev/null
    VCPU_SINGLE=1
    if [ ${#VCPU_SETS[@]} -gt 0 ]; then
        [ ${#VCPU_SETS[@]} = "$VCPUS" ] ||
            die "NVGPU_VCPU_PINS=${NVGPU_VCPU_PINS:-${PIN_SETS[*]}}: one entry per vCPU ($VCPUS), not ${#VCPU_SETS[@]}"
        for s in "${VCPU_SETS[@]}"; do
            [ -n "$s" ] || die "NVGPU_VCPU_PINS: an empty entry"
            s=$(cpu_list NVGPU_VCPU_PINS "$s") || exit 1
            VCPU_SETS_J+=("$s")
            case ${VCPU_SETS_J[-1]} in *,*) VCPU_SINGLE=0 ;; esac
        done
        if [ "$VCPU_SINGLE" = 1 ]; then
            VCPU_PINS_J=$(IFS=,; echo "${VCPU_SETS_J[*]}" | sed 's/,/, /g')
            [ "$(tr ',' '\n' <<<"$VCPU_PINS_J" | tr -d ' ' | sort -u | wc -l)" = "$VCPUS" ] ||
                die "NVGPU_VCPU_PINS: one CPU twice; two vCPUs would share it"
        fi
    fi
    case $GUEST_SMT in '' | 1 | 2) ;; *) die "NVGPU_GUEST_SMT=$GUEST_SMT: 1 or 2 threads per guest core" ;; esac
    if [ "$GUEST_SMT" = 2 ]; then
        # The guest is told vCPUs 2k and 2k+1 share a core: only a pin of
        # each to one CPU keeps that true, and those CPUs must be SMT
        # siblings (nesbox refuses it without pins; crosvm would say it
        # regardless).
        [ ${#VCPU_SETS[@]} -gt 0 ] && [ "$VCPU_SINGLE" = 1 ] ||
            die "NVGPU_GUEST_SMT=2 needs one CPU per vCPU (NVGPU_VCPU_PINS, or NVGPU_PIN=smt):" \
                "otherwise nothing keeps vCPUs 2k and 2k+1 on one core's two threads"
        [ $((VCPUS % 2)) = 0 ] || die "NVGPU_GUEST_SMT=2: an even NVGPU_VCPUS"
        local sys=${NVGPU_SYSFS_CPU:-/sys/devices/system/cpu} a b sib
        for ((i = 0; i + 1 < VCPUS; i += 2)); do
            a=${VCPU_SETS_J[i]} b=${VCPU_SETS_J[i + 1]}
            sib=$(cat "$sys/cpu$a/topology/thread_siblings_list" 2>/dev/null) || continue
            [[ " $(expand_cpus "$sib") " == *" $b "* ]] ||
                die "NVGPU_GUEST_SMT=2: vCPUs $i and $((i + 1)) are pinned to $a and $b, which are not" \
                    "one core's threads ($a's are $sib)"
        done
    fi
    # Two vCPUs told they are one core's threads, and pinned so, must be
    # able to run at once: one core-scheduling cookie for the whole VM
    # (launcher/tuning.sh's NVGPU_CORE_SCHED=vm), which still keeps every
    # host task and every other VM off a core while one of its vCPUs runs
    # (SECURITY.md, "vCPU placement"). A cookie per vCPU, crosvm's default,
    # would run the core one vCPU at a time.
    [ "$GUEST_SMT" != 2 ] || CORE_SCHED_DEFAULT=vm
    PLACEMENT=
    if [ -n "$pin$CPU_AFFINITY$IO_AFFINITY$BACKEND_CPUS$GUEST_SMT" ] || [ ${#VCPU_SETS[@]} -gt 0 ]; then
        local v=
        if [ ${#VCPU_SETS[@]} -gt 0 ]; then
            v="vCPUs on $(for s in "${VCPU_SETS_J[@]}"; do printf '%s ' "${s// /}"; done)"
            v=${v% }
        elif [ -n "$CPU_AFFINITY" ]; then
            v="vCPUs on any of $CPU_AFFINITY"
        else
            v="vCPUs anywhere"
        fi
        PLACEMENT="$v; VMM's other threads ${IO_AFFINITY:-anywhere}; backend ${BACKEND_CPUS:-anywhere}"
        if [ -n "$GUEST_SMT" ]; then
            PLACEMENT+="; guest told $GUEST_SMT thread(s) a core"
        else
            PLACEMENT+="; guest told the VMM's default topology"
        fi
        [ -z "$pin" ] || PLACEMENT+=" (NVGPU_PIN=$pin)"
    fi
}

# Once the core-scheduling mode is settled (launcher/tuning.sh's
# tuning_settings, which reads CORE_SCHED_DEFAULT above): sibling vCPUs
# cannot run under a cookie each. Where nothing has settled it (no
# CORE_SCHED), nothing is checked.
placement_core_sched_check() {
    [ "$GUEST_SMT" = 2 ] || return 0
    [ "${CORE_SCHED:-}" != per-vcpu ] ||
        die "NVGPU_GUEST_SMT=2 (NVGPU_PIN=smt) with a core-scheduling cookie per vCPU: the two" \
            "vCPUs of a core could never run at once; NVGPU_CORE_SCHED=vm, shared or off"
}
