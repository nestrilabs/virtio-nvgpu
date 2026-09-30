#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""What each thread of a process is blocked in (rig/heavy/hang-watch.sh).

Reads /proc/<pid>/task/<tid>/syscall and, for the arguments that point into
the process, /proc/<pid>/mem: a poll's descriptors (each named by what it
is open on) and requested events, a futex's word as it is now, an ioctl's
descriptor and command, an epoll wait's descriptor. x86-64 numbers. Needs
the rights to read the process's memory (root, or its owner without
Yama's ptrace scope).

Usage: waits.py <pid>
"""
import os
import struct
import sys

NAMES = {0: "read", 7: "poll", 16: "ioctl", 23: "select", 35: "nanosleep",
         61: "wait4", 202: "futex", 219: "restart_syscall", 230: "clock_nanosleep",
         232: "epoll_wait", 270: "pselect6", 271: "ppoll", 281: "epoll_pwait",
         441: "epoll_pwait2"}


def main(pid):
    def fdname(fd):
        try:
            return os.readlink("/proc/%s/fd/%d" % (pid, fd))
        except OSError:
            return "?"

    def peek(addr, n):
        try:
            with open("/proc/%s/mem" % pid, "rb") as m:
                m.seek(addr)
                return m.read(n)
        except (OSError, ValueError, OverflowError):
            return None

    for tid in sorted(os.listdir("/proc/%s/task" % pid), key=int):
        try:
            with open("/proc/%s/task/%s/syscall" % (pid, tid)) as f:
                sc = f.read().split()
            with open("/proc/%s/task/%s/comm" % (pid, tid)) as f:
                comm = f.read().strip()
        except OSError as e:
            print("tid %s: %s" % (tid, e))
            continue
        if not sc or sc[0] in ("running", "-1"):
            print("tid %s %-16s %s" % (tid, comm, " ".join(sc) or "?"))
            continue
        nr = int(sc[0])
        a = [int(x, 16) for x in sc[1:7]]
        line = "tid %s %-16s %s" % (tid, comm, NAMES.get(nr, "syscall %d" % nr))
        if nr in (7, 271):
            n = min(a[1], 64)
            raw = peek(a[0], 8 * n)
            if raw is None:
                line += " (pollfds unreadable)"
            else:
                fds = []
                for i in range(n):
                    fd, ev, _ = struct.unpack_from("<ihh", raw, 8 * i)
                    fds.append("%d=%s ev=%#x" % (fd, fdname(fd), ev & 0xFFFF))
                # poll's third argument is milliseconds, ppoll's a timespec pointer.
                line += " n=%d %s=%#x: %s" % (a[1], "timeout_ms" if nr == 7 else "timeout_ts", a[2],
                                               ", ".join(fds))
        elif nr in (232, 281, 441):
            to = a[3] - (1 << 64) if a[3] >= 1 << 63 else a[3]
            line += " epfd=%d=%s timeout=%d" % (a[0], fdname(a[0]), to)
        elif nr == 16:
            line += " fd=%d=%s cmd=%#x" % (a[0], fdname(a[0]), a[1])
        elif nr == 202:
            raw = peek(a[0], 4)
            now = struct.unpack("<I", raw)[0] if raw else None
            line += " uaddr=%#x op=%#x val=%#x now=%s timeout=%#x" % (
                a[0], a[1], a[2], "?" if now is None else "%#x" % now, a[3])
        elif nr == 0:
            line += " fd=%d=%s" % (a[0], fdname(a[0]))
        else:
            line += " args=" + " ".join("%#x" % x for x in a)
        print(line)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
