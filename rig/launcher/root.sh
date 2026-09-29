# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# What only a root run of rig/run-guest.sh does. Sourced by the launcher,
# which calls each of these only as root.
#
# Run as root, each VM takes a slot of its own from a pool of host users, so
# that no two VMs are one host principal (SECURITY.md, "One uid per VM", says
# what that separates and what it does not). Slot N is two users:
#
#   for i in 0 1 2 3; do
#     useradd --system --no-create-home --shell /usr/sbin/nologin --user-group nvgpu-vm$i
#     useradd --system --no-create-home --shell /usr/sbin/nologin -g nvgpu-vm$i nvgpu-vmm$i
#   done
#
# (NixOS: users.users."nvgpu-vm0" = { isSystemUser = true; group = "nvgpu-vm0"; },
# users.groups."nvgpu-vm0" = {}, users.users."nvgpu-vmm0" = { isSystemUser =
# true; group = "nvgpu-vm0"; }, and so on.)
#
# nvgpu-vmN runs the slot's backend, with the groups of the device nodes it
# opens and nothing else -- setpriv hands them over, so the account needs no
# membership (/dev/nvidia* is 0666; render and card nodes are group render and
# video; /dev/udmabuf is group kvm) -- no capabilities and no_new_privs; the
# backend drops whatever it is given anyway. nvgpu-vmmN runs the slot's VMM
# under nesbox's jailer, in group nvgpu-vmN and no other: the backend's socket
# is made 0660 in that group, which is how the VMM reaches it and all it
# reaches of the backend's. A slot is free when no run holds its lock
# (/run/nvgpu-slots/N.lock) and neither of its users has a live process; the
# first free one is taken, and none free stops the run. Pools are counted from
# nvgpu-vm0 up to the first missing user, at most NVGPU_VM_SLOTS.
#
# Which user the backend is: slot N's nvgpu-vmN. With --wayland-socket it is
# the socket's owner instead -- the backend is then a client of that user's
# compositor like any of the user's own programs, and the socket's directory
# ($XDG_RUNTIME_DIR, 0700) lets no one else in anyway. With --wayland-export
# (and no --wayland-socket) it is the owner of the directory the export
# socket goes in, since the backend accepts only clients of its own uid
# (device/src/wl/export.rs). In those two modes every VM's backend is that
# one user, and what keeps them apart is the backend's sandbox (Landlock's
# signal and socket scoping, ABI 6 and 9) and its being undumpable; the VMM
# still takes a slot. NVGPU_USER overrides all of these, and makes every VM
# started with it one user, which is said. NVGPU_USER=root runs it as root,
# which it refuses unless NVGPU_ALLOW_ROOT_UNSAFE=1 and NVGPU_DIAGNOSTIC=1 as
# well: that passes --allow-root-unsafe, and every guest process is then an
# RM administrator. Only for ruling the credentials out while chasing
# something, never for a guest you do not trust. A host with no pool but the
# older single user "nvgpu" runs every backend as that user, with a warning
# each time.
#
# The VMM, run as root: under nesbox's jailer (tools/jailer in nesbox; built
# with `cargo build --release -p jailer`, and found beside the VMM binary or
# at NVGPU_JAILER), chrooted into a jail image built for the run -- nesbox
# itself, and virtiofsd with its libraries when there is a share -- with the
# config's kernel, disk, socket and share bound in, as the slot's nvgpu-vmmN.
# The jailer clears supplementary groups, so /dev/kvm must be 0666 (systemd's
# default). The disk must be writable by that user: a per-run copy is made
# for it (NVGPU_COPY_ROOTFS defaults to 1 here), and an image booted in place
# must already be. Without a jailer, a pool or a 0666 /dev/kvm the run is
# refused; NVGPU_VMM_JAIL=off runs the VMM as root, as before, and auto does
# the same, with a warning, where one of them is missing.
#
# The socket's directory, and the copy of the backend binary in it, are
# root's (0711) throughout, and never the backend user's: root binds the
# socket (systemd-socket-activate, which then execs the backend and hands it
# the socket as systemd's socket activation does) and opens it to the slot's
# group. The backend's user -- which with --wayland-socket, --wayland-export
# or NVGPU_USER has other live processes -- can then neither swap the binary
# before it runs nor put a socket of its own where the VMM connects. The
# backend starts when the VMM first connects.
#
# Both halves run as root in network namespaces of their own (unshare --net):
# neither needs a network, the vhost-user socket is a path, and a namespace
# made by root still hears the kernel's uevents. NVGPU_SANDBOX=off leaves the
# backend in the host's.

