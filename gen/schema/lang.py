"""The ioctl schema language both IOCTL2 interpreters are generated from.

An entry describes one ioctl completely enough for a party that trusts none
of the caller's layout to run it: the argument's size, every pointer in it
(and in what those point at) with the rule for its length and for what comes
back, and every descriptor and GEM handle, wherever it sits. The guest walks
an entry to gather a caller's memory; the backend walks its own copy of the
same entry over the bytes it received and refuses anything that disagrees.
Both walks are the *canonical traversal*:

  buffer 0 is the ioctl argument; then, depth first and in field order, every
  PTR field whose value is non-zero and whose length is non-zero gets the next
  buffer, and the fields of what it points at (element by element) are walked
  before the next field of its parent. ARRAY fields are inline and are walked
  element by element in place -- only as many elements as the array's
  `limit` says the kernel reads. A NULL or zero-length pointer contributes no
  buffer, and the backend hands the host NULL for it.

Lengths are read from the IN bytes of the enclosing buffer, which are also
the bytes the host kernel will read, so the length the backend allocates is
the length the kernel uses: there is no second copy of a count for a guest to
make disagree.

Everything here is data; schema_gen.py checks it (offsets inside the struct,
no overlaps, counts readable, copy-back counts writable) and emits the same
tables for C (driver/gen/nvgpu_schema.h) and Rust (gen/src/schema/).
"""

from dataclasses import dataclass, field
from typing import Optional

# ── Directions (of a buffer) ──
IN, OUT, INOUT = 1, 2, 3

# ── ioctl direction bits, as in <asm-generic/ioctl.h> ──
IOC_NONE, IOC_WRITE, IOC_READ = 0, 1, 2
IOW, IOR, IOWR = IOC_WRITE, IOC_READ, IOC_WRITE | IOC_READ

# ── Schema classes: which kind of host file a call targets ──
RENDER, KMS, MODESET = 1, 2, 3

# ── Where the backend runs a call ──
INLINE, EXECUTOR = 0, 1

# ── Descriptor kinds (FdIn.kinds). Bit n = protocol HK_* value n; the Dev
# kind is split further, because "a descriptor of one of our devices" is far
# too wide for a field that must be nvidiactl or a fresh nvidia-modeset. ──
HK_DEV, HK_DRI_RENDER, HK_DRM_CARD, HK_DRM_LEASE = 1, 2, 3, 4
HK_SYNC_FILE, HK_SYNCOBJ, HK_DMABUF, HK_EVENTFD, HK_MEMFD = 5, 6, 7, 8, 9
K_ANY_DEV = 1 << HK_DEV
K_SYNC_FILE = 1 << HK_SYNC_FILE
K_SYNCOBJ = 1 << HK_SYNCOBJ
K_DMABUF = 1 << HK_DMABUF
K_EVENTFD = 1 << HK_EVENTFD
K_DEV_CTL = 1 << 16
K_DEV_MODESET = 1 << 17
K_DEV_GPU = 1 << 18

# ── Specials: code on both sides, named here ──
SPECIAL_NONE = 0
# DRM ATOMIC: props/prop_values are SUM_ARRAY lengths (generic); fence and
# pointer properties inside them are the special's business.
SPECIAL_ATOMIC = 1
# NVKMS: buffer 0 is the 16-byte NvKmsIoctlParams; the entry is keyed by the
# command inside it and describes the params block its pointer reaches.
SPECIAL_NVKMS_PARAMS = 2

# ── Policies: backend-only checks an entry opts into (a bitmask) ──
POL_FENCE = 1 << 0          # fence schemas; the FENCES hook decides
POL_GRANT = 1 << 1          # nvidia-drm GRANT_PERMISSIONS
POL_REVOKE = 1 << 2         # nvidia-drm REVOKE_PERMISSIONS
POL_FB_CREATE = 1 << 3      # records the new fb_id (u32 @0) as the file's own
POL_FB_REMOVE = 1 << 4      # forgets the fb_id (u32 @0)
POL_FB_READ = 1 << 5        # GEM_OUT handles only for the file's own FBs
POL_SETPROP = 1 << 6        # legacy property set: no fd/pointer properties
POL_FB_PLANES = 1 << 7      # ADDFB2: plane count from pixel_format
POL_NVKMS = 1 << 8          # NVKMS command; the NVKMS hook decides
# NVKMS: the command's layout differs in the next release we measured, so a
# host between the two may have either; run it only on a host of exactly the
# table's release (the NVKMS hook checks).
POL_NVKMS_EXACT = 1 << 9
POL_MASTER = 1 << 10        # SET/DROP_MASTER: only on a host card, never a lease


