#!/usr/bin/env python3
"""schema_gen.py -- emit the IOCTL2 schema tables for both interpreters.

One source (gen/schema/*.py), two outputs that must never disagree:

    driver/gen/nvgpu_schema.h        the guest's tables (C)
    gen/src/schema/generated.rs      the backend's tables (Rust, crate abi)

Usage (from anywhere):

    gen/schema_gen.py                 regenerate both, in place
    gen/schema_gen.py --out DIR       write both under DIR instead (the
                                      staleness test compares those with the
                                      checked-in files)
    gen/schema_gen.py --probe DIR     write offset probes (C): every struct
                                      size, field offset, ioctl number and
                                      fourcc the schema states, as
                                      _Static_asserts, to be compiled against
                                      the real headers (below)

The probes only have to compile. With LINUX a kernel source tree, KBUILD its
build tree and NV an open-gpu-kernel-modules checkout of the host version:

    gcc -fsyntax-only -D__user= -Wno-cpp \
        -I$LINUX/include/uapi -I$LINUX/arch/x86/include/uapi \
        -I$KBUILD/arch/x86/include/generated/uapi \
        -I$NV/kernel-open/nvidia-drm -I$NV/src/nvidia-modeset/kapi/interface \
        -I$NV/src/nvidia-modeset/interface -I$NV/src/common/unix/common/inc \
        -I$NV/src/common/inc -I$NV/src/common/sdk/nvidia/inc DIR/probe_drm.c

and each DIR/probe_<nvkms table>.c the same way with the last four -I's.

Checked in CI by gen/src/schema/mod.rs's staleness test, which regenerates
into a temporary directory and compares: edit the Python, run this, commit
both outputs.

The schema is checked before anything is written: every field inside its
struct, no two fields overlapping unless they are exclusive alternatives,
every count readable where it is read, every copy-back count present in both
directions, every pointer capped. A schema that fails a check is a bug in the
schema, and nothing is emitted.
"""

import argparse
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from schema import drm_kms, drm_render, formats, nvidia_drm, nvkms  # noqa: E402
from schema.lang import *  # noqa: E402,F403

C_OUT = Path('driver/gen/nvgpu_schema.h')
RS_OUT = Path('gen/src/schema/generated.rs')
REPO = HERE.parent

MAX_LIST = 32
NVKMS_IOCTL_IOWR = ioc(IOWR, 'm', 0, 16)

# NVGPU_SFF_* in the C header.
FF_COND, FF_VALIDATE = 1, 2


class SchemaError(Exception):
    pass


def fail(where, msg):
    raise SchemaError(f'{where}: {msg}')


# ─────────────────────────── flattening + checks ───────────────────────────

class Flat:
    """One field, flattened: children are a contiguous span of the table."""

    def __init__(self, f, where):
        self.f = f
        self.where = where
        self.child = 0
        self.nchild = 0
        self.sum_index = None


def span_of(f):
    """(start, length) of the bytes a field occupies in its struct."""
    if isinstance(f, Ptr):
        return f.off, 8
    if isinstance(f, Array):
        return f.off, f.count * f.stride
    if isinstance(f, (FdIn, FdOut)):
        return f.off, f.width
    return f.off, 4


class Io:
    """What the enclosing buffer carries: bytes the caller sends (IN) and
    bytes the host writes back (OUT)."""

    def __init__(self, size, has_in, has_out, where):
        self.size = size
        self.has_in = has_in
        self.has_out = has_out
        self.where = where


def check_ref(io, where, what, off, width):
    if width not in (4, 8):
        fail(where, f'{what}: width {width} is not 4 or 8')
    if off < 0 or off + width > io.size:
        fail(where, f'{what} at {off}+{width} is outside the {io.size}-byte '
             'struct')


def exclusive(a, b):
    return (a.cond is not None and b.cond is not None
            and a.cond.off == b.cond.off and a.cond.mask == b.cond.mask
            and a.cond.value != b.cond.value)