# Only root's files: the launcher's header says why. Each path is replaced
# by its resolved form, which from here on only root can change. (The
# launcher itself and its pieces were checked before they were read.)
root_only_files() {
    BACKEND_BIN=$(root_owned "$BACKEND_BIN" "the backend")
    VMM=$(root_owned "$VMM" "the VMM")
    KERNEL=$(root_owned "$KERNEL" "the guest kernel")
    ROOTFS=$(root_owned "$ROOTFS" "the rootfs")
    if [ -n "$NVIDIA_SHARE" ]; then
        NVIDIA_SHARE=$(root_owned "$NVIDIA_SHARE" "the share (NVGPU_NVIDIA_SHARE)")
        # What nesbox would start for it: its NESBOX_VIRTIOFSD, or the one on
        # PATH, named explicitly from here on.
        VFS=${NESBOX_VIRTIOFSD:-$(command -v virtiofsd || true)}
        if [ -n "$VFS" ]; then
            NESBOX_VIRTIOFSD=$(root_owned "$VFS" "virtiofsd (NESBOX_VIRTIOFSD)")
            export NESBOX_VIRTIOFSD
        fi
    fi
    # The logs directory is made if missing, so its nearest existing
    # ancestor is checked first.
    d=$LOGS
    while [ ! -e "$d" ]; do d=$(dirname -- "$d"); done
    root_owned "$d" "the logs directory's parent" >/dev/null
    mkdir -p -- "$LOGS"
    LOGS=$(root_owned "$LOGS" "the logs directory")
}

# A slot of the pool: this VM's own host users.
#
# Taken first, as root, so that nothing after it can mistake another VM's
# processes for this one's. The lock is held by this launcher for the run; a
# launcher killed outright lets go of it while its VM may still run, which is
# why a slot whose users have live processes is not free either. Children
# are started with the lock's descriptor closed ({SLOT_FD}>&-).
take_slot() {
    mkdir -p /run/nvgpu-slots
    chmod 0700 /run/nvgpu-slots
    MAX_SLOTS=${NVGPU_VM_SLOTS:-64}
    [[ $MAX_SLOTS =~ ^[0-9]+$ ]] || die "NVGPU_VM_SLOTS=$MAX_SLOTS: a number"
    for ((i = 0; i < MAX_SLOTS; i++)); do
        id "nvgpu-vm$i" >/dev/null 2>&1 || break
        exec {SLOT_FD}>&-
        exec {SLOT_FD}>>"/run/nvgpu-slots/$i.lock"
        flock -n "$SLOT_FD" || continue
        if pgrep -u "nvgpu-vm$i" >/dev/null ||
            { id "nvgpu-vmm$i" >/dev/null 2>&1 && pgrep -u "nvgpu-vmm$i" >/dev/null; }; then
            echo "run-guest: slot $i is unlocked but its users still run something; skipping it" >&2
            continue
        fi
        SLOT=$i
        break
    done
    if [ -z "$SLOT" ]; then
        exec {SLOT_FD}>&-
        exec {SLOT_FD}</dev/null
        if [ "$i" -gt 0 ]; then
            die "all $i slots of the pool (nvgpu-vm0..nvgpu-vm$((i - 1))) are in use"
        fi
    else
        SLOT_GROUP=nvgpu-vm$SLOT
        getent group "$SLOT_GROUP" >/dev/null ||
            die "no group $SLOT_GROUP: make the pool's users with --user-group (the header of $LIB/root.sh)"
        if id "nvgpu-vmm$SLOT" >/dev/null 2>&1; then
            VMM_USER=nvgpu-vmm$SLOT
            [ "$(id -gn "$VMM_USER")" = "$SLOT_GROUP" ] ||
                die "$VMM_USER's group is $(id -gn "$VMM_USER"), not $SLOT_GROUP (the header of $LIB/root.sh)"
        fi
        echo "run-guest: slot $SLOT (nvgpu-vm$SLOT${VMM_USER:+, $VMM_USER})" >&2
    fi
}

