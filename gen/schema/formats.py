# SPDX-License-Identifier: Apache-2.0
"""Pixel formats with more than one plane, and how many.

POL_FB_PLANES needs the plane count of an ADDFB2 pixel_format to know which
handles[] entries nvidia-drm will look up (nvidia-drm-fb.c:94-113 walks
nv_drm_format_num_planes(), i.e. drm_format_info()). Every format not listed
has one plane. Taken from Linux 7.2.7 drivers/gpu/drm/drm_fourcc.c's
format table (.num_planes > 1) with the fourcc codes of
include/uapi/drm/drm_fourcc.h; the probe asserts each code.

A format missing from here that the host knows as multi-planar fails closed:
the backend requires handles[1..] to be 0, and the host's framebuffer_check
(drm_framebuffer.c:182) then refuses the missing plane.
"""

MULTI_PLANE = [
    ('DRM_FORMAT_BGR565_A8', 0x38413542, 2),
    ('DRM_FORMAT_BGR888_A8', 0x38413842, 2),
    ('DRM_FORMAT_BGRX8888_A8', 0x38415842, 2),
    ('DRM_FORMAT_NV12', 0x3231564e, 2),
    ('DRM_FORMAT_NV15', 0x3531564e, 2),
    ('DRM_FORMAT_NV16', 0x3631564e, 2),
    ('DRM_FORMAT_NV20', 0x3032564e, 2),
    ('DRM_FORMAT_NV21', 0x3132564e, 2),
    ('DRM_FORMAT_NV24', 0x3432564e, 2),
    ('DRM_FORMAT_NV30', 0x3033564e, 2),
    ('DRM_FORMAT_NV42', 0x3234564e, 2),
    ('DRM_FORMAT_NV61', 0x3136564e, 2),
    ('DRM_FORMAT_P010', 0x30313050, 2),
    ('DRM_FORMAT_P012', 0x32313050, 2),
    ('DRM_FORMAT_P016', 0x36313050, 2),
    ('DRM_FORMAT_P030', 0x30333050, 2),
    ('DRM_FORMAT_P210', 0x30313250, 2),
    ('DRM_FORMAT_P230', 0x30333250, 2),
    ('DRM_FORMAT_Q401', 0x31303451, 3),
    ('DRM_FORMAT_Q410', 0x30313451, 3),
    ('DRM_FORMAT_RGB565_A8', 0x38413552, 2),
    ('DRM_FORMAT_RGB888_A8', 0x38413852, 2),
    ('DRM_FORMAT_RGBX8888_A8', 0x38415852, 2),
    ('DRM_FORMAT_S010', 0x30313053, 3),
    ('DRM_FORMAT_S012', 0x32313053, 3),
    ('DRM_FORMAT_S016', 0x36313053, 3),
    ('DRM_FORMAT_S210', 0x30313253, 3),
    ('DRM_FORMAT_S212', 0x32313253, 3),
    ('DRM_FORMAT_S216', 0x36313253, 3),
    ('DRM_FORMAT_S410', 0x30313453, 3),
    ('DRM_FORMAT_S412', 0x32313453, 3),
    ('DRM_FORMAT_S416', 0x36313453, 3),
    ('DRM_FORMAT_T430', 0x30333454, 3),
    ('DRM_FORMAT_XBGR8888_A8', 0x38414258, 2),
    ('DRM_FORMAT_XRGB8888_A8', 0x38415258, 2),
    ('DRM_FORMAT_YUV410', 0x39565559, 3),
    ('DRM_FORMAT_YUV411', 0x31315559, 3),
    ('DRM_FORMAT_YUV420', 0x32315559, 3),
    ('DRM_FORMAT_YUV422', 0x36315559, 3),
    ('DRM_FORMAT_YUV444', 0x34325559, 3),
    ('DRM_FORMAT_YVU410', 0x39555659, 3),
    ('DRM_FORMAT_YVU411', 0x31315659, 3),
    ('DRM_FORMAT_YVU420', 0x32315659, 3),
    ('DRM_FORMAT_YVU422', 0x36315659, 3),
    ('DRM_FORMAT_YVU444', 0x34325659, 3),
]
