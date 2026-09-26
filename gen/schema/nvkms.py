# SPDX-License-Identifier: Apache-2.0
"""NVKMS commands (class MODESET), one table per host driver release.

The ioctl itself is always NVKMS_IOCTL_IOWR, _IOWR('m', 0, 16), whose
argument is struct NvKmsIoctlParams {u32 cmd; u32 size; u64 address}
(kernel-open/nvidia-modeset/nvkms-ioctl.h:38). An entry here is keyed by the
command inside, describes the params block `address` points at, and requires
`size` to equal that block's size (nvKmsIoctl refuses anything else,
src/nvidia-modeset/src/nvkms.c:5183). NVKMS copies in the request half and
out the reply half, the latter even when the command failed (5220-5243),
which is the RANGE copy-back.

Nothing here is measured by hand. Every table is converted from
gen/nvkms/<release>.json, which gen/nvkms_extract.py compiles out of that
release's own headers and dispatch table (see gen/nvkms/README.md for the
format). Command numbers are never carried from one release to another --
REGISTER_SURFACE is 16 in 535 and 615 and 17 in between -- so each release
gets its own table, keyed by (release, command).

Which table a host gets (the profile rule): the one of the newest release we
measured that is not newer than the host's; a host newer than every release
gets the last. That guesses for a host we never measured, which is why the
guess is fenced twice. NVKMS itself refuses a params size it does not expect
and so do both interpreters (Len::NvkmsParams), so a command whose struct
grew fails closed. And a command whose layout differs between a table's
release and the next one we measured -- the only ones an in-between release
could have either way -- carries POL_NVKMS_EXACT, and the backend runs it
only on a host of exactly the table's release.

What is left out of every table, and so refused by both halves:
  - commands with no dispatch entry (35/36, the 3D Vision ones);
  - commands that carry a kernel pointer in the request
    (REGISTER/UNREGISTER_VBLANK_INTR_CALLBACK, kernel clients only,
    nvkms.c:4967, 5012) and FRAMEBUFFER_CONSOLE_DISABLED (kernel clients
    only, 4941): numbered 63-65 in 610, but never refused by number, since
    63 is ACCEL_VBLANK_SEM_CONTROLS in 580;
  - EXPORT_VRR_SEMAPHORE_SURFACE and VRR_SIGNAL_SEMAPHORE (535, 580): the
    first exports NVKMS's device-global VRR semaphore memory into an
    nvidiactl descriptor, the second lets any open signal it
    (580 nvkms.c:4350, 4970) -- both reach the host compositor's state with
    no permission check.
Tegra syncpoint descriptors (FLIP/SET_MODE layers' pre and post fences) are
left out of the layouts: a dGPU host refuses useSyncpt
(nvkms-hw-flip.c:716-719), and the backend refuses it before the host does,
so no descriptor ever sits there. (Both read useSyncpt only in a layer whose
syncObjects.specified is set, which is why the layout carries that byte too.)
"""

import json
from pathlib import Path

from .lang import *

DATA = Path(__file__).resolve().parent.parent / 'nvkms'

# Commands no table has; see above.
REFUSED = {
    'FRAMEBUFFER_CONSOLE_DISABLED',
    'REGISTER_VBLANK_INTR_CALLBACK',
    'UNREGISTER_VBLANK_INTR_CALLBACK',
    'EXPORT_VRR_SEMAPHORE_SURFACE',
    'VRR_SIGNAL_SEMAPHORE',
}

# fd_kinds (gen/nvkms/README.md) -> FdIn kinds. The modeset kinds all cross
# as a /dev/nvidia-modeset handle; which state that file must be in (never
# ioctl'd, a grant, a unicast event) is the backend's to track, because a
# kind bit cannot see it.
FD_KINDS = {
    'modeset_fresh': K_DEV_MODESET,
    'modeset_unicast': K_DEV_MODESET,
    'modeset_grant_surface': K_DEV_MODESET,
    'modeset_grant_permissions': K_DEV_MODESET,
    'modeset_grant_swap_group': K_DEV_MODESET,
    'nvidiactl_exported': K_DEV_CTL,
    'dmabuf': K_DMABUF,
}