# Who the backend runs as (this file's header), and how it is started: as
# $NVGPU_USER, with only the device nodes' groups, no capabilities to inherit
# or regain, and no way to gain privilege by exec.
root_backend_user() {
    if [ -n "${NVGPU_USER:-}" ]; then
        echo "run-guest: NVGPU_USER=$NVGPU_USER: every backend started this way is that one" \
            "user, and those VMs are one principal to the host" >&2
    elif [ -n "$WL_SOCK" ]; then
        NVGPU_USER=$(stat -c %U "$WL_SOCK") || {
            echo "--wayland-socket $WL_SOCK: no such socket" >&2
            exit 1
        }
    elif [ -n "$WL_EXPORT" ]; then
        NVGPU_USER=$(stat -c %U "$(dirname "$WL_EXPORT")") || {
            echo "--wayland-export $WL_EXPORT: no such directory" >&2
            exit 1
        }
    elif [ -n "$SLOT" ]; then
        NVGPU_USER=nvgpu-vm$SLOT
    elif id nvgpu >/dev/null 2>&1; then
        NVGPU_USER=nvgpu
        echo "run-guest: WARNING: no pool of VM users (nvgpu-vm0, ...); every VM's backend" \
            "is the one user nvgpu, and can signal the others, read their files and share" \
            "RM's security token. Make the pool: the header of $LIB/root.sh" >&2
    else
        die "no user to run the backend as: make the pool of VM users (the header of $LIB/root.sh)"
    fi
    id "$NVGPU_USER" >/dev/null 2>&1 || {
        echo "no user $NVGPU_USER to run the backend as; see the header of $LIB/root.sh" >&2
        exit 1
    }

    # The groups the device nodes want, those of them this host has.
    GROUPS_WANTED=()
    for g in video render kvm; do
        getent group "$g" >/dev/null && GROUPS_WANTED+=("$g")
    done
    if [ ${#GROUPS_WANTED[@]} -gt 0 ]; then
        GROUP_OPT=--groups=$(IFS=,; echo "${GROUPS_WANTED[*]}")
    else
        GROUP_OPT=--clear-groups
    fi

    # How the backend is started: as $NVGPU_USER, with only those groups, no
    # capabilities to inherit or regain, and no way to gain privilege by exec.
    BACKEND_UID=$(id -u "$NVGPU_USER")
    AS_BACKEND=(setpriv --reuid="$BACKEND_UID" --regid="$(id -g "$NVGPU_USER")"
        "$GROUP_OPT" --inh-caps=-all --bounding-set=-all --no-new-privs)
    if [ "$BACKEND_UID" = 0 ]; then
        [ "${NVGPU_ALLOW_ROOT_UNSAFE:-}" = 1 ] || {
            echo "refusing to run the backend as root: every guest process would be an" \
                "RM administrator. NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 if you mean it." >&2
            exit 1
        }
        echo "WARNING: backend runs as root (NVGPU_ALLOW_ROOT_UNSAFE=1)" >&2
        AS_BACKEND=()
        BACKEND_ARGS+=(--allow-root-unsafe)
    fi

    # Its network namespace is made here, where that needs no user
    # namespace and the namespace still hears the kernel's uevents; the
    # backend finds only loopback and keeps it (device/src/sandbox.rs).
    if [ "$SANDBOX" = on ]; then
        BACKEND_NETNS=(unshare --net --)
    fi
}

# Who the VMM runs as: the slot's nvgpu-vmmN, under nesbox's jailer, in a
# network namespace of its own. nesbox's own "unshare-network" cannot be
# used there: the kernel refuses a user namespace to a chrooted process.
root_vmm_user() {
    VMM_JAIL=${NVGPU_VMM_JAIL:-on}
    case $VMM_JAIL in auto | on | off) ;; *) die "NVGPU_VMM_JAIL=$VMM_JAIL: on, auto or off" ;; esac
    JAILER=${NVGPU_JAILER:-$(dirname -- "$VMM")/jailer}
    why=
    if [ "$VMM_JAIL" != off ]; then
        if [ -z "$VMM_USER" ]; then
            why="no pool slot with a VMM user (nvgpu-vmmN; the header of $LIB/root.sh)"
        elif [ ! -x "$JAILER" ]; then
            why="no jailer at $JAILER (NVGPU_JAILER; cargo build --release -p jailer in nesbox)"
        elif [ "$(stat -L -c %a /dev/kvm 2>/dev/null)" != 666 ]; then
            why="/dev/kvm is not 0666, and the jailer clears supplementary groups"
        fi
    fi
    if [ "$VMM_JAIL" = off ]; then
        echo "run-guest: WARNING: the VMM runs as root (NVGPU_VMM_JAIL=off)" >&2
    elif [ -n "$why" ]; then
        [ "$VMM_JAIL" = on ] &&
            die "the VMM would run as root, unjailed: $why (NVGPU_VMM_JAIL=auto or off to" \
                "allow that)"
        echo "run-guest: WARNING: the VMM runs as root, unjailed: $why" >&2
        VMM_JAIL=off
    else
        VMM_JAIL=on
        # Run by root, so root's alone.
        JAILER=$(root_owned "$JAILER" "the jailer (NVGPU_JAILER)")
    fi
    VMM_NETNS=(unshare --net --)
}

