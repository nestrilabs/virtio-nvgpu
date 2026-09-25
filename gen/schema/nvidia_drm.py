"""nvidia-drm's own ioctls (DRM_COMMAND_BASE + n), 610.57.04.

Layouts from kernel-open/nvidia-drm/nv_drm_common_ioctl.h, handlers from
nvidia-drm-drv.c (table at 1806-1893) and the files cited per entry. The
private blocks the nvkms_params pointers reach are
src/nvidia-modeset/kapi/interface/nvkms-kapi-private.h; KAPI checks their size
exactly (nvkms-kapi.c:1698, 1888; nvkms-kapi-sync.c:199), so the cap is the
size.

Refused by absence: 0x42 GEM_IMPORT_USERSPACE_MEMORY (pins pages at an
address in the *caller's* mm, nvidia-drm-gem-user-memory.c:185 -- the backend's
-- and makes a GEM object with pMemory NULL), 0x45/0x46 PRIME fence contexts
(unused by 610 userspace, R:fences §1.2), 0x4a GEM_MAP_OFFSET, 0x4e
GEM_IDENTIFY_OBJECT and 0x58 GET_DRM_FILE_UNIQUE_ID (answered by the guest),
0x59-0x5c (not registered).

Entries that reach NVKMS or modeset locks run on an executor even on a
render node: the dpy/connector lookups walk the connector list under
mode_config locks.

GET_CRTC_CRC32 and _V2 are KMS-class only, although nvidia-drm lets a render
node call them (DRM_RENDER_ALLOW, nvidia-drm-drv.c:1860-1865): a file with no
master finds every CRTC, lease or no lease (drm_crtc_find is lease-filtered
only for a lessee, linux drm_lease.c:109-121, drm_mode_object.c:151-155), so
from a render node they read back a checksum of whatever the host compositor
scans out, at the cost of two synchronous core updates under nvkms_lock per
call (nvkms-evo.c:9301-9318). On a card or lease file the backend lets them
through only for a card of the guest's own (compositor-VM mode) or a real
lessee, whose CRTCs the host filters itself (kms.rs, `crc_gate`).
"""

from .lang import *

X = EXECUTOR


def nv(n):
    return 0x40 + n


def kms_params(struct, size, fields=()):
    """The u64 pointer + u64 size pair at 8/16 that nvidia-drm hands to KAPI."""
    return Ptr('nvkms_params_ptr', 8, IN, Field64('nvkms_params_size', 16),
               NoCopy(), max=size, elem_struct=struct, stride=size,
               fields=list(fields))


# The memory to import or export is named by an nvidiactl descriptor RM
# exported the object into (NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD), and
# RM resolves it with fget in the calling process (nv.c:4145).
MEM_FD = FdIn('memFd', 0, 4, K_DEV_CTL)


def shared(cls):
    """Entries valid on both a render node and a card or lease."""
    return [
        Ioctl('NV_GET_DPY_ID_FOR_CONNECTOR_ID', cls,
              'DRM_IOCTL_NVIDIA_GET_DPY_ID_FOR_CONNECTOR_ID',
              'struct drm_nvidia_get_dpy_id_for_connector_id_params', 8,
              nv(0x10), IOWR, exec=X, doc='nvidia-drm-drv.c:1174'),
        Ioctl('NV_GET_CONNECTOR_ID_FOR_DPY_ID', cls,
              'DRM_IOCTL_NVIDIA_GET_CONNECTOR_ID_FOR_DPY_ID',
              'struct drm_nvidia_get_connector_id_for_dpy_id_params', 8,
              nv(0x11), IOWR, exec=X, doc='nvidia-drm-drv.c:1214'),
    ]


# Card and lease files only (see above): checksums of scanout, each call
# holding nvkms_lock across two core updates.
CRC_IOCTLS = [
    Ioctl('NV_GET_CRTC_CRC32', KMS, 'DRM_IOCTL_NVIDIA_GET_CRTC_CRC32',
          'struct drm_nvidia_get_crtc_crc32_params', 8, nv(0x00), IOWR,
          exec=X, doc='nvidia-drm-crtc.c:3417'),
    Ioctl('NV_GET_CRTC_CRC32_V2', KMS, 'DRM_IOCTL_NVIDIA_GET_CRTC_CRC32_V2',
          'struct drm_nvidia_get_crtc_crc32_v2_params', 28, nv(0x0c), IOWR,
          exec=X, doc='nvidia-drm-crtc.c:3389'),
]

KMS_IOCTLS = CRC_IOCTLS + shared(KMS) + [
    # nvidia-drm-drv.c:1137: reads the drm_file's own client caps; primary
    # nodes only (flags 0).
    Ioctl('NV_GET_CLIENT_CAPABILITY', KMS,
          'DRM_IOCTL_NVIDIA_GET_CLIENT_CAPABILITY',
          'struct drm_nvidia_get_client_capability_params', 16, nv(0x08),
          IOWR, exec=X),
    # nvidia-drm-drv.c:1257-1435. `fd` is an nvidia-modeset descriptor,
    # resolved with fget in the calling process (nvidia-modeset-linux.c:2081)
    # and required to be fresh (nvkms.c:1291). SUB_OWNER (3) blanks every head
    # on the GPU and hands whole-device NVKMS ownership over (drv.c:1372), so
    # POL_GRANT refuses everything but MODESET (2) (RV: GRANT_PERMISSIONS).
    Ioctl('NV_GRANT_PERMISSIONS', KMS, 'DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS',
          'struct drm_nvidia_grant_permissions_params', 12, nv(0x12), IOWR,
          exec=X, policy=POL_GRANT, fields=[
              FdIn('fd', 0, 4, K_DEV_MODESET),
          ]),
    # nvidia-drm-drv.c:1567; SUB_OWNER revocation is device-wide (1533).
    Ioctl('NV_REVOKE_PERMISSIONS', KMS, 'DRM_IOCTL_NVIDIA_REVOKE_PERMISSIONS',
          'struct drm_nvidia_revoke_permissions_params', 8, nv(0x13), IOWR,
          exec=X, policy=POL_REVOKE),
    # The same two before they grew a `type` (535's nvidia-drm-ioctl.h:
    # {s32 fd; u32 dpyId} and {u32 dpyId}, always MODESET;
    # gen/nvkms/535.129.03.json). Their own numbers, since the size is in
    # the number; the NVKMS hook lets them through only on a host whose
    # nvidia-drm has that layout (NvkmsLayout::drm_grant_typed).
    Ioctl('NV_GRANT_PERMISSIONS_UNTYPED', KMS, None, None, 8, nv(0x12), IOWR,
          exec=X, policy=POL_GRANT, fields=[
              FdIn('fd', 0, 4, K_DEV_MODESET),
          ]),
    Ioctl('NV_REVOKE_PERMISSIONS_UNTYPED', KMS, None, None, 4, nv(0x13), IOWR,
          exec=X, policy=POL_REVOKE),
]