def check_list(fields, io, where, root_modeset=False):
    if len(fields) > MAX_LIST:
        fail(where, f'{len(fields)} fields in one list (at most {MAX_LIST})')
    for i, a in enumerate(fields):
        s, n = span_of(a)
        if s < 0 or s + n > io.size:
            fail(where, f'field {a.name!r} at {s}+{n} is outside the '
                 f'{io.size}-byte struct')
        for b in fields[:i]:
            t, m = span_of(b)
            if s < t + m and t < s + n and not exclusive(a, b):
                fail(where, f'fields {b.name!r} and {a.name!r} overlap')
        fw = f'{where}.{a.name or "[]"}'
        if a.cond is not None:
            if not io.has_in:
                fail(fw, 'a condition must read bytes the caller sends')
            check_ref(io, fw, 'condition', a.cond.off, 4)
        if isinstance(a, Ptr):
            check_ptr(a, fields[:i], io, fw, root_modeset)
        elif isinstance(a, Array):
            if a.count <= 0 or a.stride <= 0:
                fail(fw, 'an array needs a count and a stride')
            check_list(a.fields, Io(a.stride, io.has_in, io.has_out, fw), fw)
        elif isinstance(a, FdIn):
            if a.width not in (4, 8) or not a.kinds or not io.has_in:
                fail(fw, 'FdIn needs width 4/8, kinds, and an IN buffer')
        elif isinstance(a, FdOut):
            if a.width not in (4, 8) or not io.has_out:
                fail(fw, 'FdOut needs width 4/8 and a buffer the host writes')
        elif isinstance(a, GemIn):
            if not io.has_in:
                fail(fw, 'GemIn needs an IN buffer')
        elif isinstance(a, GemOut):
            if not io.has_out:
                fail(fw, 'GemOut needs a buffer the host writes')
        else:
            fail(fw, f'unknown field type {type(a).__name__}')


def check_ptr(p, before, io, where, root_modeset):
    if p.dir not in (IN, OUT, INOUT):
        fail(where, 'bad direction')
    ln = p.len
    if isinstance(ln, Const):
        if ln.n > p.max:
            fail(where, 'constant length above the cap')
    elif isinstance(ln, Count):
        if not io.has_in:
            fail(where, 'a count must be read from bytes the caller sends')
        check_ref(io, where, f'count {ln.name!r}', ln.off, ln.width)
        if ln.elem <= 0:
            fail(where, 'count element size must be positive')
    elif isinstance(ln, Sum):
        sib = [b for b in before if isinstance(b, Ptr) and b.name == ln.ptr]
        if not sib:
            fail(where, f'Sum names {ln.ptr!r}, which is not an earlier '
                 'sibling pointer')
        if not (sib[0].dir & IN) or sib[0].fields:
            fail(where, f'Sum source {ln.ptr!r} must be a plain IN array')
    elif isinstance(ln, NvkmsParamsLen):
        if not root_modeset:
            fail(where, 'NvkmsParamsLen only describes the NVKMS params block')
    else:
        fail(where, f'unknown length rule {ln!r}')
    if p.max <= 0 and not (isinstance(ln, Const) and ln.n == 0):
        fail(where, 'every pointer needs a cap')

    cb = p.copyback
    if p.dir == IN:
        if not isinstance(cb, NoCopy):
            fail(where, 'an IN-only buffer has nothing to copy back')
    else:
        if cb is None or isinstance(cb, NoCopy):
            fail(where, 'a buffer the host writes needs a copy-back rule')
        if isinstance(cb, (Partial, AllOrNothing, Exact)):
            if not (io.has_in and io.has_out):
                fail(where, 'a copy-back count must be both sent and returned')
            check_ref(io, where, f'copy-back count {cb.name!r}', cb.off,
                      cb.width)
        elif isinstance(cb, Range):
            if cb.off < 0 or cb.len <= 0 or cb.off + cb.len > p.max:
                fail(where, 'copy-back range outside the buffer')
        elif not isinstance(cb, Full):
            fail(where, f'unknown copy-back rule {cb!r}')

    if p.fields:
        if not (p.dir & IN):
            fail(where, 'fields inside a buffer must be readable: IN or INOUT')
        if p.stride <= 0:
            fail(where, 'a pointer with fields needs a stride')
        check_list(p.fields, Io(p.stride, bool(p.dir & IN),
                                bool(p.dir & OUT), where), where)


def root_io(e):
    return Io(e.size, bool(e.dir & IOC_WRITE), bool(e.dir & IOC_READ), e.name)


class Table:
    """A flattened table: entries plus one field array they index into."""

    def __init__(self, name, versions, entries):
        self.name = name
        self.versions = versions
        self.entries = entries  # list of (Ioctl-like, root fields)
        self.flat = []
        self.roots = []
        for e, fields in entries:
            self.roots.append(self._flatten(fields, e.name))

    def _flatten(self, fields, where):
        start = len(self.flat)
        mine = [Flat(f, f'{where}.{f.name or "[]"}') for f in fields]
        self.flat.extend(mine)
        for i, fl in enumerate(mine):
            f = fl.f
            if isinstance(f, Ptr) and isinstance(f.len, Sum):
                j = [k for k, g in enumerate(fields) if g.name == f.len.ptr]
                fl.sum_index = start + j[0]
            kids = getattr(f, 'fields', None)
            if kids:
                fl.child, fl.nchild = self._flatten(kids, fl.where)
        return start, len(fields)


