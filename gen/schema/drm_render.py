"""DRM core syncobj ioctls, run on the file's host render node (class RENDER).

Layouts from Linux 7.2.7 include/uapi/drm/drm.h; handlers in
drivers/gpu/drm/drm_syncobj.c. All of them are fence schemas: the FENCES
workstream's policy hook decides whether and how each may run (waits become
timeout-0 polls there, DESIGN §6), and until it does the backend refuses them.
Syncobj handles are per host file and cross as they are; only the descriptor
fields are translated.
"""

from .lang import *

F = POL_FENCE


def handles(off, count_name, count_off, cap=4096):
    return Ptr('handles', off, IN, Count(count_name, count_off, 4, 4),
               NoCopy(), max=cap * 4)


IOCTLS = [
    Ioctl('SYNCOBJ_CREATE', RENDER, 'DRM_IOCTL_SYNCOBJ_CREATE',
          'struct drm_syncobj_create', 8, 0xbf, IOWR, policy=F),
    Ioctl('SYNCOBJ_DESTROY', RENDER, 'DRM_IOCTL_SYNCOBJ_DESTROY',
          'struct drm_syncobj_destroy', 8, 0xc0, IOWR, policy=F),
    # drm_syncobj.c:856: a syncobj file, or with EXPORT_SYNC_FILE a sync_file.
    Ioctl('SYNCOBJ_HANDLE_TO_FD', RENDER, 'DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD',
          'struct drm_syncobj_handle', 24, 0xc1, IOWR, policy=F, fields=[
              FdOut('fd', 8),
          ]),
    # drm_syncobj.c:888: with IMPORT_SYNC_FILE (1) the fd is a sync_file
    # merged into an existing syncobj, without it a syncobj file (712).
    Ioctl('SYNCOBJ_FD_TO_HANDLE', RENDER, 'DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE',
          'struct drm_syncobj_handle', 24, 0xc2, IOWR, policy=F, fields=[
              FdIn('fd', 8, 4, K_SYNC_FILE, cond=Cond('flags', 4, 1, 1)),
              FdIn('fd', 8, 4, K_SYNCOBJ, cond=Cond('flags', 4, 1, 0)),
          ]),
    Ioctl('SYNCOBJ_WAIT', RENDER, 'DRM_IOCTL_SYNCOBJ_WAIT',
          'struct drm_syncobj_wait', 40, 0xc3, IOWR, policy=F, fields=[
              handles(0, 'count_handles', 16),
          ]),
    Ioctl('SYNCOBJ_RESET', RENDER, 'DRM_IOCTL_SYNCOBJ_RESET',
          'struct drm_syncobj_array', 16, 0xc4, IOWR, policy=F, fields=[
              handles(0, 'count_handles', 8),
          ]),
    Ioctl('SYNCOBJ_SIGNAL', RENDER, 'DRM_IOCTL_SYNCOBJ_SIGNAL',
          'struct drm_syncobj_array', 16, 0xc5, IOWR, policy=F, fields=[
              handles(0, 'count_handles', 8),
          ]),
    # drm_syncobj.c:1366: points may be NULL (all zero, 1060).
    Ioctl('SYNCOBJ_TIMELINE_WAIT', RENDER, 'DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT',
          'struct drm_syncobj_timeline_wait', 48, 0xca, IOWR, policy=F,
          fields=[
              handles(0, 'count_handles', 24),
              Ptr('points', 8, IN, Count('count_handles', 24, 4, 8), NoCopy(),
                  max=4096 * 8),
          ]),
    # drm_syncobj.c:1654: points are written one by one (1712).
    Ioctl('SYNCOBJ_QUERY', RENDER, 'DRM_IOCTL_SYNCOBJ_QUERY',
          'struct drm_syncobj_timeline_array', 24, 0xcb, IOWR, policy=F,
          fields=[
              handles(0, 'count_handles', 16),
              Ptr('points', 8, OUT, Count('count_handles', 16, 4, 8), Full(),
                  max=4096 * 8),
          ]),
    Ioctl('SYNCOBJ_TRANSFER', RENDER, 'DRM_IOCTL_SYNCOBJ_TRANSFER',
          'struct drm_syncobj_transfer', 32, 0xcc, IOWR, policy=F),
    Ioctl('SYNCOBJ_TIMELINE_SIGNAL', RENDER,
          'DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL',
          'struct drm_syncobj_timeline_array', 24, 0xcd, IOWR, policy=F,
          fields=[
              handles(0, 'count_handles', 16),
              Ptr('points', 8, IN, Count('count_handles', 16, 4, 8), NoCopy(),
                  max=4096 * 8),
          ]),
    # drm_syncobj.c:1462: an eventfd in the caller's table (1482).
    Ioctl('SYNCOBJ_EVENTFD', RENDER, 'DRM_IOCTL_SYNCOBJ_EVENTFD',
          'struct drm_syncobj_eventfd', 24, 0xcf, IOWR, policy=F, fields=[
              FdIn('fd', 16, 4, K_EVENTFD),
          ]),
]