# Event types a guest may declare interest in (ARCHITECTURE.md §13): what a display
# client needs, and nothing a flood of which it could not drain.
EVENTS_ALLOWED = [
    'NVKMS_EVENT_TYPE_DPY_CHANGED',
    'NVKMS_EVENT_TYPE_DYNAMIC_DPY_CONNECTED',
    'NVKMS_EVENT_TYPE_DYNAMIC_DPY_DISCONNECTED',
    'NVKMS_EVENT_TYPE_FLIP_OCCURRED',
]

PROBE_HEADERS = ['nvkms-api.h', 'nvkms-ioctl.h']
PROBE_INCLUDES = [
    'src/nvidia-modeset/interface',
    'src/common/unix/common/inc',
    'src/common/inc',
    'src/common/sdk/nvidia/inc',
]


class DataError(Exception):
    pass


def version_of(text):
    a, b, c = (int(x) for x in text.split('.'))
    return (a, b, c)


def before(v):
    """The release just before `v` in NVGPU_SCHEMA_VERSION's encoding
    (minor and patch below 1000)."""
    a, b, c = v
    if c:
        return (a, b, c - 1)
    if b:
        return (a, b - 1, 999)
    return (a - 1, 999, 999)


LAST = (999, 999, 999)


def where(cmd, path):
    return f'{cmd}: {path}'


def mask_of(size):
    return (1 << (8 * size)) - 1


def cond_of(cmd, refs):
    """A JSON cond (all refs must hold) as one Cond; more than one is not a
    layout any table we keep needs."""
    if not refs:
        return None
    if len(refs) != 1:
        raise DataError(where(cmd, f'{len(refs)} conditions on one field'))
    r = refs[0]
    if r['size'] not in (1, 2, 4):
        raise DataError(where(cmd, f'condition on a {r["size"]}-byte field'))
    if r['op'] == 'nonzero':
        return Cond(r['path'], r['off'], mask_of(r['size']), 0, ne=True)
    if r['op'] == 'eq':
        return Cond(r['path'], r['off'], mask_of(r['size']), r['value'])
    raise DataError(where(cmd, f'condition op {r["op"]!r}'))


def is_syncpt(node):
    return 'syncpts' in node['path'] or 'postSyncpt' in node['path']


def convert(cmd, nodes, struct):
    """JSON nodes -> schema fields, in offset order. Plain fields are the
    policy's (layout below), not the interpreters'."""
    out = []
    for n in nodes:
        k = n['kind']
        if k == 'field':
            continue
        if k in ('fd_in', 'fd_out') and is_syncpt(n):
            continue
        if k == 'ptr':
            out.append(convert_ptr(cmd, n))
        elif k == 'fd_in':
            kinds = 0
            for fk in n['fd_kinds']:
                if fk not in FD_KINDS:
                    raise DataError(where(cmd, f'fd kind {fk!r}'))
                kinds |= FD_KINDS[fk]
            out.append(FdIn(n['path'], n['off'], n['size'], kinds,
                            cond=cond_of(cmd, n.get('cond', []))))
        elif k == 'array':
            elem = (f'__typeof__((({struct} *)0)->{n["path"]}[0])'
                    if struct else None)
            kids = convert(cmd, n['fields'], elem)
            if not kids:
                continue
            valid = n['valid']
            cond, limit = None, None
            if valid['rule'] == 'count':
                c = valid['count']
                limit = ArrayCount(c['path'], c['off'], c['size'])
            elif valid['rule'] == 'format_num_planes':
                limit = ArrayPlanes(valid['format']['path'],
                                    valid['format']['off'])
                cond = cond_of(cmd, valid.get('when', []))
            elif valid['rule'] != 'all':
                raise DataError(where(cmd, f'array rule {valid["rule"]!r}'))
            out.append(Array(n['path'], n['off'], n['count'], n['stride'],
                             elem_struct=elem, fields=kids, cond=cond,
                             limit=limit))
        elif k == 'kernel_ptr' and n['path'].startswith('reply.event.'):
            # 615's DPY_CP_TOPOLOGY_CHANGED event carries one; NVKMS never
            # queues that event for a user client, and the policy never lets
            # a guest declare interest in it.
            continue
        else:
            raise DataError(where(cmd, f'{k} at {n["path"]} in a kept command'))
    return sorted(out, key=lambda f: f.off)