def modeset_root(m):
    """The params block, as the pointer in the outer struct reaches it."""
    return [Ptr('address', 8, INOUT, NvkmsParamsLen(),
                Range(m.reply_off, m.reply_len), max=m.size,
                elem_struct=m.struct, stride=m.size, fields=m.fields)]


class ModesetEntry:
    """An NVKMS command as an entry of the generic kind."""

    def __init__(self, m):
        self.m = m
        self.name = f'NVKMS_{m.name}'
        self.cls = MODESET
        self.cmd = NVKMS_IOCTL_IOWR
        self.nvkms_cmd = m.nvkms_cmd
        self.size = 16
        self.dir = IOC_WRITE  # the kernel never writes the outer struct
        self.exec = EXECUTOR
        self.special = SPECIAL_NVKMS_PARAMS
        self.policy = m.policy


def build():
    drm = (drm_kms.IOCTLS + nvidia_drm.KMS_IOCTLS + drm_render.IOCTLS
           + nvidia_drm.RENDER_IOCTLS)
    seen = {}
    for e in drm:
        key = (e.cls, e.type, e.nr)
        if key in seen:
            fail(e.name, f'same key as {seen[key]}')
        seen[key] = e.name
        if e.size >= 1 << 14:
            fail(e.name, 'size does not fit _IOC_SIZE')
        if e.dir == IOC_NONE and (e.size or e.fields):
            fail(e.name, 'an _IO ioctl has no argument')
        check_list(e.fields, root_io(e), e.name)
    tables = [Table('drm', None, [(e, e.fields) for e in drm])]

    for t in nvkms.TABLES:
        cmds = set()
        entries = []
        for m in t.ioctls:
            if m.nvkms_cmd in cmds:
                fail(m.name, 'NVKMS command listed twice')
            cmds.add(m.nvkms_cmd)
            if m.reply_off + m.reply_len > m.size:
                fail(m.name, 'reply half outside the params block')
            e = ModesetEntry(m)
            root = modeset_root(m)
            check_list(root, Io(16, True, False, e.name), e.name,
                       root_modeset=True)
            entries.append((e, root))
        if t.vmin > t.vmax:
            fail(t.name, 'empty version range')
        tables.append(Table(t.name, (t.vmin, t.vmax), entries))
    return tables


# ─────────────────────────────── C emission ───────────────────────────────