# The run's directory, root's: the socket is bound in it and the backend
# run from it.
root_run_dir() {
    # Root's, 0711, from the start to the end of the run, and never the
    # backend user's (the header, and SECURITY.md, "The backend's socket"):
    # root binds the socket
    # in it and hands it to the backend, and the backend's user -- which may
    # have other live processes -- can neither rename the binary copied here nor
    # put a socket of its own where the VMM connects. The binary is copied
    # here because /root is not the backend user's to read. (root's
    # environment does not describe another user's $XDG_RUNTIME_DIR.)
    RUN=$(mktemp -d /run/nvgpu.XXXXXX)
    chmod 0711 "$RUN"
    install -m 0755 "$BACKEND_BIN" "$RUN/vhost-user-nvgpu"
    BACKEND_EXE=$RUN/vhost-user-nvgpu
    # The GPUs' whole PCI config space, which sysfs gives the backend's own
    # user only 64 bytes of: root snapshots it, as the unit's ExecStartPre
    # does (contrib/systemd/nvgpu-pci-snapshot), for --pci-config-dir.
    mkdir -m 0755 "$RUN/pci"
    for g in /proc/driver/nvidia/gpus/*; do
        [ -d "$g" ] || continue
        a=${g##*/}
        [ -r "/sys/bus/pci/devices/$a/config" ] || continue
        cat "/sys/bus/pci/devices/$a/config" > "$RUN/pci/$a"
        chmod 0644 "$RUN/pci/$a"
    done
    BACKEND_ARGS+=(--pci-config-dir "$RUN/pci")
    # What binds the socket and execs the backend with it (systemd's, run
    # by root: root's alone).
    SOCKET_ACTIVATE=$(command -v systemd-socket-activate) ||
        die "as root the launcher binds the backend's socket itself, with systemd-socket-activate," \
            "and there is none on PATH"
    SOCKET_ACTIVATE=$(root_owned "$SOCKET_ACTIVATE" "systemd-socket-activate")
}