RENDER_IOCTLS = shared(RENDER) + [
    # nvidia-drm-gem-nvkms-memory.c:525 → nvkms-kapi.c:1681 ImportMemory.
    Ioctl('NV_GEM_IMPORT_NVKMS_MEMORY', RENDER,
          'DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY',
          'struct drm_nvidia_gem_import_nvkms_memory_params', 32, nv(0x01),
          IOWR, fields=[
              kms_params('struct NvKmsKapiPrivImportMemoryParams', 28,
                         [MEM_FD]),
              GemOut('handle', 24),
          ]),
    Ioctl('NV_GET_DEV_INFO', RENDER, 'DRM_IOCTL_NVIDIA_GET_DEV_INFO',
          'struct drm_nvidia_get_dev_info_params', 36, nv(0x03), IOWR),
    Ioctl('NV_FENCE_SUPPORTED', RENDER, 'DRM_IOCTL_NVIDIA_FENCE_SUPPORTED',
          None, 0, nv(0x04), IOC_NONE),
    # nvidia-drm-gem-nvkms-memory.c:576 → nvkms-kapi.c:1867 ExportMemory.
    Ioctl('NV_GEM_EXPORT_NVKMS_MEMORY', RENDER,
          'DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY',
          'struct drm_nvidia_gem_export_nvkms_memory_params', 24, nv(0x09),
          IOWR, fields=[
              GemIn('handle', 0),
              kms_params('struct NvKmsKapiPrivExportMemoryParams', 4,
                         [MEM_FD]),
          ]),
    # nvidia-drm-gem-nvkms-memory.c:625: scanout-capable unless NO_SCANOUT.
    Ioctl('NV_GEM_ALLOC_NVKMS_MEMORY', RENDER,
          'DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY',
          'struct drm_nvidia_gem_alloc_nvkms_memory_params', 24, nv(0x0b),
          IOWR, fields=[
              GemOut('handle', 0),
          ]),
    # nvidia-drm-gem-dma-buf.c:168: same private block as the NVKMS export.
    Ioctl('NV_GEM_EXPORT_DMABUF_MEMORY', RENDER,
          'DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY',
          'struct drm_nvidia_gem_export_dmabuf_memory_params', 24, nv(0x0d),
          IOWR, fields=[
              GemIn('handle', 0),
              kms_params('struct NvKmsKapiPrivExportMemoryParams', 4,
                         [MEM_FD]),
          ]),
    Ioctl('NV_DMABUF_SUPPORTED', RENDER, 'DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED',
          None, 0, nv(0x0f), IOC_NONE),
    # nvidia-drm-fence.c:1224 → nvkms-kapi-sync.c:182: RM handles of the
    # caller's own client, no descriptor; the result is a fence-context GEM.
    Ioctl('NV_SEMSURF_FENCE_CTX_CREATE', RENDER,
          'DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE',
          'struct drm_nvidia_semsurf_fence_ctx_create_params', 32, nv(0x14),
          IOWR, policy=POL_FENCE, fields=[
              kms_params('struct NvKmsKapiPrivImportSemaphoreSurfaceParams',
                         16),
              GemOut('handle', 24),
          ]),
    # nvidia-drm-fence.c:1471: a new sync_file in the caller's table.
    Ioctl('NV_SEMSURF_FENCE_CREATE', RENDER,
          'DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE',
          'struct drm_nvidia_semsurf_fence_create_params', 24, nv(0x15), IOWR,
          policy=POL_FENCE, fields=[
              GemIn('fence_context_handle', 0),
              FdOut('fd', 16),
          ]),
    # nvidia-drm-fence.c:1635: any sync_file, resolved in the caller.
    Ioctl('NV_SEMSURF_FENCE_WAIT', RENDER,
          'DRM_IOCTL_NVIDIA_SEMSURF_FENCE_WAIT',
          'struct drm_nvidia_semsurf_fence_wait_params', 24, nv(0x16), IOW,
          policy=POL_FENCE, fields=[
              GemIn('fence_context_handle', 0),
              FdIn('fd', 4, 4, K_SYNC_FILE),
          ]),
    # nvidia-drm-fence.c:1748: both handles in the file the call runs on.
    Ioctl('NV_SEMSURF_FENCE_ATTACH', RENDER,
          'DRM_IOCTL_NVIDIA_SEMSURF_FENCE_ATTACH',
          'struct drm_nvidia_semsurf_fence_attach_params', 24, nv(0x17), IOW,
          policy=POL_FENCE, fields=[
              GemIn('handle', 0),
              GemIn('fence_context_handle', 4),
          ]),
]