C_PREAMBLE = '''\
/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The IOCTL2 schema: what an ioctl's argument points at, and where its
 * descriptors and GEM handles are, for the guest's interpreter (nvgpu_i2.c).
 * The backend's copy is gen/src/schema/generated.rs; both are generated from
 * the Python in gen/schema/ by gen/schema_gen.py. DO NOT EDIT -- regenerate.
 *
 * Everything above NVGPU_SCHEMA_TABLES may be included anywhere (the hook
 * implementations compare special ids and kind bits); the tables themselves
 * are static and belong to the interpreter alone.
 */

#ifndef NVGPU_SCHEMA_H
#define NVGPU_SCHEMA_H

#include <linux/types.h>

/* Field kinds. */
#define NVGPU_SF_PTR 1
#define NVGPU_SF_ARRAY 2
#define NVGPU_SF_FD_IN 3
#define NVGPU_SF_FD_OUT 4
#define NVGPU_SF_GEM_IN 5
#define NVGPU_SF_GEM_OUT 6

/* Buffer directions. */
#define NVGPU_SDIR_IN 1
#define NVGPU_SDIR_OUT 2
#define NVGPU_SDIR_INOUT 3

/* Field flags. */
#define NVGPU_SFF_COND (1u << 0)            /* present only if cond holds */
#define NVGPU_SFF_VALIDATE_NVKMS (1u << 1)  /* GEM_IN: backend IDENTIFYs  */

/* Length rules. */
#define NVGPU_SLEN_CONST 1        /* len_a bytes                            */
#define NVGPU_SLEN_COUNT 2        /* uN at len_a (width len_width) x elem   */
#define NVGPU_SLEN_SUM 3          /* sum of u32s of field len_a's buffer    */
#define NVGPU_SLEN_NVKMS_PARAMS 4 /* NvKmsIoctlParams.size, == max          */

/* Copy-back rules. */
#define NVGPU_SCB_NONE 0
#define NVGPU_SCB_FULL 1
#define NVGPU_SCB_PARTIAL 2
#define NVGPU_SCB_ALL_OR_NOTHING 3
#define NVGPU_SCB_EXACT 4
#define NVGPU_SCB_RANGE 5

/* Specials, as passed to nvgpu_i2_ops.special. */
#define NVGPU_SSPECIAL_NONE 0
#define NVGPU_SSPECIAL_ATOMIC 1
#define NVGPU_SSPECIAL_NVKMS_PARAMS 2

/* Backend policies an entry opts into (informational on this side). */
#define NVGPU_SPOL_FENCE (1u << 0)
#define NVGPU_SPOL_GRANT (1u << 1)
#define NVGPU_SPOL_REVOKE (1u << 2)
#define NVGPU_SPOL_FB_CREATE (1u << 3)
#define NVGPU_SPOL_FB_REMOVE (1u << 4)
#define NVGPU_SPOL_FB_READ (1u << 5)
#define NVGPU_SPOL_SETPROP (1u << 6)
#define NVGPU_SPOL_FB_PLANES (1u << 7)
#define NVGPU_SPOL_NVKMS (1u << 8)

/* FD_IN kinds: bit n is NVGPU_HK_* n; Dev is split by device. */
#define NVGPU_SKIND(hk) (1u << (hk))
#define NVGPU_SKIND_DEV_CTL (1u << 16)
#define NVGPU_SKIND_DEV_MODESET (1u << 17)
#define NVGPU_SKIND_DEV_GPU (1u << 18)

/* Entry flags. */
#define NVGPU_SIO_EXECUTOR (1u << 0)    /* runs on the host file's executor */
#define NVGPU_SIO_ARG_IN_ONLY (1u << 1) /* the host never writes the arg    */

/* Longest field list, and deepest nesting, in any table. */
#define NVGPU_SCHEMA_MAX_LIST {max_list}
#define NVGPU_SCHEMA_MAX_DEPTH {max_depth}

#define NVGPU_NVKMS_IOCTL_IOWR 0x{nvkms_iowr:08x}u

#define NVGPU_SCHEMA_VERSION(a, b, c) ((a) * 1000000u + (b) * 1000u + (c))

struct nvgpu_sfield {{
  u32 off;
  u8 kind;  /* NVGPU_SF_* */
  u8 width; /* FD_IN/FD_OUT: 4 or 8 */
  u8 dir;   /* PTR: NVGPU_SDIR_* */
  u8 flags; /* NVGPU_SFF_* */
  u32 cond_off, cond_mask, cond_value;
  u8 len_kind, len_width, cb_kind, cb_width;
  u32 len_a, len_elem;
  u32 max;            /* PTR: most bytes */
  u32 cb_off, cb_arg; /* count offset and element size; RANGE: off, len */
  u32 kinds;          /* FD_IN: NVGPU_SKIND* */
  s32 none_value;     /* FD_IN: the "no descriptor" value */
  u32 stride;         /* PTR elements / ARRAY elements */
  u32 count;          /* ARRAY */
  u16 child, nchild;  /* fields of one element */
}};

struct nvgpu_sioctl {{
  const char *name;
  u32 cmd;       /* the full ioctl number, direction and size included */
  u32 nvkms_cmd; /* MODESET: the NVKMS command inside NvKmsIoctlParams */
  u32 size;      /* == _IOC_SIZE(cmd) */
  u8 sclass;     /* NVGPU_SCLASS_* */
  u8 special;    /* NVGPU_SSPECIAL_* */
  u16 flags;     /* NVGPU_SIO_* */
  u32 policy;    /* NVGPU_SPOL_* */
  u16 field, nfield;
}};

struct nvgpu_stable {{
  const char *name;
  u32 vmin, vmax; /* NVGPU_SCHEMA_VERSION; 0, 0 = any version */
  const struct nvgpu_sioctl *ioctls;
  u32 nioctls;
  const struct nvgpu_sfield *fields;
  u32 nfields;
}};

/* What a guest runs with: the DRM tables, and NVKMS's for the host version. */
struct nvgpu_schema_set {{
  const struct nvgpu_stable *drm;
  const struct nvgpu_stable *modeset; /* NULL: no table for this host */
}};

#ifdef NVGPU_SCHEMA_TABLES
'''


C_DIR = {IN: 'NVGPU_SDIR_IN', OUT: 'NVGPU_SDIR_OUT', INOUT: 'NVGPU_SDIR_INOUT'}
C_SPECIAL = {SPECIAL_NONE: 0, SPECIAL_ATOMIC: 'NVGPU_SSPECIAL_ATOMIC',
             SPECIAL_NVKMS_PARAMS: 'NVGPU_SSPECIAL_NVKMS_PARAMS'}