def convert_ptr(cmd, n):
    ln, elem = n['len'], n.get('elem')
    d = {'in': IN, 'out': OUT}[n['dir']]
    written = None
    if ln['rule'] == 'const':
        length, cap = Const(ln['bytes']), ln['bytes']
    elif ln['rule'] == 'count':
        c = ln['count']
        most = ln['max'] if ln['max'] is not None else mask_of(c['size'])
        length = Count(c['path'], c['off'], c['size'], ln['elem_size'])
        cap = most * ln['elem_size']
    elif ln['rule'] == 'bytes':
        s = ln['size']
        length, cap = Count(s['path'], s['off'], s['size'], 1), ln['max']
        written = ln.get('written')
    else:
        raise DataError(where(cmd, f'length rule {ln["rule"]!r}'))
    if d == IN:
        cb = NoCopy()
    elif written:
        cb = Written(written['path'], written['off'], written['size'])
    else:
        cb = Full()
    etype = elem['type'] if elem else None
    stride = elem['size'] if elem else 0
    kids = convert(cmd, elem.get('fields', []), etype) if elem else []
    return Ptr(n['path'], n['off'], d, length, cb, max=cap,
               elem_struct=etype, stride=stride, fields=kids)


def refused(c):
    if not c['dispatch'] or c['name'] in REFUSED:
        return True

    def has(nodes):
        for n in nodes:
            if n['kind'] == 'user_va':
                return True
            if n['kind'] == 'kernel_ptr' and not n['path'].startswith('reply.event.'):
                return True
            if has(n.get('fields', [])) or has(n.get('elem', {}).get('fields', [])):
                return True
        return False
    return has(c.get('fields', []))


# ───────────────────────────── policy layout ─────────────────────────────

def find(cmd, nodes, path):
    for n in nodes:
        if n['path'] == path:
            return n
    raise DataError(where(cmd, f'no node {path!r}'))


def fields_of(c):
    return c.get('fields', [])


def arr(n):
    return {'off': n['off'], 'count': n['count'], 'stride': n['stride']}


def target(c, what):
    f = fields_of(c)
    return {'device': find(c['name'], f, 'request.deviceHandle')['off'],
            'disp': find(c['name'], f, 'request.dispHandle')['off'],
            'what': find(c['name'], f, what)['off']}


def ranges(c, pick):
    """The byte ranges of the plain fields `pick` selects, adjacent ones
    merged."""
    spans = []

    def walk(nodes, base):
        for n in nodes:
            if n['kind'] == 'array':
                for i in range(n['count']):
                    walk(n['fields'], base + n['off'] + i * n['stride'])
            elif n['kind'] == 'field' and pick(n):
                spans.append([base + n['off'], n['size']])
    walk(fields_of(c), 0)
    spans.sort()
    out = []
    for off, size in spans:
        if out and out[-1][0] + out[-1][1] == off:
            out[-1][1] += size
        else:
            out.append([off, size])
    return [tuple(s) for s in out]