# The disk, the jailed VMM's to write: the launcher's copy of it is made
# the VMM's user's, and an image booted in place must be already.
give_disk_to_vmm() {
    if [ -n "$DISK_COPY" ]; then
        chown "$VMM_USER:$SLOT_GROUP" "$DISK_COPY"
        chmod 0600 "$DISK_COPY"
    elif [ "$(stat -c %U "$DISK")" != "$VMM_USER" ]; then
        die "$DISK is booted in place (NVGPU_COPY_ROOTFS=0) but is not $VMM_USER's to write"
    fi
}

# The VMM's jail.
#
# The jail image is the jailer's read-only lower layer: here nesbox (at
# /usr/bin/nesbox, where the jailer looks) and, for a share, virtiofsd, each
# with the loader and libraries it names, at their own paths. Everything the
# config names -- kernel, disk, socket, share, the config itself -- the jailer
# binds in at the same path. NVGPU_JAIL_ROOT uses a materialized image instead.
jail_add() {
    local src=$1 dst=$2 out lib
    install -D -m 0755 -- "$src" "$JAIL_ROOT$dst"
    command -v ldd >/dev/null || die "no ldd to find what $src links against; NVGPU_JAIL_ROOT"
    out=$(ldd -- "$src" 2>&1) || true
    case $out in *"not a dynamic executable"* | *"statically linked"*) return 0 ;; esac
    # Each library is root's, as everything else root hands the VMM is.
    local libs real
    libs=$(printf '%s\n' "$out" | awk '$2 == "=>" && $3 ~ /^\// { print $3 } $1 ~ /^\// { print $1 }')
    while read -r lib; do
        [ -n "$lib" ] && [ ! -e "$JAIL_ROOT$lib" ] || continue
        real=$(root_owned "$lib" "a library of $src")
        install -D -m 0755 -- "$real" "$JAIL_ROOT$lib"
    done <<<"$libs"
    if [ -e /etc/ld.so.cache ] && [ ! -e "$JAIL_ROOT/etc/ld.so.cache" ]; then
        real=$(root_owned /etc/ld.so.cache "the loader's cache")
        install -D -m 0644 -- "$real" "$JAIL_ROOT/etc/ld.so.cache"
    fi
}

# The jail image, built or given, and the VMM's command line under the
# jailer.
jail_vmm() {
    if [ -n "${NVGPU_JAIL_ROOT:-}" ]; then
        JAIL_ROOT=$(root_owned "$NVGPU_JAIL_ROOT" "the jail image (NVGPU_JAIL_ROOT)")
        [ -x "$JAIL_ROOT/usr/bin/nesbox" ] || die "NVGPU_JAIL_ROOT=$JAIL_ROOT has no usr/bin/nesbox"
    else
        JAIL_ROOT=$(mktemp -d /run/nvgpu-jail.XXXXXX)
        JAIL_BUILT=$JAIL_ROOT
        chmod 0755 "$JAIL_ROOT"
        jail_add "$VMM" /usr/bin/nesbox
        if [ -n "$NVIDIA_SHARE" ]; then
            VFS=${NESBOX_VIRTIOFSD:-$(command -v virtiofsd || true)}
            [ -n "$VFS" ] || die "a share needs virtiofsd in the jail, and none is on PATH (NESBOX_VIRTIOFSD)"
            VFS=$(realpath -- "$VFS")
            jail_add "$VFS" "$VFS"
            export NESBOX_VIRTIOFSD=$VFS
        fi
    fi
    VMM_CMD=("${VMM_NETNS[@]}" "$JAILER" --config "$CFG" --jail-root "$JAIL_ROOT"
        --uid "$(id -u "$VMM_USER")" --gid "$(getent group "$SLOT_GROUP" | cut -d: -f3)")
}

# The socket, checked (await_socket), opened to the VMM: the jailed VMM
# is another user, in the slot's group, which holds no one else. (The
# jailer binds the socket file alone into the jail.) -h and the checks
# before: never through a link.
socket_to_vmm() {
    if [ "$VMM_JAIL" = on ]; then
        chgrp -h -- "$SLOT_GROUP" "$SOCK"
        chmod 0660 -- "$SOCK"
    else
        chmod 0600 -- "$SOCK"
    fi
}
