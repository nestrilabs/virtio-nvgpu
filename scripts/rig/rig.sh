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
remote() { retry "${SSH[@]}" "$1" bash -s <<<"$2"; }

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
            --exclude target/ --exclude .git/ --exclude /out/ --exclude /out-test.log \
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
        cargo test --workspace --quiet > out-test.log 2>&1 \\
            || { echo '== test'; grep -E 'FAILED|panicked|^error' -A8 out-test.log | head -n 60; rc=1; }
        grep -E '^test result' out-test.log | awk '{p+=\$4; f+=\$6} END {print \"tests: \" p \" passed, \" f \" failed\"}'
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

do_build() {
    log "build on $BUILD_HOST"
    remote "$BUILD_HOST" "
        set -euo pipefail
        ulimit -n 65536 2>/dev/null || true
        cd ~/$BUILD_DIR
        rc=0; cargo test --workspace --quiet > out-test.log 2>&1 || rc=\$?
        grep -E '^test result|FAILED|panicked|^error' out-test.log || true
        [ \$rc = 0 ] || { echo \"cargo test failed (\$rc); see ~/$BUILD_DIR/out-test.log\" >&2; exit 1; }
        cargo build --release --target $MUSL --features vhost-user -p device --bins --quiet
        mkdir -p out
        for b in vhost-user-nvgpu nvgpu-userspace; do
            cp target/$MUSL/release/\$b out/\$b
            strip out/\$b
        done
        ls -l out | tail -n +2
    "
    remote "$GPU_HOST" "mkdir -p '$GPU_BIN'"
    local b
    for b in vhost-user-nvgpu nvgpu-userspace; do
        retry scp -3 -q "$BUILD_HOST:$BUILD_DIR/out/$b" "$GPU_HOST:$GPU_BIN/$b-$TAG"
        log "$b -> $GPU_HOST:$GPU_BIN/$b-$TAG"
    done
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
    [ $# -gt 0 ] || { log "probe: name at least one probe"; exit 2; }
    local p rc=0
    for p in "$@"; do
        log "probe $p on $GPU_HOST"
        remote "$GPU_HOST" "$(gpu_env) BACKEND_ARGS=$(printf %q "${BACKEND_ARGS:-}") \
            bash $GPU_DIR/scripts/rig/box-run.sh $p $TAG-${p%.sh}" || rc=$?
    done
    return $rc
}

cmd=${1:-}
shift || true
case "$cmd" in
check) do_check ;;
fmt) do_fmt ;;
sync) do_sync ;;
build) do_build ;;
module) do_module ;;
stage) do_stage ;;
probe) do_probe "$@" ;;
all) do_sync; do_build; do_module; do_stage; do_probe "$@" ;;
*) sed -n '2,12p' "$0" >&2; exit 2 ;;
esac
