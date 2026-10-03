#!/usr/bin/env bash
# Build and test on remote machines from a workstation that builds nothing.
#
#   rig.sh check             copy the tree to the build host only; fmt,
#                            clippy, tests and the musl build, nothing moved
#   rig.sh fmt               cargo fmt on the build host (its rustfmt is the
#                            one CI-style checks use), .rs files copied back
#   rig.sh sync              copy the tree to the build host and the GPU host
#   rig.sh build             cargo test + static release build on the build
#                            host, binaries copied to the GPU host
#   rig.sh module            build the guest module where a tree for the
#                            guest kernel lives, put it on the GPU host
#   rig.sh stage             put the module and probes into the test rootfs
#   rig.sh probe P [P...]    boot one guest per probe, print what it said
#   rig.sh nesbox            test and build the VMM at NESBOX_SRC on the build
#                            host (static, forwarding only, no renderer) and
#                            copy it to the GPU host as nesbox-$TAG
#   rig.sh caps [SET...]     boot rig-probe-caps.sh once per capability set
#                            (default: the four sets below), each checked
#                            against the nodes the guest should have
#   rig.sh hostbench         rmbench on the GPU host itself, as the backend's
#                            user, for the bare-metal side of the ratio
#   rig.sh all P [P...]      sync, build, module, stage, probe
#
# RIG_TARGET picks scripts/rig/hosts-$RIG_TARGET.env (default box1). Those
# files are not in git; hosts.env.example lists every variable. The
# workstation only relays: source goes out with rsync, binaries move host to
# host with `scp -3`.
#
# Assumptions:
#   - BUILD_HOST has rustc, cargo and the musl target. Binaries are static
#     musl, so one build runs on any GPU host whatever its glibc. Its login
#     shell may not be bash, so every remote command runs under `bash -s`.
#   - The module is built where a kernel tree for the guest kernel lives:
#     MODULE_ON=gpu on GPU_HOST in GPU_KDIR, MODULE_ON=build on BUILD_HOST in
#     BUILD_KDIR. The vermagic is printed so a mismatch shows before a boot.
#   - box-stage.sh and box-run.sh do the GPU host's side; see their headers.
#   - Each remote step is one round trip, retried up to RIG_TRIES times when
#     ssh itself fails (exit 255). Any other exit is the step's answer.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
TOP=$(cd "$HERE/../.." && pwd)
RIG_TARGET=${RIG_TARGET:-box1}
ENVF="$HERE/hosts-$RIG_TARGET.env"
[ -f "$ENVF" ] || { echo "rig: no $ENVF (see hosts.env.example)" >&2; exit 2; }
# shellcheck source=/dev/null
. "$ENVF"

: "${BUILD_HOST:?}" "${GPU_HOST:?}" "${GPU_DIR:?}" "${GPU_BIN:?}"
BUILD_DIR=${BUILD_DIR:-nvgpu-rig}
MODULE_ON=${MODULE_ON:-gpu}
TAG=${TAG:-v02}
RIG_TRIES=${RIG_TRIES:-3}
MUSL=x86_64-unknown-linux-musl
SSH=(ssh -o ConnectTimeout=15 -o BatchMode=yes)

log() { printf 'rig: %s\n' "$*" >&2; }

retry() {
    local i rc
    for i in $(seq 1 "$RIG_TRIES"); do
        rc=0
        "$@" || rc=$?
        [ "$rc" -ne 255 ] && return "$rc"
        log "connection failed (try $i of $RIG_TRIES): ${*:1:3}"
        sleep 5
    done
    log "gave up after $RIG_TRIES tries"
    return 255
}

