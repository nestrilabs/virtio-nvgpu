# SPDX-License-Identifier: Apache-2.0
"""UVM parameter blocks, one table per range of host driver releases.

Not IOCTL2 schema: nvidia-uvm's parameters are flat (the commands that
carried a CPU buffer are refused, device/src/guestptr.rs), so what both
sides need is only the size of each command's block -- which the ioctl
number does not carry -- and, for the backend, where the commands that name
another open file keep that descriptor (device/src/uvmfd.rs). The guest
copies exactly that many bytes in and out, the backend forwards a block of
exactly that size and nothing else, and a command with no row is refused by
both: it is not one the backend lets through, or this release has no such
command.

Every number comes from gen/uvm/<release>.json, which gen/uvm_extract.py
measures with the compiler against that release's own uvm_ioctl.h. Ranges:
a table runs from its release to the one before the next measured release,
and the last to the newest release measured -- not to every newer host, as
NVKMS's does: nothing has looked at a release past it.
Measured releases whose tables are the same are one range. Unlike NVKMS's,
these ranges are not a guess for the releases in between: `uvm_extract.py
scan` measures every published tag and fails if one differs from the table
it would get, and the releases in VERSIONS that are not ABI profiles are
there because it found a change.
"""

import json
from pathlib import Path

from .nvkms import before, version_of

DATA = Path(__file__).resolve().parent.parent / 'uvm'
FORMAT = 'virtio-nvgpu/uvm-params/1'

# What a command's descriptor names (uvm_extract.py's "of").
FD_OF = ('rmctl', 'uvm')


class DataError(Exception):
    pass


class UvmTable:
    def __init__(self, name, vmin, vmax, commands, init_flags_mask):
        self.name = name
        self.vmin = vmin
        self.vmax = vmax
        self.commands = commands  # [{name, cmd, size, fd}], by command number
        # UVM_INIT_FLAGS_MASK: the initialization flags this release takes.
        self.init_flags_mask = init_flags_mask


def load():
    releases = []
    for p in sorted(DATA.glob('*.json'), key=lambda p: version_of(p.stem)):
        d = json.loads(p.read_text())
        if d.get('format') != FORMAT:
            raise DataError(f'{p.name}: format {d.get("format")!r}')
        if d['driver_version'] != p.stem:
            raise DataError(f'{p.name}: driver_version {d["driver_version"]!r}')
        m = d.get('init_flags_mask')
        if not isinstance(m, int) or m < 0 or not m & 1:
            raise DataError(f'{p.name}: init_flags_mask {m!r} (it must take '
                            'UVM_INIT_FLAGS_DISABLE_HMM)')
        seen = set()
        for c in d['commands']:
            if c['cmd'] in seen:
                raise DataError(f'{p.name}: {c["name"]}: number listed twice')
            seen.add(c['cmd'])
            if c['fd'] and (c['fd']['of'] not in FD_OF
                            or c['fd']['offset'] + 4 > c['size']):
                raise DataError(f'{p.name}: {c["name"]}: bad descriptor field')
        releases.append(d)
    if not releases:
        raise DataError('gen/uvm has no releases')
    return releases


def tables():
    out = []
    releases = load()
    for d in releases:
        cmds = sorted(d['commands'], key=lambda c: c['cmd'])
        mask = d['init_flags_mask']
        if out and out[-1].commands == cmds and out[-1].init_flags_mask == mask:
            continue
        out.append(UvmTable('v' + d['driver_version'].replace('.', '_'),
                            version_of(d['driver_version']), None, cmds, mask))
    for t, nxt in zip(out, out[1:]):
        t.vmax = before(nxt.vmin)
    # The last table ends at the newest release measured, not at every newer
    # one: a release published after it may change a block (590.44.01 did),
    # and nothing has scanned it. A host past it has no UVM table on either
    # side, so compute is refused there until it is measured.
    out[-1].vmax = version_of(releases[-1]['driver_version'])
    return out


TABLES = tables()