def ioc(d, t, nr, size):
    """_IOC() from <asm-generic/ioctl.h>."""
    assert size < (1 << 14)
    return (d << 30) | (size << 16) | (ord(t) << 8) | nr


# ── Conditions ──

@dataclass
class Cond:
    """A field that exists only when (u32 at `off` & mask) == value, or with
    `ne`, != value: NVKMS's NvBool flags are one byte the kernel tests for
    non-zero (`if (pRequest->useFd)`), so a caller's 2 must count as true
    here too, or a descriptor the kernel reads would cross untranslated.

    `off` is in the same struct as the field; `name` is the C member it
    reads, for the offset probe."""
    name: str
    off: int
    mask: int
    value: int
    ne: bool = False

    def holds(self, v):
        return ((v & self.mask) != self.value) if self.ne else \
            ((v & self.mask) == self.value)


# ── Length rules ──

@dataclass
class Const:
    """A fixed number of bytes. Const(0) is a pointer the host must never
    see: it is always sent as NULL (e.g. a field the kernel ignores)."""
    n: int


@dataclass
class Count:
    """count × elem bytes, count being the `width`-byte unsigned field at
    `off` of the enclosing struct."""
    name: str
    off: int
    width: int
    elem: int


def Field64(name, off):
    """A byte length held in a u64 (nvidia-drm's nvkms_params_size)."""
    return Count(name, off, 8, 1)


@dataclass
class Sum:
    """Σ of the u32 elements of what sibling PTR `ptr` points at, × elem.
    The sibling must come first in field order (it is walked first)."""
    ptr: str
    elem: int


@dataclass
class NvkmsParamsLen:
    """The NVKMS outer struct's `size` (u32 @4), which must equal the params
    size of the entry the outer `cmd` selects."""


# ── Copy-back rules (what of an OUT buffer reaches the caller) ──
#
# The backend always returns every OUT byte; the rule is the guest's, and
# mirrors what the kernel would have written into the caller's memory.
# Counts are the pointer's sibling field: `in` is the value the caller sent,
# `out` the value the host left there.

@dataclass
class NoCopy:
    """Nothing: the kernel never writes this memory."""


@dataclass
class Full:
    """All of it. An OUT-only buffer only on success (on failure the kernel
    wrote nothing and our zeroes are not the caller's bytes); an IN/OUT buffer
    always, since unwritten bytes are the caller's own."""


@dataclass
class Partial:
    """On success, min(in, out) elements: "we fill what fits and tell you
    how many there are"."""
    name: str
    off: int
    width: int
    elem: int


@dataclass
class AllOrNothing:
    """On success, `out` elements if out <= in, else nothing."""
    name: str
    off: int
    width: int
    elem: int


@dataclass
class Exact:
    """On success, all of it if out == in, else nothing (GETPROPBLOB)."""
    name: str
    off: int
    width: int


@dataclass
class Written:
    """Bytes [0, n), always, n being the `width`-byte field at `off` of the
    pointer's struct as the host left it: NVKMS's pInfoString, which
    InfoStringDoneUserCommon copies out infoStringLenWritten bytes of
    whether or not the command succeeded (nvkms.c:1920-1945, 5220-5229)."""
    name: str
    off: int
    width: int


@dataclass
class Range:
    """Bytes [off, off+len), always: NVKMS copies its reply half out even
    when the command failed."""
    off: int
    len: int


# ── Fields ──

