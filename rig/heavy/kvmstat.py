#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""KVM's own counters for a guest run: VM exits and their kinds, halts and
halt polling, faults, per vCPU, over a window of the run.

    kvmstat.py --out FILE [--delay S] [--secs S] -- LAUNCHER [ARGS...]

Runs LAUNCHER (rig/run-guest.sh, as rig/rig-heavy.sh does with
NVGPU_HEAVY_KVMSTAT=1) as a child, finds the VMM among its descendants
(nesbox or crosvm), and DELAY seconds later takes a duplicate of each of
the VMM's KVM statistics descriptors (pidfd_getfd: allowed to an ancestor
under Yama's ptrace scope 1) and reads them at the start and the end of a
SECS window. KVM answers KVM_GET_STATS_FD only to the process that made the
VM, so the VMM must hold them: nesbox does with NESBOX_HOLD_KVM_STATS=1
(its virtio-nvgpu-v7 branch; rig/rig-heavy.sh sets it). FILE gets a line
per counter: the vCPUs' sum, its rate a second, and the busiest vCPU's
rate. The launcher's exit status is kept. Nothing is
written to the VMM or KVM; perf and tracefs are not needed.
"""
import argparse
import ctypes
import os
import signal
import struct
import sys
import time

SYS_PIDFD_OPEN = 434
SYS_PIDFD_GETFD = 438
libc = ctypes.CDLL(None, use_errno=True)


def syscall(nr, *args):
    r = libc.syscall(nr, *[ctypes.c_long(a) for a in args])
    if r < 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
    return r


def children(pid):
    out = []
    try:
        for t in os.listdir(f"/proc/{pid}/task"):
            with open(f"/proc/{pid}/task/{t}/children") as f:
                out += [int(x) for x in f.read().split()]
    except OSError:
        pass
    return out


def find_vmm(root):
    todo, seen = [root], set()
    while todo:
        p = todo.pop()
        if p in seen:
            continue
        seen.add(p)
        try:
            with open(f"/proc/{p}/comm") as f:
                if f.read().strip() in ("nesbox", "crosvm"):
                    # crosvm's device processes are its children; the vCPUs
                    # are the one with kvm-vcpu descriptors.
                    if any(k for _, k in kvm_fds(p) if k.startswith("vcpu")):
                        return p
        except OSError:
            pass
        todo += children(p)
    return None


def kvm_fds(pid):
    out = []
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                t = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                continue
            if t.startswith("anon_inode:kvm-vcpu:"):
                out.append((int(fd), "vcpu" + t.rsplit(":", 1)[1]))
            elif t.startswith("anon_inode:kvm-vcpu-stats:"):
                out.append((int(fd), "stats" + t.rsplit(":", 1)[1]))
    except OSError:
        pass
    return out


class Stats:
    def __init__(self, fd):
        self.fd = fd
        flags, name_size, num, _id_off, desc_off, data_off = struct.unpack(
            "6I", os.pread(fd, 24, 0))
        self.data_off = data_off
        dsz = 16 + name_size
        raw = os.pread(fd, dsz * num, desc_off)
        self.desc = []
        for i in range(num):
            fl, _exp, size, off, _bucket = struct.unpack_from("IhHII", raw, i * dsz)
            name = raw[i * dsz + 16:(i + 1) * dsz].split(b"\0", 1)[0].decode()
            # Plain counters only; the type is in flags' low nibble
            # (0 cumulative, 1 instant, 2 peak).
            if size == 1 and (fl & 0xF) == 0:
                self.desc.append((name, off))

    def read(self):
        return {n: struct.unpack("Q", os.pread(self.fd, 8, self.data_off + off))[0]
                for n, off in self.desc}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--delay", type=float, default=30)
    ap.add_argument("--secs", type=float, default=15)
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    child = os.fork()
    if child == 0:
        os.execvp(cmd[0], cmd)
    signal.signal(signal.SIGTERM, lambda *_: os.kill(child, signal.SIGTERM))
    lines = []
    try:
        vmm, t_end = None, time.time() + 60
        while vmm is None and time.time() < t_end:
            vmm = find_vmm(child)
            if vmm is None:
                time.sleep(0.2)
        if vmm is None:
            raise RuntimeError("no VMM with vCPUs among the launcher's descendants")
        time.sleep(a.delay)
        pidfd = syscall(SYS_PIDFD_OPEN, vmm, 0)
        stats = {}
        for fd, kind in kvm_fds(vmm):
            if kind.startswith("stats"):
                stats["vcpu%02d" % int(kind[5:])] = Stats(syscall(SYS_PIDFD_GETFD, pidfd, fd, 0))
        if not stats:
            raise RuntimeError("the VMM holds no KVM statistics descriptors "
                               "(nesbox: NESBOX_HOLD_KVM_STATS=1)")
        t0 = time.time()
        s0 = {k: s.read() for k, s in stats.items()}
        time.sleep(a.secs)
        s1 = {k: s.read() for k, s in stats.items()}
        dt = time.time() - t0
        vcpus = sorted(k for k in stats if k.startswith("vcpu"))
        lines.append(f"KVMSTAT window={dt:.1f}s vcpus={len(vcpus)} vmm={vmm}")
        names = sorted({n for k in vcpus for n in s1[k]})
        for n in names:
            d = [s1[k][n] - s0[k][n] for k in vcpus]
            tot = sum(d)
            if tot:
                lines.append(f"KVMSTAT vcpu {n} total={tot} per_s={tot / dt:.0f} "
                             f"max_vcpu_per_s={max(d) / dt:.0f}")
    except Exception as e:  # the run goes on; the file says why it has no numbers
        lines.append(f"KVMSTAT error: {e}")
    with open(a.out, "w") as f:
        f.write("\n".join(lines) + "\n")
    _, st = os.waitpid(child, 0)
    sys.exit(os.waitstatus_to_exitcode(st))


if __name__ == "__main__":
    main()