# remote HOST SCRIPT: run a bash script there.
# A non-interactive shell does not read the profile that puts rustup's cargo
# on PATH, so it is added here for every host.
remote() { retry "${SSH[@]}" "$1" bash -s <<<"export PATH=\$HOME/.cargo/bin:\$PATH
$2"; }

# The variables box-side scripts read, as one line of shell assignments.
gpu_env() {
    local v out=""
    for v in GPU_DIR GPU_BIN GPU_ROOT GPU_USER GPU_ROOTFS GPU_ROOTFS_BASE \
             GPU_KERNEL GPU_VMM GPU_SHARE GPU_LOGS GPU_TIMEOUT TAG; do
        [ -n "${!v:-}" ] && out+="$v=$(printf %q "${!v}") "
    done
    printf '%s' "$out"
}

push() {
    local i
    for i in $(seq 1 "$RIG_TRIES"); do
        rsync -az --delete \
            --exclude target/ --exclude .git/ --exclude /out/ --exclude "/out-*.log" \
            --exclude PLAN.md --exclude NOTES.md --exclude notes/ \
            --exclude 'scripts/rig/hosts-*.env' \
            --exclude '*.ko' --exclude '*.o' --exclude '*.mod*' --exclude '.*.cmd' \
            --exclude Module.symvers --exclude modules.order \
            -e "${SSH[*]}" "$TOP/" "$1:$2/" && return 0
        log "$1: rsync failed (try $i of $RIG_TRIES)"
        sleep 5
    done
    return 255
}

do_sync() {
    local rev dirty=""
    rev=$(git -C "$TOP" rev-parse --short HEAD)
    git -C "$TOP" diff --quiet HEAD -- . ':!scripts/rig/.stamp' || dirty="+dirty"
    echo "$rev$dirty" > "$HERE/.stamp"
    log "sync $rev$dirty -> $BUILD_HOST:$BUILD_DIR and $GPU_HOST:$GPU_DIR"
    push "$BUILD_HOST" "$BUILD_DIR"
    push "$GPU_HOST" "$GPU_DIR"
}

do_check() {
    log "check on $BUILD_HOST"
    push "$BUILD_HOST" "$BUILD_DIR"
    remote "$BUILD_HOST" "
        set -uo pipefail
        ulimit -n 65536 2>/dev/null || true
        cd ~/$BUILD_DIR
        rc=0
        cargo fmt --all --check > out-fmt.log 2>&1 || { echo '== fmt'; head -n 40 out-fmt.log; rc=1; }
        cargo clippy --workspace --all-targets --features device/vhost-user --quiet > out-clippy.log 2>&1 \\
            || { echo '== clippy'; grep -E '^(error|warning)' -A6 out-clippy.log | head -n 60; rc=1; }
        echo \"clippy: \$(grep -cE '^warning' out-clippy.log) warning(s), see ~/$BUILD_DIR/out-clippy.log\"
        # A crate whose test binary dies on a signal prints no 'test result'
        # line at all, so the summary below silently shrinks rather than going
        # red. Carry cargo's own verdict into it.
        cargo test --workspace --quiet > out-test.log 2>&1 && trc=0 || trc=1
        [ \$trc -eq 0 ] || { echo '== test'; grep -E 'FAILED|panicked|^error|signal:' -A8 out-test.log | head -n 60; rc=1; }
        grep -E '^test result' out-test.log | awk -v trc=\$trc '{p+=\$4; f+=\$6} END {print \"tests: \" p \" passed, \" f \" failed\" (trc ? \" -- AND CARGO REPORTED FAILURE: a crate may have crashed before reporting\" : \"\")}'
        cargo build --release --target $MUSL --features vhost-user -p device --bins --quiet > out-build.log 2>&1 \\
            || { echo '== musl build'; grep -E '^error' -A12 out-build.log | head -n 60; rc=1; }
        exit \$rc
    "
}

do_fmt() {
    push "$BUILD_HOST" "$BUILD_DIR"
    remote "$BUILD_HOST" "cd ~/$BUILD_DIR && cargo fmt --all"
    rsync -az --include='*/' --include='*.rs' --exclude='*' --exclude target/ \
        -e "${SSH[*]}" "$BUILD_HOST:$BUILD_DIR/" "$TOP/"
    git -C "$TOP" status --short -- '*.rs'
}

do_nesbox() {
    # glibc, not musl: nesbox uses statx and ioctl constants musl's bindings
    # lack. So it is built where its glibc is no newer than the GPU host's:
    # NESBOX_ON=build on BUILD_HOST, NESBOX_ON=gpu on GPU_HOST.
    local src=${NESBOX_SRC:?set NESBOX_SRC to a nesbox checkout} host dir i
    if [ "${NESBOX_ON:-build}" = gpu ]; then
        host=$GPU_HOST dir="$GPU_DIR-nesbox"
    else
        host=$BUILD_HOST dir="\$HOME/nesbox-rig"
    fi
    log "nesbox from $src on $host"
    for i in $(seq 1 "$RIG_TRIES"); do
        # Tracked files only: a checkout collects untracked images and
        # recordings (8.4 GB of them once), and the workstation's link is the
        # slow one.
        git -C "$src" ls-files -z | rsync -az --from0 --files-from=- \
            -e "${SSH[*]}" "$src/" "$host:${dir#\$HOME/}/" && break
        [ "$i" = "$RIG_TRIES" ] && return 255
        sleep 5
    done
    remote "$host" "
        set -euo pipefail
        cd $dir
        rc=0; cargo test -p virtio-devices --no-default-features --quiet nvgpu > out-test.log 2>&1 || rc=\$?
        grep -E '^test result|FAILED|panicked|^error' out-test.log || true
        [ \$rc = 0 ] || { tail -n 40 out-test.log; exit 1; }
        cargo build --release --no-default-features -p nesbox-vmm --bin nesbox --quiet > out-build.log 2>&1 \\
            || { grep -E '^error' -A12 out-build.log | head -n 60; exit 1; }
        strip -o nesbox target/release/nesbox
        ls -l nesbox
    "
    if [ "${NESBOX_ON:-build}" = gpu ]; then
        remote "$GPU_HOST" "install -m 755 $dir/nesbox $GPU_BIN/nesbox-$TAG"
    else
        retry scp -3 -q "$BUILD_HOST:nesbox-rig/nesbox" "$GPU_HOST:$GPU_BIN/nesbox-$TAG"
    fi
    log "nesbox -> $GPU_HOST:$GPU_BIN/nesbox-$TAG"
}

do_build() {
    log "build on $BUILD_HOST"
    remote "$BUILD_HOST" "
        set -euo pipefail
        ulimit -n 65536 2>/dev/null || true
        cd ~/$BUILD_DIR
        rc=0; cargo test --workspace --quiet > out-test.log 2>&1 || rc=\$?
        grep -E '^test result|FAILED|panicked|^error' out-test.log || true
        [ \$rc = 0 ] || { echo \"cargo test failed (\$rc); see ~/$BUILD_DIR/out-test.log\" >&2; exit 1; }
        cargo build --release --target $MUSL --features vhost-user -p device --bins --quiet > out-build.log 2>&1 \\
            || { grep -E '^error' -A12 out-build.log | head -n 60; exit 1; }
        mkdir -p out
        for b in vhost-user-nvgpu nvgpu-userspace; do
            cp target/$MUSL/release/\$b out/\$b
            strip out/\$b
        done
        # The guest's Vulkan layer is loaded by the guest's glibc loader, so
        # it is built for the host's own target, not musl.
        cargo build --release -p nvgpu-vklayer --quiet >> out-build.log 2>&1 \\
            || { grep -E '^error' -A12 out-build.log | head -n 60; exit 1; }
        cp target/release/libVkLayer_nvgpu.so out/
        strip out/libVkLayer_nvgpu.so
        ls -l out | tail -n +2
    "
    remote "$GPU_HOST" "mkdir -p '$GPU_BIN'"
    local b
    for b in vhost-user-nvgpu nvgpu-userspace; do
        retry scp -3 -q "$BUILD_HOST:$BUILD_DIR/out/$b" "$GPU_HOST:$GPU_BIN/$b-$TAG"
        log "$b -> $GPU_HOST:$GPU_BIN/$b-$TAG"
    done
    # Into the synced tree's guest directory, which box-stage copies whole.
    retry scp -3 -q "$BUILD_HOST:$BUILD_DIR/out/libVkLayer_nvgpu.so" \
        "$GPU_HOST:$GPU_DIR/scripts/rig/guest/libVkLayer_nvgpu.so"
    log "libVkLayer_nvgpu.so -> $GPU_HOST:$GPU_DIR/scripts/rig/guest"
}

do_module() {
    local host kdir dir
    if [ "$MODULE_ON" = build ]; then
        host=$BUILD_HOST kdir=${BUILD_KDIR:?set BUILD_KDIR} dir="\$HOME/$BUILD_DIR"
    else
        host=$GPU_HOST kdir=${GPU_KDIR:?set GPU_KDIR} dir=$GPU_DIR
    fi
    log "guest module on $host against $kdir"
    remote "$host" "
        set -euo pipefail
        [ -f '$kdir/Module.symvers' ] || { echo 'no built kernel tree at $kdir' >&2; exit 3; }
        cd \"$dir/driver\"
        rm -f virtio_gpu_nv.ko
        make -C '$kdir' M=\"\$PWD\" CONFIG_VIRTIO_GPU_NV=m modules > build.log 2>&1 || { tail -n 30 build.log; exit 1; }
        grep -E 'warning:|error:' build.log || true
        echo \"vermagic: \$(modinfo -F vermagic virtio_gpu_nv.ko)\"
    "
    if [ "$MODULE_ON" = build ]; then
        retry scp -3 -q "$BUILD_HOST:$BUILD_DIR/driver/virtio_gpu_nv.ko" "$GPU_HOST:$GPU_DIR/driver/virtio_gpu_nv.ko"
    fi
}

do_stage() {
    log "stage into $GPU_HOST:${GPU_ROOTFS:?}"
    remote "$GPU_HOST" "$(gpu_env) bash $GPU_DIR/scripts/rig/box-stage.sh"
}

do_probe() {
    # Each guest runs detached on the GPU host and writes its own summary, and
    # this side polls for it. A dropped connection then costs a retry of the
    # poll, not the run: an ssh that dies mid-boot used to take the probe's
    # output with it, and retrying meant booting the guest again.
    [ $# -gt 0 ] || { log "probe: name at least one probe"; exit 2; }
    local p rc=0 tag sum out i
    for p in "$@"; do
        tag="$TAG-${p%.sh}${TAG_SUFFIX:-}"
        sum="${GPU_LOGS:?}/$tag.summary"
        log "probe $p on $GPU_HOST"
        remote "$GPU_HOST" "
            mkdir -p '$GPU_LOGS'; rm -f '$sum'
            $(gpu_env) BACKEND_ARGS=$(printf %q "${BACKEND_ARGS:-}") GUEST_ARGS=$(printf %q "${GUEST_ARGS:-}") \
            BACKEND_WRAP=$(printf %q "${BACKEND_WRAP:-}") \
                setsid nohup bash -c 'bash $GPU_DIR/scripts/rig/box-run.sh $p $tag; echo \"rig-done rc=\$?\"' \
                > '$sum' 2>&1 < /dev/null &
        " || { rc=$?; continue; }
        out=""
        for i in $(seq 1 360); do
            sleep 5
            out=$(remote "$GPU_HOST" "grep -q '^rig-done' '$sum' 2>/dev/null && cat '$sum'" 2>/dev/null) && break
            out=""
        done
        if [ -z "$out" ]; then
            log "probe $p: no result after 30 minutes; see $GPU_HOST:$sum"
            rc=1
            continue
        fi
        printf '%s\n' "$out" | grep -v '^rig-done'
        printf '%s\n' "$out" | grep -q '^rig-done rc=0' || rc=1
    done
    return $rc
}

do_caps() {
    local sets=("$@") set rc=0
    [ ${#sets[@]} -gt 0 ] || sets=(graphics,video,utility graphics,compute,video,utility compute video)
    for set in "${sets[@]}"; do
        BACKEND_ARGS="--caps $set" GUEST_ARGS="nvgpu_expect=$set" TAG_SUFFIX="-$set" \
            do_probe rig-probe-caps.sh || rc=$?
    done
    return $rc
}

do_hostbench() {
    remote "$GPU_HOST" "
        set -euo pipefail
        b=$GPU_DIR/scripts/rig/guest/rmbench
        [ -x \$b ] || cc -O2 -static -o \$b \$b.c
        cp \$b /tmp/rmbench-host && chmod 755 /tmp/rmbench-host
        as=()
        [ ${GPU_ROOT:-1} = 1 ] && as=(setpriv --reuid=${GPU_USER:-nvgpu-test} --regid=${GPU_USER:-nvgpu-test} --init-groups)
        for i in 1 2 3; do \"\${as[@]}\" /tmp/rmbench-host 100000; done
    "
}

cmd=${1:-}
shift || true
case "$cmd" in
check) do_check ;;
nesbox) do_nesbox ;;
fmt) do_fmt ;;
sync) do_sync ;;
build) do_build ;;
module) do_module ;;
stage) do_stage ;;
probe) do_probe "$@" ;;
caps) do_caps "$@" ;;
hostbench) do_hostbench ;;
all) do_sync; do_build; do_module; do_stage; do_probe "$@" ;;
*) sed -n '2,12p' "$0" >&2; exit 2 ;;
esac
