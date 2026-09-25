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
and the last to every newer host (the profile rule of gen/schema/nvkms.py).
Measured releases whose tables are the same are one range. Unlike NVKMS's,
these ranges are not a guess for the releases in between: `uvm_extract.py
scan` measures every published tag and fails if one differs from the table
it would get, and the releases in VERSIONS that are not ABI profiles are
there because it found a change.
"""

import json
from pathlib import Path

from .nvkms import LAST, before, version_of

DATA = Path(__file__).resolve().parent.parent / 'uvm'
FORMAT = 'virtio-nvgpu/uvm-params/1'

# What a command's descriptor names (uvm_extract.py's "of").
FD_OF = ('rmctl', 'uvm')


class DataError(Exception):
    pass


class UvmTable:
    def __init__(self, name, vmin, vmax, commands):
        self.name = name
        self.vmin = vmin
        self.vmax = vmax
        self.commands = commands  # [{name, cmd, size, fd}], by command number


def load():
    releases = []
    for p in sorted(DATA.glob('*.json'), key=lambda p: version_of(p.stem)):
        d = json.loads(p.read_text())
        if d.get('format') != FORMAT:
            raise DataError(f'{p.name}: format {d.get("format")!r}')
        if d['driver_version'] != p.stem:
            raise DataError(f'{p.name}: driver_version {d["driver_version"]!r}')
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
    for d in load():
        cmds = sorted(d['commands'], key=lambda c: c['cmd'])
        if out and out[-1].commands == cmds:
            continue
        out.append(UvmTable('v' + d['driver_version'].replace('.', '_'),
                            version_of(d['driver_version']), None, cmds))
    for t, nxt in zip(out, out[1:]):
        t.vmax = before(nxt.vmin)
    out[-1].vmax = LAST
    return out


TABLES = tables()