@dataclass
class Ptr:
    name: str
    off: int
    dir: int
    len: object
    copyback: object = None
    max: int = 0
    # What it points at: element struct (for the probe) and stride, and the
    # fields of one element. A pointer with fields must hold whole elements.
    elem_struct: Optional[str] = None
    stride: int = 0
    fields: list = field(default_factory=list)
    cond: Optional[Cond] = None


@dataclass
class ArrayCount:
    """Only the first min(count, n) elements are the kernel's, n being the
    `width`-byte field at `off` of the enclosing struct (JOIN_SWAP_GROUP's
    numMembers)."""
    name: str
    off: int
    width: int


@dataclass
class ArrayPlanes:
    """Only the first numPlanes(format) elements are the kernel's, format
    being the u32 at `off` of the enclosing struct and numPlanes the
    table's own format list (REGISTER_SURFACE's planes, nvkms-surface.c
    reads [0, numPlanes)); a format the table does not know has none."""
    name: str
    off: int


@dataclass
class Array:
    """An inline array of structs inside the enclosing buffer. Every element
    is walked unless `limit` (ArrayCount / ArrayPlanes) says fewer are the
    kernel's: a descriptor in an element the kernel never reads must not
    have to be one."""
    name: str
    off: int
    count: int
    stride: int
    elem_struct: Optional[str] = None
    fields: list = field(default_factory=list)
    cond: Optional[Cond] = None
    limit: object = None


@dataclass
class FdIn:
    """A descriptor the caller passes. It crosses as a backend handle of one
    of `kinds`, or holds `none` and crosses as it is."""
    name: str
    off: int
    width: int
    kinds: int
    none: int = -1
    cond: Optional[Cond] = None


@dataclass
class FdOut:
    """A descriptor the host creates, adopted by the backend as a handle."""
    name: str
    off: int
    width: int = 4
    cond: Optional[Cond] = None


@dataclass
class GemIn:
    """A GEM handle the caller passes; crosses as the proxy's (owner, gem)
    and is re-homed into the target file by the backend. validate_nvkms: the
    backend requires GEM_IDENTIFY_OBJECT == NVKMS for it, in the same job as
    the ioctl (nvidia-drm-fb.c dereferences pMemory without a check)."""
    name: str
    off: int
    validate_nvkms: bool = False
    cond: Optional[Cond] = None


@dataclass
class GemOut:
    """A GEM handle the host creates; the backend re-homes it into the
    calling file's render handle."""
    name: str
    off: int
    cond: Optional[Cond] = None


# ── Entries ──

@dataclass
class Ioctl:
    name: str
    cls: int
    macro: str       # the C macro, asserted equal to `cmd` by the probe
    struct: Optional[str]
    size: int
    nr: int
    dir: int
    type: str = 'd'
    exec: int = INLINE
    fields: list = field(default_factory=list)
    special: int = SPECIAL_NONE
    policy: int = 0
    doc: str = ''

    @property
    def cmd(self):
        return ioc(self.dir, self.type, self.nr, self.size)


@dataclass
class NvkmsIoctl:
    """One NVKMS command of one driver-version table. `size` is the params
    struct's size, which the outer struct's `size` must equal; the reply half
    [reply_off, reply_off + reply_len) is what is copied back."""
    name: str
    nvkms_cmd: int
    struct: str
    size: int
    reply_off: int
    reply_len: int
    fields: list = field(default_factory=list)
    policy: int = POL_NVKMS
    doc: str = ''


@dataclass
class ModesetTable:
    """The NVKMS commands of the host driver versions [vmin, vmax]."""
    name: str
    vmin: tuple
    vmax: tuple
    ioctls: list
    # Include path (relative to an open-gpu-kernel-modules tree) for the probe.
    probe_includes: list = field(default_factory=list)
    probe_headers: list = field(default_factory=list)
    # numPlanes by NvKmsSurfaceMemoryFormat value (ArrayPlanes).
    planes: list = field(default_factory=list)
    # What the backend's NVKMS policy reads and rewrites, for the Rust
    # tables only: {field name: value}, see NvkmsLayout in gen/src/schema.
    layout: Optional[dict] = None