def c_field(fl, index):
    f = fl.f
    m = {'off': f.off}
    flags = 0
    if f.cond is not None:
        flags |= FF_COND
        m.update(cond_off=f.cond.off, cond_mask=f.cond.mask,
                 cond_value=f.cond.value)
    if isinstance(f, Ptr):
        m.update(kind='NVGPU_SF_PTR', width=8, dir=C_DIR[f.dir], max=f.max,
                 stride=f.stride)
        ln = f.len
        if isinstance(ln, Const):
            m.update(len_kind='NVGPU_SLEN_CONST', len_a=ln.n)
        elif isinstance(ln, Count):
            m.update(len_kind='NVGPU_SLEN_COUNT', len_a=ln.off,
                     len_width=ln.width, len_elem=ln.elem)
        elif isinstance(ln, Sum):
            m.update(len_kind='NVGPU_SLEN_SUM', len_a=fl.sum_index,
                     len_elem=ln.elem)
        else:
            m.update(len_kind='NVGPU_SLEN_NVKMS_PARAMS')
        cb = f.copyback
        if isinstance(cb, Full):
            m.update(cb_kind='NVGPU_SCB_FULL')
        elif isinstance(cb, Partial):
            m.update(cb_kind='NVGPU_SCB_PARTIAL', cb_off=cb.off,
                     cb_width=cb.width, cb_arg=cb.elem)
        elif isinstance(cb, AllOrNothing):
            m.update(cb_kind='NVGPU_SCB_ALL_OR_NOTHING', cb_off=cb.off,
                     cb_width=cb.width, cb_arg=cb.elem)
        elif isinstance(cb, Exact):
            m.update(cb_kind='NVGPU_SCB_EXACT', cb_off=cb.off,
                     cb_width=cb.width)
        elif isinstance(cb, Range):
            m.update(cb_kind='NVGPU_SCB_RANGE', cb_off=cb.off, cb_arg=cb.len)
    elif isinstance(f, Array):
        m.update(kind='NVGPU_SF_ARRAY', stride=f.stride, count=f.count)
    elif isinstance(f, FdIn):
        m.update(kind='NVGPU_SF_FD_IN', width=f.width,
                 kinds=f'0x{f.kinds:x}u', none_value=f.none)
    elif isinstance(f, FdOut):
        m.update(kind='NVGPU_SF_FD_OUT', width=f.width)
    elif isinstance(f, GemIn):
        m.update(kind='NVGPU_SF_GEM_IN', width=4)
        if f.validate_nvkms:
            flags |= FF_VALIDATE
    elif isinstance(f, GemOut):
        m.update(kind='NVGPU_SF_GEM_OUT', width=4)
    if flags:
        m['flags'] = ' | '.join(n for b, n in ((FF_COND, 'NVGPU_SFF_COND'),
                                (FF_VALIDATE, 'NVGPU_SFF_VALIDATE_NVKMS'))
                                if flags & b)
    if fl.nchild:
        m.update(child=fl.child, nchild=fl.nchild)
    body = ', '.join(f'.{k} = {v}' for k, v in m.items() if v != 0)
    return f'  /* {index:3d} {fl.where} */\n  {{{body}}},'


def c_version(v):
    return f'NVGPU_SCHEMA_VERSION({v[0]}, {v[1]}, {v[2]})'


def c_sym(t):
    return f'nvgpu_schema_{t.name}'