def perms(c, half):
    """NvKmsPermissions in `half` ('request' or 'reply'): per head from 595,
    per (disp, head) before."""
    f = fields_of(c)
    name = c['name']
    out = {'ptype': find(name, f, f'{half}.permissions.type')['off']}
    for kind, leaf in (('flip', 'layerMask'), ('modeset', 'dpyIdList')):
        per_head = [n for n in f if n['path'] == f'{half}.permissions.{kind}.head']
        if per_head:
            h = per_head[0]
            find(name, h['fields'], leaf)
            out[kind] = {'disp': None, 'head': arr(h)}
        else:
            dsp = find(name, f, f'{half}.permissions.{kind}.disp')
            h = find(name, dsp['fields'], 'head')
            find(name, h['fields'], leaf)
            out[kind] = {'disp': arr(dsp), 'head': arr(h)}
    return out


def layout(d):
    cmds = {c['name']: c for c in d['nvkms']['commands'] if c['dispatch']}
    ev = d['nvkms']['event_types']
    g = lambda n: cmds[n]  # noqa: E731

    alloc = g('ALLOC_DEVICE')
    af = fields_of(alloc)
    dyn = g('QUERY_DPY_DYNAMIC_DATA')
    dynf = fields_of(dyn)
    # The GPU an open's device is: deviceId (535) or deviceId.rmDeviceId
    # (a struct from 580 on), its first member either way.
    alloc_id = next((n for n in af if n['path'] in
                     ('request.deviceId', 'request.deviceId.rmDeviceId')), None)
    if alloc_id is None:
        raise DataError(where('ALLOC_DEVICE', 'no request.deviceId'))
    dyn_disp = find('QUERY_DPY_DYNAMIC_DATA', dynf, 'request.dispHandle')['off']
    # The extractor lists no deviceHandle for this request; it is the
    # struct's first member, as in every NVKMS request that names a device,
    # and dispHandle follows it.
    if dyn_disp != 4:
        raise DataError(where('QUERY_DPY_DYNAMIC_DATA', 'dispHandle is not at 4'))
    nev = g('GET_NEXT_EVENT')

    lp = g('SET_LAYER_POSITION')
    lpf = fields_of(lp)
    lpd = find('SET_LAYER_POSITION', lpf, 'request.disp')

    flip = g('FLIP')
    ff = fields_of(flip)
    heads = find('FLIP', ff, 'request.pFlipHead')['elem']
    flayer = find('FLIP', heads['fields'], 'flip.layer')

    sm = g('SET_MODE')
    smf = fields_of(sm)
    smd = find('SET_MODE', smf, 'request.disp')
    smh = find('SET_MODE', smd['fields'], 'head')
    sml = find('SET_MODE', smh['fields'], 'flip.layer')

    drm = {i['name']: i for i in d['nvidia_drm']['ioctls']}
    return {
        # Device-wide knobs NVKMS applies when this open creates the device
        # (nvkms-evo.c:9043-9050 registry keys, nvkms.c:1417 console
        # hotplugs, no3d / SLI mosaic at creation); versionString must
        # reach the host as sent (nvkms.c:1382).
        'alloc_scrub': ranges(alloc, lambda n: n['role'] == 'policy'
                              and not n['path'].startswith('reply.')
                              and n['path'] != 'request.versionString'),
        'alloc_reply_device': find('ALLOC_DEVICE', af, 'reply.deviceHandle')['off'],
        'alloc_reply_disps': find('ALLOC_DEVICE', af, 'reply.dispHandles')['off'],
        # isoIOCoherencyModes then nisoIOCoherencyModes ({NvBool coherent;
        # NvBool noncoherent;} each), then displayIsGpuL2Coherent, then
        # supportsSyncpts: five bytes of NvBool before supportsSyncpts in
        # every release profiled, 535 through 615 (nvkms-api.h, e.g. 610
        # :1232-1246), so no padding can fall between them.
        'alloc_reply_coherency': find('ALLOC_DEVICE', af, 'reply.supportsSyncpts')['off'] - 5,
        # Every override flag and the EDID: nvDpyGetDynamicData stores them
        # in the dpy, which outlives the call (nvkms-dpy.c:3055-3160).
        'dpy_dynamic_scrub': ranges(dyn, lambda n: n['role'] == 'policy'),
        # Which dpy a QUERY_DPY_DYNAMIC_DATA probes, and its reply half: what
        # the backend answers a repeated probe with (S-8).
        'dpy_dynamic': {'device': 0, 'disp': dyn_disp,
                        'what': find('QUERY_DPY_DYNAMIC_DATA', dynf, 'request.dpyId')['off']},
        'dpy_dynamic_reply': (dyn['reply']['offset'], dyn['reply']['size']),
        'alloc_device_id': alloc_id['off'],
        'set_cursor_image': target(g('SET_CURSOR_IMAGE'), 'request.head'),
        'move_cursor': target(g('MOVE_CURSOR'), 'request.head'),
        'set_lut': target(g('SET_LUT'), 'request.head'),
        'set_dpy_attribute': target(g('SET_DPY_ATTRIBUTE'), 'request.dpyId'),
        'layer_position': {
            'device': find('SET_LAYER_POSITION', lpf, 'request.deviceHandle')['off'],
            'disps': find('SET_LAYER_POSITION', lpf, 'request.requestedDispsBitMask')['off'],
            'disp': arr(lpd),
            'heads': find('SET_LAYER_POSITION', lpd['fields'], 'requestedHeadsBitMask')['off'],
            'head': arr(find('SET_LAYER_POSITION', lpd['fields'], 'head'))},
        # FLIP: which (sd, head) each pFlipHead element acts on -- NVKMS
        # checks permission only for the layers a flip dirties
        # (nvkms-flip.c nvCheckFlipPermissions), so a cursor-, HDR- or
        # colorimetry-only element passes it on any head -- and per layer
        # the bytes the policy vets: useSyncpt (read only where
        # syncObjects.specified is set, nvkms-hw-flip.c:714-718) and
        # completionNotifier.awaken (a FLIP_OCCURRED broadcast).
        'flip': {
            'device': find('FLIP', ff, 'request.deviceHandle')['off'],
            'ptr': find('FLIP', ff, 'request.pFlipHead')['off'],
            'heads': find('FLIP', ff, 'request.numFlipHeads')['off'],
            'head_size': heads['size'],
            'sd': find('FLIP', heads['fields'], 'sd')['off'],
            'head': find('FLIP', heads['fields'], 'head')['off'],
            'layer': arr(flayer),
            'use_syncpt': find('FLIP', flayer['fields'], 'syncObjects.val.useSyncpt')['off'],
            'sync_specified': find('FLIP', flayer['fields'], 'syncObjects.specified')['off'],
            'awaken': find('FLIP', flayer['fields'], 'completionNotifier.val.awaken')['off']},
        # SET_MODE: the heads a committed request touches and the dpys it
        # puts on them, which NVKMS's ValidateRequest checks against the
        # file's modeset permissions (nvkms-modeset.c:3940-3966), plus the
        # same per-layer bytes as FLIP.
        'set_mode': {
            'device': find('SET_MODE', smf, 'request.deviceHandle')['off'],
            'commit': find('SET_MODE', smf, 'request.commit')['off'],
            'disps': find('SET_MODE', smf, 'request.requestedDispsBitMask')['off'],
            'disp': arr(smd),
            'heads': find('SET_MODE', smd['fields'], 'requestedHeadsBitMask')['off'],
            'head': arr(smh),
            'dpys': find('SET_MODE', smh['fields'], 'dpyIdList')['off'],
            'layer': arr(sml),
            'use_syncpt': find('SET_MODE', sml['fields'], 'syncObjects.val.useSyncpt')['off'],
            'sync_specified': find('SET_MODE', sml['fields'], 'syncObjects.specified')['off'],
            'awaken': find('SET_MODE', sml['fields'], 'completionNotifier.val.awaken')['off']},
        'grant': dict(perms(g('GRANT_PERMISSIONS'), 'request'),
                      device=find('GRANT_PERMISSIONS', fields_of(g('GRANT_PERMISSIONS')),
                                  'request.deviceHandle')['off']),
        'acquire': dict(perms(g('ACQUIRE_PERMISSIONS'), 'reply'),
                        device=find('ACQUIRE_PERMISSIONS', fields_of(g('ACQUIRE_PERMISSIONS')),
                                    'reply.deviceHandle')['off']),
        'revoke': dict(perms(g('REVOKE_PERMISSIONS'), 'request'),
                       device=find('REVOKE_PERMISSIONS', fields_of(g('REVOKE_PERMISSIONS')),
                                   'request.deviceHandle')['off']),
        'event_interest': find('DECLARE_EVENT_INTEREST', fields_of(g('DECLARE_EVENT_INTEREST')),
                               'request.interestMask')['off'],
        'events_allowed': sum(1 << ev[e] for e in EVENTS_ALLOWED),
        'next_event_valid': find('GET_NEXT_EVENT', fields_of(g('GET_NEXT_EVENT')),
                                 'reply.valid')['off'],
        # reply.event.eventType, the event's first member, and the events
        # after which a dpy's dynamic data may have changed.
        'next_event_type': find('GET_NEXT_EVENT', fields_of(nev), 'reply.event')['off'],
        'dpy_events': sum(1 << ev[e] for e in (
            'NVKMS_EVENT_TYPE_DPY_CHANGED',
            'NVKMS_EVENT_TYPE_DYNAMIC_DPY_CONNECTED',
            'NVKMS_EVENT_TYPE_DYNAMIC_DPY_DISCONNECTED')),
        'drm_grant_typed': drm['GRANT_PERMISSIONS']['size'] == 12,
    }


