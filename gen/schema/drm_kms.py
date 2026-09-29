# SPDX-License-Identifier: Apache-2.0
"""DRM core KMS ioctls, run on a host card node or lease (class KMS).

Layouts and fill rules are Linux 7.2.7's (include/uapi/drm/drm.h,
drm_mode.h; handlers cited per entry, all under drivers/gpu/drm/). Offsets
are asserted by the probe schema_gen.py --probe writes, compiled against
those headers, so a transcription slip is a build failure rather than a
pointer at the wrong offset.

Every KMS entry runs on the file's serial executor: nearly all of them take
modeset locks, and the queue thread must never wait on one (ARCHITECTURE.md,
"Protocol v2").

Refused here by absence, on every class: GEM_CLOSE/FLINK/OPEN, PRIME_*,
AUTH/GET_MAGIC (guest-local); MAP_DUMB and DESTROY_DUMB (answered from the
guest's proxy). SET/DROP_MASTER are arbitrated by the guest core and never
forwarded from userspace; the guest's master hooks send them as IOCTL2 so
they run on the file's executor (POL_MASTER: host cards only).

Caps (`max`) are bytes and far above anything real hardware reports; they
exist so a count the caller made up cannot size an allocation.
"""

from .lang import *

X = EXECUTOR


def u32_list(name, off, count_name, count_off, cap, cb=Partial):
    """An OUT array of u32 object ids filled "as many as fit"."""
    return Ptr(name, off, OUT, Count(count_name, count_off, 4, 4),
               cb(count_name, count_off, 4, 4), max=cap * 4)