def emit_c(tables, max_depth):
    out = [C_PREAMBLE.format(max_list=MAX_LIST, max_depth=max_depth,
                             nvkms_iowr=NVKMS_IOCTL_IOWR)]
    for t in tables:
        sym = c_sym(t)
        out.append(f'static const struct nvgpu_sfield {sym}_fields[] = {{')
        for i, fl in enumerate(t.flat):
            out.append(c_field(fl, i))
        if not t.flat:
            out.append('  {0},')
        out.append('};\n')
        out.append(f'static const struct nvgpu_sioctl {sym}_ioctls[] = {{')
        for (e, _), (first, n) in zip(t.entries, t.roots):
            flags = []
            if e.exec == EXECUTOR:
                flags.append('NVGPU_SIO_EXECUTOR')
            if e.cls == MODESET:
                flags.append('NVGPU_SIO_ARG_IN_ONLY')
            cls = {RENDER: 'NVGPU_SCLASS_RENDER', KMS: 'NVGPU_SCLASS_KMS',
                   MODESET: 'NVGPU_SCLASS_MODESET'}[e.cls]
            m = {'.name': f'"{e.name}"', '.cmd': f'0x{e.cmd:08x}u',
                 '.nvkms_cmd': getattr(e, 'nvkms_cmd', 0),
                 '.size': e.size, '.sclass': cls,
                 '.special': C_SPECIAL[e.special],
                 '.flags': ' | '.join(flags) or 0,
                 '.policy': f'0x{e.policy:x}u', '.field': first,
                 '.nfield': n}
            body = ', '.join(f'{k} = {v}' for k, v in m.items()
                             if v not in (0, '0x0u'))
            out.append(f'  {{{body}}},')
        out.append('};\n')
        vmin, vmax = (('0', '0') if t.versions is None else
                      (c_version(t.versions[0]), c_version(t.versions[1])))
        out.append(f'static const struct nvgpu_stable {sym} = {{\n'
                   f'  .name = "{t.name}", .vmin = {vmin}, .vmax = {vmax},\n'
                   f'  .ioctls = {sym}_ioctls, .nioctls = '
                   f'ARRAY_SIZE({sym}_ioctls),\n'
                   f'  .fields = {sym}_fields, .nfields = {len(t.flat)},\n'
                   '};\n')
    mods = [t for t in tables if t.versions is not None]
    drm = c_sym(tables[0])
    out.append('/* Set 0 has no NVKMS table; the rest one per range of host '
               'driver versions. */')
    out.append('static const struct nvgpu_schema_set nvgpu_schema_sets[] = {')
    out.append(f'  {{.drm = &{drm}}},')
    for t in mods:
        out.append(f'  {{.drm = &{drm}, .modeset = &{c_sym(t)}}},')
    out.append('};\n')
    out.append('#endif /* NVGPU_SCHEMA_TABLES */\n\n#endif /* NVGPU_SCHEMA_H */')
    return '\n'.join(out) + '\n'


# ────────────────────────────── Rust emission ──────────────────────────────

RS_PREAMBLE = '''\
// The IOCTL2 schema tables, for the backend's interpreter (device/src/xfer.rs).
// The guest's copy is driver/gen/nvgpu_schema.h; both are generated from
// gen/schema/*.py by gen/schema_gen.py. DO NOT EDIT -- regenerate.

use super::*;
use crate::version::DriverVersion;
'''

RS_DIR = {IN: 'Dir::In', OUT: 'Dir::Out', INOUT: 'Dir::InOut'}


def rs_field(fl, index):
    f = fl.f
    cond = 'None'
    if f.cond is not None:
        cond = (f'Some(Cond {{ off: {f.cond.off}, mask: 0x{f.cond.mask:x}, '
                f'value: 0x{f.cond.value:x} }})')
    span = f'Span {{ first: {fl.child}, len: {fl.nchild} }}'
    if isinstance(f, Ptr):
        ln = f.len
        if isinstance(ln, Const):
            lens = f'Len::Const({ln.n})'
        elif isinstance(ln, Count):
            lens = (f'Len::Count {{ off: {ln.off}, width: {ln.width}, '
                    f'elem: {ln.elem} }}')
        elif isinstance(ln, Sum):
            lens = f'Len::Sum {{ field: {fl.sum_index}, elem: {ln.elem} }}'
        else:
            lens = 'Len::NvkmsParams'
        cb = f.copyback
        if isinstance(cb, Full):
            cbs = 'CopyBack::Full'
        elif isinstance(cb, Partial):
            cbs = (f'CopyBack::Partial {{ off: {cb.off}, width: {cb.width}, '
                   f'elem: {cb.elem} }}')
        elif isinstance(cb, AllOrNothing):
            cbs = (f'CopyBack::AllOrNothing {{ off: {cb.off}, '
                   f'width: {cb.width}, elem: {cb.elem} }}')
        elif isinstance(cb, Exact):
            cbs = f'CopyBack::Exact {{ off: {cb.off}, width: {cb.width} }}'
        elif isinstance(cb, Range):
            cbs = f'CopyBack::Range {{ off: {cb.off}, len: {cb.len} }}'
        else:
            cbs = 'CopyBack::None'
        kind = (f'Kind::Ptr {{ dir: {RS_DIR[f.dir]}, len: {lens}, '
                f'copyback: {cbs}, max: {f.max}, stride: {f.stride}, '
                f'children: {span} }}')
    elif isinstance(f, Array):
        kind = (f'Kind::Array {{ count: {f.count}, stride: {f.stride}, '
                f'children: {span} }}')
    elif isinstance(f, FdIn):
        kind = (f'Kind::FdIn {{ width: {f.width}, kinds: 0x{f.kinds:x}, '
                f'none: {f.none} }}')
    elif isinstance(f, FdOut):
        kind = f'Kind::FdOut {{ width: {f.width} }}'
    elif isinstance(f, GemIn):
        kind = ('Kind::GemIn { validate_nvkms: '
                f'{"true" if f.validate_nvkms else "false"} }}')
    else:
        kind = 'Kind::GemOut'
    return (f'    // {index} {fl.where}\n'
            f'    Field {{ name: "{f.name}", off: {f.off}, cond: {cond}, '
            f'kind: {kind} }},')