# ───────────────────────────── the tables ─────────────────────────────

def fingerprint(c):
    """Everything about a command a table depends on: its number, size,
    halves and every node, notes aside."""
    def strip(x):
        if isinstance(x, dict):
            return {k: strip(v) for k, v in x.items() if k != 'note'}
        if isinstance(x, list):
            return [strip(v) for v in x]
        return x
    return json.dumps(strip(c), sort_keys=True)


def load():
    releases = []
    for p in sorted(DATA.glob('*.json'), key=lambda p: version_of(p.stem)):
        d = json.loads(p.read_text())
        if d['driver_version'] != p.stem:
            raise DataError(f'{p.name}: driver_version {d["driver_version"]!r}')
        releases.append(d)
    return releases


def tables():
    releases = load()
    out = []
    for i, d in enumerate(releases):
        v = version_of(d['driver_version'])
        nxt = releases[i + 1] if i + 1 < len(releases) else None
        vmax = before(version_of(nxt['driver_version'])) if nxt else LAST
        later = ({c['name']: fingerprint(c) for c in nxt['nvkms']['commands']}
                 if nxt else None)
        ioctls = []
        for c in d['nvkms']['commands']:
            if refused(c):
                continue
            params = c['params']
            pol = POL_NVKMS
            if later is not None and later.get(c['name']) != fingerprint(c):
                pol |= POL_NVKMS_EXACT
            ioctls.append(NvkmsIoctl(
                c['name'], c['nr'], params, c['size'],
                reply_off=c['reply']['offset'], reply_len=c['reply']['size'],
                fields=convert(c['name'], c.get('fields', []), params),
                policy=pol))
        fmts = d['nvkms']['surface_formats']
        planes = [0] * (max(f['value'] for f in fmts) + 1)
        for f in fmts:
            planes[f['value']] = f['num_planes']
        name = 'v' + d['driver_version'].replace('.', '_')
        out.append(ModesetTable(
            name, v, vmax, ioctls,
            probe_includes=PROBE_INCLUDES, probe_headers=PROBE_HEADERS,
            planes=planes, layout=layout(d)))
    return out


TABLES = tables()