IOCTLS = [
    Ioctl('GET_CAP', KMS, 'DRM_IOCTL_GET_CAP', 'struct drm_get_cap', 16,
          0x0c, IOWR, exec=X,
          doc='drm_ioctl.c:234 drm_getcap; the guest overrides SYNCOBJ*/PRIME'),
    Ioctl('SET_CLIENT_CAP', KMS, 'DRM_IOCTL_SET_CLIENT_CAP',
          'struct drm_set_client_cap', 16, 0x0d, IOW, exec=X,
          doc='drm_ioctl.c:317; per-file host state'),
    # drm_auth.c:245, 288. Sent only by the guest's drm_driver master hooks,
    # after its own core has arbitrated (nvgpu_kms.c); nvidia-drm's
    # master_set grabs NVKMS ownership and master_drop disables every head
    # (nvidia-drm-drv.c:954, 1038), so they wait on nvkms_lock like any KMS
    # call and go to the executor.
    Ioctl('SET_MASTER', KMS, 'DRM_IOCTL_SET_MASTER', None, 0, 0x1e, IOC_NONE,
          exec=X, policy=POL_MASTER),
    Ioctl('DROP_MASTER', KMS, 'DRM_IOCTL_DROP_MASTER', None, 0, 0x1f,
          IOC_NONE, exec=X, policy=POL_MASTER),
    Ioctl('WAIT_VBLANK', KMS, 'DRM_IOCTL_WAIT_VBLANK', 'union drm_wait_vblank',
          24, 0x3a, IOWR, exec=X,
          doc='drm_vblank.c:1740; the guest only forwards the EVENT form'),
    Ioctl('CRTC_GET_SEQUENCE', KMS, 'DRM_IOCTL_CRTC_GET_SEQUENCE',
          'struct drm_crtc_get_sequence', 24, 0x3b, IOWR, exec=X),
    Ioctl('CRTC_QUEUE_SEQUENCE', KMS, 'DRM_IOCTL_CRTC_QUEUE_SEQUENCE',
          'struct drm_crtc_queue_sequence', 24, 0x3c, IOWR, exec=X),

    # drm_mode_config.c:93 drm_mode_getresources: each list is filled with
    # min(count, actual) ids and the count set to actual.
    Ioctl('GETRESOURCES', KMS, 'DRM_IOCTL_MODE_GETRESOURCES',
          'struct drm_mode_card_res', 64, 0xa0, IOWR, exec=X, fields=[
              u32_list('fb_id_ptr', 0, 'count_fbs', 32, 4096),
              u32_list('crtc_id_ptr', 8, 'count_crtcs', 36, 4096),
              u32_list('connector_id_ptr', 16, 'count_connectors', 40, 4096),
              u32_list('encoder_id_ptr', 24, 'count_encoders', 44, 4096),
          ]),
    # drm_crtc.c:543 drm_mode_getcrtc never reads set_connectors_ptr; it is
    # still a pointer, so the host is handed NULL rather than a guest address.
    Ioctl('GETCRTC', KMS, 'DRM_IOCTL_MODE_GETCRTC', 'struct drm_mode_crtc',
          104, 0xa1, IOWR, exec=X, fields=[
              Ptr('set_connectors_ptr', 0, IN, Const(0), NoCopy(), max=0),
          ]),
    # drm_crtc.c:841: count_connectors <= num_connector, ids read as u32.
    Ioctl('SETCRTC', KMS, 'DRM_IOCTL_MODE_SETCRTC', 'struct drm_mode_crtc',
          104, 0xa2, IOWR, exec=X, fields=[
              Ptr('set_connectors_ptr', 0, IN,
                  Count('count_connectors', 8, 4, 4), NoCopy(), max=1024 * 4),
          ]),
    # drm_plane.c:1276 drm_mode_cursor_common: the handle is a GEM handle
    # only with DRM_MODE_CURSOR_BO, and becomes a framebuffer through
    # drm_internal_framebuffer_create (1195), i.e. nvidia-drm's fb_create.
    Ioctl('CURSOR', KMS, 'DRM_IOCTL_MODE_CURSOR', 'struct drm_mode_cursor',
          28, 0xa3, IOWR, exec=X, fields=[
              GemIn('handle', 24, validate_nvkms=True,
                    cond=Cond('flags', 0, 0x01, 0x01)),
          ]),
    Ioctl('CURSOR2', KMS, 'DRM_IOCTL_MODE_CURSOR2', 'struct drm_mode_cursor2',
          36, 0xbb, IOWR, exec=X, fields=[
              GemIn('handle', 24, validate_nvkms=True,
                    cond=Cond('flags', 0, 0x01, 0x01)),
          ]),
    # drm_color_mgmt.c:430: gamma_size must equal the CRTC's (else -EINVAL,
    # nothing written); on success all three are written in full.
    Ioctl('GETGAMMA', KMS, 'DRM_IOCTL_MODE_GETGAMMA',
          'struct drm_mode_crtc_lut', 32, 0xa4, IOWR, exec=X, fields=[
              Ptr(n, o, OUT, Count('gamma_size', 4, 4, 2), Full(),
                  max=8192 * 2)
              for n, o in (('red', 8), ('green', 16), ('blue', 24))
          ]),
    Ioctl('SETGAMMA', KMS, 'DRM_IOCTL_MODE_SETGAMMA',
          'struct drm_mode_crtc_lut', 32, 0xa5, IOWR, exec=X, fields=[
              Ptr(n, o, IN, Count('gamma_size', 4, 4, 2), NoCopy(),
                  max=8192 * 2)
              for n, o in (('red', 8), ('green', 16), ('blue', 24))
          ]),
    Ioctl('GETENCODER', KMS, 'DRM_IOCTL_MODE_GETENCODER',
          'struct drm_mode_get_encoder', 20, 0xa6, IOWR, exec=X),
    # drm_connector.c:3326: encoders and modes all-or-nothing (3353, 3404),
    # props/values through drm_mode_object_get_properties, "as many as fit"
    # (drm_mode_object.c:414).
    Ioctl('GETCONNECTOR', KMS, 'DRM_IOCTL_MODE_GETCONNECTOR',
          'struct drm_mode_get_connector', 80, 0xa7, IOWR, exec=X, fields=[
              u32_list('encoders_ptr', 0, 'count_encoders', 40, 256,
                       cb=AllOrNothing),
              Ptr('modes_ptr', 8, OUT, Count('count_modes', 32, 4, 68),
                  AllOrNothing('count_modes', 32, 4, 68), max=1024 * 68,
                  elem_struct='struct drm_mode_modeinfo', stride=68),
              u32_list('props_ptr', 16, 'count_props', 36, 1024),
              Ptr('prop_values_ptr', 24, OUT, Count('count_props', 36, 4, 8),
                  Partial('count_props', 36, 4, 8), max=1024 * 8),
          ]),
    # drm_property.c:458: values "as many as fit". The enum list is written
    # only for ENUM/BITMASK properties and its count left alone otherwise, so
    # it travels IN/OUT: bytes the kernel does not write come back unchanged.
    Ioctl('GETPROPERTY', KMS, 'DRM_IOCTL_MODE_GETPROPERTY',
          'struct drm_mode_get_property', 64, 0xaa, IOWR, exec=X, fields=[
              Ptr('values_ptr', 0, OUT, Count('count_values', 56, 4, 8),
                  Partial('count_values', 56, 4, 8), max=1024 * 8),
              Ptr('enum_blob_ptr', 8, INOUT,
                  Count('count_enum_blobs', 60, 4, 40),
                  Partial('count_enum_blobs', 60, 4, 40), max=1024 * 40,
                  elem_struct='struct drm_mode_property_enum', stride=40),
          ]),
    # drm_connector.c:3267 → the OBJ_SETPROPERTY path. `value` is a plain
    # integer unless the property is an fd or pointer one, which POL_SETPROP
    # refuses outright (RV:setprop).
    Ioctl('SETPROPERTY', KMS, 'DRM_IOCTL_MODE_SETPROPERTY',
          'struct drm_mode_connector_set_property', 16, 0xab, IOWR, exec=X,
          policy=POL_SETPROP),
    # drm_property.c:824: data copied only if length == the blob's length
    # (838); the true length is always returned.
    Ioctl('GETPROPBLOB', KMS, 'DRM_IOCTL_MODE_GETPROPBLOB',
          'struct drm_mode_get_blob', 16, 0xac, IOWR, exec=X, fields=[
              Ptr('data', 8, OUT, Count('length', 4, 4, 1),
                  Exact('length', 4, 4), max=1 << 20),
          ]),
    # drm_framebuffer.c:521: a new GEM handle in the caller's file, or 0 for
    # a non-master. POL_FB_READ returns it only for the file's own FBs
    # (RV:getfb: a lessee is "current master" and FB ids are global).
    Ioctl('GETFB', KMS, 'DRM_IOCTL_MODE_GETFB', 'struct drm_mode_fb_cmd', 28,
          0xad, IOWR, exec=X, policy=POL_FB_READ, fields=[
              GemOut('handle', 24),
          ]),
    # drm_framebuffer.c:583: handles deduplicated when planes share an
    # object (663); the backend re-homes each distinct one once.
    Ioctl('GETFB2', KMS, 'DRM_IOCTL_MODE_GETFB2', 'struct drm_mode_fb_cmd2',
          104, 0xce, IOWR, exec=X, policy=POL_FB_READ, fields=[
              Array('handles', 20, 4, 4, fields=[GemOut('', 0)]),
          ]),
    Ioctl('ADDFB', KMS, 'DRM_IOCTL_MODE_ADDFB', 'struct drm_mode_fb_cmd', 28,
          0xae, IOWR, exec=X, policy=POL_FB_CREATE, fields=[
              GemIn('handle', 24, validate_nvkms=True),
          ]),
    # drm_framebuffer.c:330 + nvidia-drm-fb.c:112, which looks up a handle
    # per format plane and dereferences its pMemory unchecked (:167).
    Ioctl('ADDFB2', KMS, 'DRM_IOCTL_MODE_ADDFB2', 'struct drm_mode_fb_cmd2',
          104, 0xb8, IOWR, exec=X, policy=POL_FB_CREATE | POL_FB_PLANES,
          fields=[
              Array('handles', 20, 4, 4,
                    fields=[GemIn('', 0, validate_nvkms=True)]),
          ]),
    Ioctl('RMFB', KMS, 'DRM_IOCTL_MODE_RMFB', 'unsigned int', 4, 0xaf, IOWR,
          exec=X, policy=POL_FB_REMOVE),
    Ioctl('CLOSEFB', KMS, 'DRM_IOCTL_MODE_CLOSEFB', 'struct drm_mode_closefb',
          8, 0xd0, IOWR, exec=X, policy=POL_FB_REMOVE),
    Ioctl('PAGE_FLIP', KMS, 'DRM_IOCTL_MODE_PAGE_FLIP',
          'struct drm_mode_crtc_page_flip', 24, 0xb0, IOWR, exec=X),
    # drm_framebuffer.c:746: num_clips <= DRM_MODE_FB_DIRTY_MAX_CLIPS (256).
    Ioctl('DIRTYFB', KMS, 'DRM_IOCTL_MODE_DIRTYFB',
          'struct drm_mode_fb_dirty_cmd', 24, 0xb1, IOWR, exec=X, fields=[
              Ptr('clips_ptr', 16, IN, Count('num_clips', 12, 4, 8), NoCopy(),
                  max=256 * 8, elem_struct='struct drm_clip_rect', stride=8),
          ]),
    Ioctl('CREATE_DUMB', KMS, 'DRM_IOCTL_MODE_CREATE_DUMB',
          'struct drm_mode_create_dumb', 32, 0xb2, IOWR, exec=X, fields=[
              GemOut('handle', 16),
          ]),
    Ioctl('GETPLANERESOURCES', KMS, 'DRM_IOCTL_MODE_GETPLANERESOURCES',
          'struct drm_mode_get_plane_res', 16, 0xb5, IOWR, exec=X, fields=[
              u32_list('plane_id_ptr', 0, 'count_planes', 8, 1024),
          ]),
    # drm_plane.c:893: formats written only if count >= format_count.
    Ioctl('GETPLANE', KMS, 'DRM_IOCTL_MODE_GETPLANE',
          'struct drm_mode_get_plane', 32, 0xb6, IOWR, exec=X, fields=[
              u32_list('format_type_ptr', 24, 'count_format_types', 20, 1024,
                       cb=AllOrNothing),
          ]),
    Ioctl('SETPLANE', KMS, 'DRM_IOCTL_MODE_SETPLANE',
          'struct drm_mode_set_plane', 48, 0xb7, IOWR, exec=X),
    Ioctl('OBJ_GETPROPERTIES', KMS, 'DRM_IOCTL_MODE_OBJ_GETPROPERTIES',
          'struct drm_mode_obj_get_properties', 32, 0xb9, IOWR, exec=X,
          fields=[
              u32_list('props_ptr', 0, 'count_props', 16, 1024),
              Ptr('prop_values_ptr', 8, OUT, Count('count_props', 16, 4, 8),
                  Partial('count_props', 16, 4, 8), max=1024 * 8),
          ]),
    Ioctl('OBJ_SETPROPERTY', KMS, 'DRM_IOCTL_MODE_OBJ_SETPROPERTY',
          'struct drm_mode_obj_set_property', 24, 0xba, IOWR, exec=X,
          policy=POL_SETPROP),
    # drm_atomic_uapi.c:1603: objs and count_props are count_objs long,
    # props and values Σ count_props long, read element by element and never
    # written (1680-1750). Fence/pointer property values are SPECIAL_ATOMIC's.
    Ioctl('ATOMIC', KMS, 'DRM_IOCTL_MODE_ATOMIC', 'struct drm_mode_atomic',
          56, 0xbc, IOWR, exec=X, special=SPECIAL_ATOMIC, fields=[
              Ptr('objs_ptr', 8, IN, Count('count_objs', 4, 4, 4), NoCopy(),
                  max=4096 * 4),
              Ptr('count_props_ptr', 16, IN, Count('count_objs', 4, 4, 4),
                  NoCopy(), max=4096 * 4),
              Ptr('props_ptr', 24, IN, Sum('count_props_ptr', 4), NoCopy(),
                  max=65536 * 4),
              Ptr('prop_values_ptr', 32, IN, Sum('count_props_ptr', 8),
                  NoCopy(), max=65536 * 8),
          ]),
    # drm_property.c:853: `length` bytes of blob data.
    Ioctl('CREATEPROPBLOB', KMS, 'DRM_IOCTL_MODE_CREATEPROPBLOB',
          'struct drm_mode_create_blob', 16, 0xbd, IOWR, exec=X, fields=[
              Ptr('data', 0, IN, Count('length', 8, 4, 1), NoCopy(),
                  max=1 << 20),
          ]),
    Ioctl('DESTROYPROPBLOB', KMS, 'DRM_IOCTL_MODE_DESTROYPROPBLOB',
          'struct drm_mode_destroy_blob', 4, 0xbe, IOWR, exec=X),
    # drm_lease.c:475: object ids in, a lessee DRM file out.
    Ioctl('CREATE_LEASE', KMS, 'DRM_IOCTL_MODE_CREATE_LEASE',
          'struct drm_mode_create_lease', 24, 0xc6, IOWR, exec=X, fields=[
              Ptr('object_ids', 0, IN, Count('object_count', 8, 4, 4),
                  NoCopy(), max=4096 * 4),
              FdOut('fd', 20),
          ]),
    # drm_lease.c:588, 636: "as many as fit"; the count is not updated on
    # -EFAULT, which Partial's success-only rule already covers.
    Ioctl('LIST_LESSEES', KMS, 'DRM_IOCTL_MODE_LIST_LESSEES',
          'struct drm_mode_list_lessees', 16, 0xc7, IOWR, exec=X, fields=[
              u32_list('lessees_ptr', 8, 'count_lessees', 0, 4096),
          ]),
    Ioctl('GET_LEASE', KMS, 'DRM_IOCTL_MODE_GET_LEASE',
          'struct drm_mode_get_lease', 16, 0xc8, IOWR, exec=X, fields=[
              u32_list('objects_ptr', 8, 'count_objects', 0, 16384),
          ]),
    Ioctl('REVOKE_LEASE', KMS, 'DRM_IOCTL_MODE_REVOKE_LEASE',
          'struct drm_mode_revoke_lease', 4, 0xc9, IOWR, exec=X),
]