def rs_version(v):
    return f'DriverVersion::new({v[0]}, {v[1]}, {v[2]})'


def emit_rs(tables):
    out = [RS_PREAMBLE]
    syms = []
    for t in tables:
        sym = t.name.upper()
        syms.append(sym)
        out.append(f'static {sym}_FIELDS: &[Field] = &[')
        for i, fl in enumerate(t.flat):
            out.append(rs_field(fl, i))
        out.append('];\n')
        out.append(f'static {sym}_IOCTLS: &[Ioctl] = &[')
        for (e, _), (first, n) in zip(t.entries, t.roots):
            cls = {RENDER: 'Class::Render', KMS: 'Class::Kms',
                   MODESET: 'Class::Modeset'}[e.cls]
            special = {SPECIAL_NONE: 'Special::None',
                       SPECIAL_ATOMIC: 'Special::Atomic',
                       SPECIAL_NVKMS_PARAMS: 'Special::NvkmsParams'}[e.special]
            ex = 'Exec::Executor' if e.exec == EXECUTOR else 'Exec::Inline'
            out.append(
                f'    Ioctl {{ name: "{e.name}", class: {cls}, '
                f'cmd: 0x{e.cmd:08x}, nvkms_cmd: {getattr(e, "nvkms_cmd", 0)}, '
                f'size: {e.size}, exec: {ex}, special: {special}, '
                f'policy: 0x{e.policy:x}, '
                f'arg_in_only: {"true" if e.cls == MODESET else "false"}, '
                f'fields: Span {{ first: {first}, len: {n} }} }},')
        out.append('];\n')
        vers = ('None' if t.versions is None else
                f'Some(({rs_version(t.versions[0])}, '
                f'{rs_version(t.versions[1])}))')
        out.append(f'static {sym}: Table = Table {{\n'
                   f'    name: "{t.name}",\n    versions: {vers},\n'
                   f'    ioctls: {sym}_IOCTLS,\n    fields: {sym}_FIELDS,\n'
                   '};\n')
    out.append('/// DRM core and nvidia-drm entries (classes Render and Kms), '
               'for any host version.')
    out.append(f'pub static DRM_TABLE: &Table = &{syms[0]};\n')
    out.append('/// NVKMS entries, one table per range of host driver '
               'versions.')
    out.append('pub static MODESET_TABLES: &[&Table] = &['
               + ', '.join(f'&{s}' for s in syms[1:]) + '];\n')
    out.append('/// (fourcc, planes) of every multi-planar format; the rest '
               'have one plane.')
    out.append('pub static MULTI_PLANE_FORMATS: &[(u32, u8)] = &[')
    for name, code, planes in formats.MULTI_PLANE:
        out.append(f'    (0x{code:08x}, {planes}), // {name}')
    out.append('];')
    return '\n'.join(out) + '\n'


def max_depth(tables):
    def depth(t, first, n):
        d = 0
        for fl in t.flat[first:first + n]:
            if fl.nchild:
                d = max(d, 1 + depth(t, fl.child, fl.nchild))
        return d
    return max(depth(t, a, b) for t in tables for a, b in t.roots) + 1


# ─────────────────────────────── probes ───────────────────────────────

