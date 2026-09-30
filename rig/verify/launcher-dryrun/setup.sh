#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Run with: unshare --user --map-root-user --mount --pid --fork bash setup.sh <old> <new>
# Builds a tmpfs root with a root-owned rig and chroots into it to run inside.sh.
# NixOS: the tools come from /run/current-system and /nix.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
OLD=$1 NEW=$2
R=$D/root
mkdir -p "$R"
mount -t tmpfs tmpfs "$R"
chmod 0755 "$R"
mkdir -p "$R"/{nix,bin,usr,proc,dev,etc,run,tmp,root,stubs,home} "$R"/rig/{bin,kernel,guest,logs}
mount --rbind /nix "$R/nix"
mount --rbind /bin "$R/bin"
mount -t tmpfs tmpfs "$R/run"
mkdir -p "$R/run/current-system"
mount --rbind /run/current-system "$R/run/current-system"
mount -t tmpfs tmpfs "$R/dev"
for n in null zero urandom random; do touch "$R/dev/$n"; chmod 0666 "$R/dev/$n"; done  # plain files: this sandbox mounts nodev
touch "$R/dev/kvm"; chmod 0666 "$R/dev/kvm"; ln -s /proc/self/fd "$R/dev/fd"
mount -t proc proc "$R/proc"
chmod 1777 "$R/tmp"
# A host file owned by real root: uid 65534 in here.
cat > "$R/etc/passwd" <<'EOF'
root:x:0:0:root:/root:/bin/sh
nvgpu-vm0:x:0:0::/:/bin/false
nvgpu-vmm0:x:0:0::/:/bin/false
EOF
cat > "$R/etc/group" <<'EOF'
nvgpu-vm0:x:0:
root:x:0:
EOF
printf 'passwd: files\ngroup: files\nshadow: files\n' > "$R/etc/nsswitch.conf"
# /usr of the chroot's own (root's, here): the new launcher starts again, as
# root, with PATH=/usr/sbin:/usr/bin:...:/run/current-system/sw/bin, so the
# stubs it must find go in /usr/sbin, and what it runs as root and checks is
# root's (/nix is real root's, uid 65534 here).
mkdir -p "$R/usr/bin" "$R/usr/sbin"
ln -s /run/current-system/sw/bin/env "$R/usr/bin/env"
# systemd-socket-activate, behind a wrapper that logs the umask it binds the
# socket with.
mkdir -p "$R/usr/lib"
cp "$(command -v systemd-socket-activate)" "$R/usr/lib/systemd-socket-activate"
printf '#!/bin/sh\necho "socket-activate umask $(umask)" >> /rig/logs/umask.log\nexec /usr/lib/systemd-socket-activate "$@"\n' \
    > "$R/usr/bin/systemd-socket-activate"
chmod 0755 "$R/usr/bin/systemd-socket-activate"
# pgrep: a slot's users "run nothing" (every user is uid 0 here, and this
# shell would otherwise count).
cat > "$R/stubs/pgrep" <<'EOF'
#!/bin/sh
case " $* " in *" -u nvgpu-vm"*) exit 1 ;; esac
exec /run/current-system/sw/bin/pgrep "$@"
EOF
# chown, chgrp: every change of ownership the launcher makes, logged (one
# uid maps here, so a chown to the backend's user changes nothing to see).
for t in chown chgrp; do
    printf '#!/bin/sh\necho "%s $*" >> /rig/logs/ownership.log\nexec /run/current-system/sw/bin/%s "$@"\n' "$t" "$t" > "$R/stubs/$t"
done
chmod 0755 "$R/stubs/"*
cp "$R/stubs/"* "$R/usr/sbin/"
cp "$D/jailer.sh" "$R/rig/bin/jailer"
cp "$D/stubvmm" "$R/rig/bin/nesbox"
cp "$D/stubvmm-dyn" "$R/rig/bin/nesbox-dyn"
cp "$D/backend.sh" "$R/rig/bin/vhost-user-nvgpu"
chmod 0755 "$R/rig/bin/"*
echo kernel > "$R/rig/kernel/vmlinux"
truncate -s 1M "$R/rig/guest/rootfs.ext4"
cp "$OLD" "$R/rig/run-guest.old.sh"
cp "$NEW" "$R/rig/run-guest.new.sh"
# The new launcher's pieces, beside it, as an install has them.
cp -r "$(dirname "$NEW")/launcher" "$R/rig/launcher"
chmod 0755 "$R/rig/launcher"
chmod 0644 "$R/rig/launcher/"*.sh
cp "$D/inside.sh" "$R/inside.sh"
exec chroot "$R" /run/current-system/sw/bin/bash /inside.sh