def probe_lines(struct, fields, prefix=''):
    """offsetof asserts for a field list, recursing into element structs."""
    out = []
    for f in fields:
        if f.name and struct:
            path = prefix + f.name
            out.append(f'_Static_assert(offsetof({struct}, {path}) == '
                       f'{f.off}, "{struct}.{path}");')
        for ref in ('len', 'copyback'):
            r = getattr(f, ref, None)
            if struct and getattr(r, 'name', None) and hasattr(r, 'off'):
                out.append(f'_Static_assert(offsetof({struct}, '
                           f'{prefix}{r.name}) == {r.off}, '
                           f'"{struct}.{r.name}");')
                if hasattr(r, 'width'):
                    out.append(f'_Static_assert(sizeof((({struct} *)0)->'
                               f'{prefix}{r.name}) == {r.width}, '
                               f'"{struct}.{r.name} width");')
        if f.cond is not None and struct:
            out.append(f'_Static_assert(offsetof({struct}, {prefix}'
                       f'{f.cond.name}) == {f.cond.off}, "{struct}.cond");')
        if isinstance(f, (FdIn, FdOut)) and f.name and struct:
            out.append(f'_Static_assert(sizeof((({struct} *)0)->{prefix}'
                       f'{f.name}) == {f.width}, "{struct}.{f.name} width");')
        if isinstance(f, Ptr):
            if f.elem_struct:
                out.append(f'_Static_assert(sizeof({f.elem_struct}) == '
                           f'{f.stride}, "{f.elem_struct}");')
            out += probe_lines(f.elem_struct, f.fields)
        elif isinstance(f, Array):
            if struct and f.name:
                out.append(f'_Static_assert(sizeof((({struct} *)0)->'
                           f'{prefix}{f.name}) == {f.count * f.stride}, '
                           f'"{struct}.{f.name} size");')
            out += probe_lines(f.elem_struct, f.fields)
    return out


def emit_probes(outdir):
    drm = (drm_kms.IOCTLS + nvidia_drm.KMS_IOCTLS + drm_render.IOCTLS
           + nvidia_drm.RENDER_IOCTLS)
    lines = [
        '/* Generated by gen/schema_gen.py --probe: every layout the schema',
        ' * states, asserted against the real headers. It only has to compile.',
        ' */',
        '#include <stddef.h>',
        '#include <stdint.h>',
        '#include <drm/drm.h>',
        '#include <drm/drm_mode.h>',
        '#include <drm/drm_fourcc.h>',
        '#define NV_LINUX 1 /* else DRM_IO ioctls are defined as 0 */',
        '#include "nv_drm_common_ioctl.h"',
        '#include "nvkms-kapi-private.h"',
        '',
    ]
    for e in drm:
        if e.macro:
            lines.append(f'_Static_assert({e.macro} == 0x{e.cmd:08x}u, '
                         f'"{e.name} ioctl number");')
        if e.struct:
            lines.append(f'_Static_assert(sizeof({e.struct}) == {e.size}, '
                         f'"{e.name} size");')
        lines += probe_lines(e.struct, e.fields)
    for name, code, _ in formats.MULTI_PLANE:
        lines.append(f'_Static_assert({name} == 0x{code:08x}u, "{name}");')
    (outdir / 'probe_drm.c').write_text('\n'.join(lines) + '\n')

    for t in nvkms.TABLES:
        lines = ['/* Generated by gen/schema_gen.py --probe. */',
                 '#include <stddef.h>', '#include <sys/ioctl.h>']
        lines += [f'#include "{h}"' for h in t.probe_headers]
        lines.append(f'_Static_assert(NVKMS_IOCTL_IOWR == '
                     f'0x{NVKMS_IOCTL_IOWR:08x}u, "outer ioctl");')
        for m in t.ioctls:
            s = m.struct
            lines += [
                f'_Static_assert(NVKMS_IOCTL_{m.name} == {m.nvkms_cmd}, '
                f'"{m.name}");',
                f'_Static_assert(sizeof({s}) == {m.size}, "{m.name} size");',
                f'_Static_assert(offsetof({s}, reply) == {m.reply_off}, '
                f'"{m.name} reply");',
                f'_Static_assert(sizeof((({s} *)0)->reply) == {m.reply_len}, '
                f'"{m.name} reply size");',
            ]
            lines += probe_lines(s, m.fields)
        (outdir / f'probe_{t.name}.c').write_text('\n'.join(lines) + '\n')


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[1])
    ap.add_argument('--out', type=Path,
                    help='write under this directory instead of the repo')
    ap.add_argument('--probe', type=Path,
                    help='write offset probes (C) into this directory')
    args = ap.parse_args()
    try:
        tables = build()
    except SchemaError as e:
        print(f'schema_gen.py: {e}', file=sys.stderr)
        return 1
    if args.probe:
        args.probe.mkdir(parents=True, exist_ok=True)
        emit_probes(args.probe)
        return 0
    root = args.out if args.out else REPO
    for rel, text in ((C_OUT, emit_c(tables, max_depth(tables))),
                      (RS_OUT, emit_rs(tables))):
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(path.suffix + '.tmp')
        tmp.write_text(text)
        os.replace(tmp, path)
    return 0


if __name__ == '__main__':
    sys.exit(main())
