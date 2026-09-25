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
#define NVGPU_SCHEMA_MAX_LIST 32
#define NVGPU_SCHEMA_MAX_DEPTH 2

#define NVGPU_NVKMS_IOCTL_IOWR 0xc0106d00u

#define NVGPU_SCHEMA_VERSION(a, b, c) ((a) * 1000000u + (b) * 1000u + (c))

struct nvgpu_sfield {
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
};

struct nvgpu_sioctl {
  const char *name;
  u32 cmd;       /* the full ioctl number, direction and size included */
  u32 nvkms_cmd; /* MODESET: the NVKMS command inside NvKmsIoctlParams */
  u32 size;      /* == _IOC_SIZE(cmd) */
  u8 sclass;     /* NVGPU_SCLASS_* */
  u8 special;    /* NVGPU_SSPECIAL_* */
  u16 flags;     /* NVGPU_SIO_* */
  u32 policy;    /* NVGPU_SPOL_* */
  u16 field, nfield;
};

struct nvgpu_stable {
  const char *name;
  u32 vmin, vmax; /* NVGPU_SCHEMA_VERSION; 0, 0 = any version */
  const struct nvgpu_sioctl *ioctls;
  u32 nioctls;
  const struct nvgpu_sfield *fields;
  u32 nfields;
};

/* What a guest runs with: the DRM tables, and NVKMS's for the host version. */
struct nvgpu_schema_set {
  const struct nvgpu_stable *drm;
  const struct nvgpu_stable *modeset; /* NULL: no table for this host */
};

#ifdef NVGPU_SCHEMA_TABLES

static const struct nvgpu_sfield nvgpu_schema_drm_fields[] = {
  /*   0 GETRESOURCES.fb_id_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 32, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 32, .cb_width = 4, .cb_arg = 4},
  /*   1 GETRESOURCES.crtc_id_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 36, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 36, .cb_width = 4, .cb_arg = 4},
  /*   2 GETRESOURCES.connector_id_ptr */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 40, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 40, .cb_width = 4, .cb_arg = 4},
  /*   3 GETRESOURCES.encoder_id_ptr */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 44, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 44, .cb_width = 4, .cb_arg = 4},
  /*   4 GETCRTC.set_connectors_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .len_kind = NVGPU_SLEN_CONST},
  /*   5 SETCRTC.set_connectors_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 4096, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*   6 CURSOR.handle */
  {.off = 24, .cond_mask = 1, .cond_value = 1, .kind = NVGPU_SF_GEM_IN, .width = 4, .flags = NVGPU_SFF_COND | NVGPU_SFF_VALIDATE_NVKMS},
  /*   7 CURSOR2.handle */
  {.off = 24, .cond_mask = 1, .cond_value = 1, .kind = NVGPU_SF_GEM_IN, .width = 4, .flags = NVGPU_SFF_COND | NVGPU_SFF_VALIDATE_NVKMS},
  /*   8 GETGAMMA.red */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2, .cb_kind = NVGPU_SCB_FULL},
  /*   9 GETGAMMA.green */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2, .cb_kind = NVGPU_SCB_FULL},
  /*  10 GETGAMMA.blue */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2, .cb_kind = NVGPU_SCB_FULL},
  /*  11 SETGAMMA.red */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2},
  /*  12 SETGAMMA.green */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2},
  /*  13 SETGAMMA.blue */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 2},
  /*  14 GETCONNECTOR.encoders_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 1024, .len_kind = NVGPU_SLEN_COUNT, .len_a = 40, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_ALL_OR_NOTHING, .cb_off = 40, .cb_width = 4, .cb_arg = 4},
  /*  15 GETCONNECTOR.modes_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 69632, .stride = 68, .len_kind = NVGPU_SLEN_COUNT, .len_a = 32, .len_width = 4, .len_elem = 68, .cb_kind = NVGPU_SCB_ALL_OR_NOTHING, .cb_off = 32, .cb_width = 4, .cb_arg = 68},
  /*  16 GETCONNECTOR.props_ptr */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 4096, .len_kind = NVGPU_SLEN_COUNT, .len_a = 36, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 36, .cb_width = 4, .cb_arg = 4},
  /*  17 GETCONNECTOR.prop_values_ptr */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 8192, .len_kind = NVGPU_SLEN_COUNT, .len_a = 36, .len_width = 4, .len_elem = 8, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 36, .cb_width = 4, .cb_arg = 8},
  /*  18 GETPROPERTY.values_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 8192, .len_kind = NVGPU_SLEN_COUNT, .len_a = 56, .len_width = 4, .len_elem = 8, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 56, .cb_width = 4, .cb_arg = 8},
  /*  19 GETPROPERTY.enum_blob_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40960, .stride = 40, .len_kind = NVGPU_SLEN_COUNT, .len_a = 60, .len_width = 4, .len_elem = 40, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 60, .cb_width = 4, .cb_arg = 40},
  /*  20 GETPROPBLOB.data */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 1048576, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_EXACT, .cb_off = 4, .cb_width = 4},
  /*  21 GETFB.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  22 GETFB2.handles */
  {.off = 20, .kind = NVGPU_SF_ARRAY, .stride = 4, .count = 4, .child = 23, .nchild = 1},
  /*  23 GETFB2.handles.[] */
  {.kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  24 ADDFB.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_IN, .width = 4, .flags = NVGPU_SFF_VALIDATE_NVKMS},
  /*  25 ADDFB2.handles */
  {.off = 20, .kind = NVGPU_SF_ARRAY, .stride = 4, .count = 4, .child = 26, .nchild = 1},
  /*  26 ADDFB2.handles.[] */
  {.kind = NVGPU_SF_GEM_IN, .width = 4, .flags = NVGPU_SFF_VALIDATE_NVKMS},
  /*  27 DIRTYFB.clips_ptr */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 2048, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 12, .len_width = 4, .len_elem = 8},
  /*  28 CREATE_DUMB.handle */
  {.off = 16, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  29 GETPLANERESOURCES.plane_id_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 4096, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 8, .cb_width = 4, .cb_arg = 4},
  /*  30 GETPLANE.format_type_ptr */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 4096, .len_kind = NVGPU_SLEN_COUNT, .len_a = 20, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_ALL_OR_NOTHING, .cb_off = 20, .cb_width = 4, .cb_arg = 4},
  /*  31 OBJ_GETPROPERTIES.props_ptr */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 4096, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 16, .cb_width = 4, .cb_arg = 4},
  /*  32 OBJ_GETPROPERTIES.prop_values_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 8192, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 8, .cb_kind = NVGPU_SCB_PARTIAL, .cb_off = 16, .cb_width = 4, .cb_arg = 8},
  /*  33 ATOMIC.objs_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 4},
  /*  34 ATOMIC.count_props_ptr */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 4, .len_width = 4, .len_elem = 4},
  /*  35 ATOMIC.props_ptr */
  {.off = 24, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 262144, .len_kind = NVGPU_SLEN_SUM, .len_a = 34, .len_elem = 4},
  /*  36 ATOMIC.prop_values_ptr */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524288, .len_kind = NVGPU_SLEN_SUM, .len_a = 34, .len_elem = 8},
  /*  37 CREATEPROPBLOB.data */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 1048576, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 1},
  /*  38 CREATE_LEASE.object_ids */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*  39 CREATE_LEASE.fd */
  {.off = 20, .kind = NVGPU_SF_FD_OUT, .width = 4},
  /*  40 LIST_LESSEES.lessees_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_width = 4, .cb_arg = 4},
  /*  41 GET_LEASE.objects_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 65536, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .len_elem = 4, .cb_kind = NVGPU_SCB_PARTIAL, .cb_width = 4, .cb_arg = 4},
  /*  42 NV_GRANT_PERMISSIONS.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  43 SYNCOBJ_HANDLE_TO_FD.fd */
  {.off = 8, .kind = NVGPU_SF_FD_OUT, .width = 4},
  /*  44 SYNCOBJ_FD_TO_HANDLE.fd */
  {.off = 8, .cond_off = 4, .cond_mask = 1, .cond_value = 1, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20u, .none_value = -1, .flags = NVGPU_SFF_COND},
  /*  45 SYNCOBJ_FD_TO_HANDLE.fd */
  {.off = 8, .cond_off = 4, .cond_mask = 1, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x40u, .none_value = -1, .flags = NVGPU_SFF_COND},
  /*  46 SYNCOBJ_WAIT.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  47 SYNCOBJ_RESET.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*  48 SYNCOBJ_SIGNAL.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*  49 SYNCOBJ_TIMELINE_WAIT.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 24, .len_width = 4, .len_elem = 4},
  /*  50 SYNCOBJ_TIMELINE_WAIT.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 24, .len_width = 4, .len_elem = 8},
  /*  51 SYNCOBJ_QUERY.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  52 SYNCOBJ_QUERY.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 8, .cb_kind = NVGPU_SCB_FULL},
  /*  53 SYNCOBJ_TIMELINE_SIGNAL.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  54 SYNCOBJ_TIMELINE_SIGNAL.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 8},
  /*  55 SYNCOBJ_EVENTFD.fd */
  {.off = 16, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x100u, .none_value = -1},
  /*  56 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 58, .nchild = 1},
  /*  57 NV_GEM_IMPORT_NVKMS_MEMORY.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  58 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  59 NV_GEM_EXPORT_NVKMS_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  60 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 4, .stride = 4, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 61, .nchild = 1},
  /*  61 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  62 NV_GEM_ALLOC_NVKMS_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  63 NV_GEM_EXPORT_DMABUF_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  64 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 4, .stride = 4, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 65, .nchild = 1},
  /*  65 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  66 NV_SEMSURF_FENCE_CTX_CREATE.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1},
  /*  67 NV_SEMSURF_FENCE_CTX_CREATE.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  68 NV_SEMSURF_FENCE_CREATE.fence_context_handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  69 NV_SEMSURF_FENCE_CREATE.fd */
  {.off = 16, .kind = NVGPU_SF_FD_OUT, .width = 4},
  /*  70 NV_SEMSURF_FENCE_WAIT.fence_context_handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  71 NV_SEMSURF_FENCE_WAIT.fd */
  {.off = 4, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20u, .none_value = -1},
  /*  72 NV_SEMSURF_FENCE_ATTACH.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  73 NV_SEMSURF_FENCE_ATTACH.fence_context_handle */
  {.off = 4, .kind = NVGPU_SF_GEM_IN, .width = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_drm_ioctls[] = {
  {.name = "GET_CAP", .cmd = 0xc010640cu, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "SET_CLIENT_CAP", .cmd = 0x4010640du, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "WAIT_VBLANK", .cmd = 0xc018643au, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "CRTC_GET_SEQUENCE", .cmd = 0xc018643bu, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "CRTC_QUEUE_SEQUENCE", .cmd = 0xc018643cu, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "GETRESOURCES", .cmd = 0xc04064a0u, .size = 64, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .nfield = 4},
  {.name = "GETCRTC", .cmd = 0xc06864a1u, .size = 104, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 4, .nfield = 1},
  {.name = "SETCRTC", .cmd = 0xc06864a2u, .size = 104, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 5, .nfield = 1},
  {.name = "CURSOR", .cmd = 0xc01c64a3u, .size = 28, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 6, .nfield = 1},
  {.name = "CURSOR2", .cmd = 0xc02464bbu, .size = 36, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 7, .nfield = 1},
  {.name = "GETGAMMA", .cmd = 0xc02064a4u, .size = 32, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 8, .nfield = 3},
  {.name = "SETGAMMA", .cmd = 0xc02064a5u, .size = 32, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 11, .nfield = 3},
  {.name = "GETENCODER", .cmd = 0xc01464a6u, .size = 20, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 14},
  {.name = "GETCONNECTOR", .cmd = 0xc05064a7u, .size = 80, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 14, .nfield = 4},
  {.name = "GETPROPERTY", .cmd = 0xc04064aau, .size = 64, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 18, .nfield = 2},
  {.name = "SETPROPERTY", .cmd = 0xc01064abu, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x40u, .field = 20},
  {.name = "GETPROPBLOB", .cmd = 0xc01064acu, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 20, .nfield = 1},
  {.name = "GETFB", .cmd = 0xc01c64adu, .size = 28, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x20u, .field = 21, .nfield = 1},
  {.name = "GETFB2", .cmd = 0xc06864ceu, .size = 104, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x20u, .field = 22, .nfield = 1},
  {.name = "ADDFB", .cmd = 0xc01c64aeu, .size = 28, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x8u, .field = 24, .nfield = 1},
  {.name = "ADDFB2", .cmd = 0xc06864b8u, .size = 104, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x88u, .field = 25, .nfield = 1},
  {.name = "RMFB", .cmd = 0xc00464afu, .size = 4, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x10u, .field = 27},
  {.name = "CLOSEFB", .cmd = 0xc00864d0u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x10u, .field = 27},
  {.name = "PAGE_FLIP", .cmd = 0xc01864b0u, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 27},
  {.name = "DIRTYFB", .cmd = 0xc01864b1u, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 27, .nfield = 1},
  {.name = "CREATE_DUMB", .cmd = 0xc02064b2u, .size = 32, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 28, .nfield = 1},
  {.name = "GETPLANERESOURCES", .cmd = 0xc01064b5u, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 29, .nfield = 1},
  {.name = "GETPLANE", .cmd = 0xc02064b6u, .size = 32, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 30, .nfield = 1},
  {.name = "SETPLANE", .cmd = 0xc03064b7u, .size = 48, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 31},
  {.name = "OBJ_GETPROPERTIES", .cmd = 0xc02064b9u, .size = 32, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 31, .nfield = 2},
  {.name = "OBJ_SETPROPERTY", .cmd = 0xc01864bau, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x40u, .field = 33},
  {.name = "ATOMIC", .cmd = 0xc03864bcu, .size = 56, .sclass = NVGPU_SCLASS_KMS, .special = NVGPU_SSPECIAL_ATOMIC, .flags = NVGPU_SIO_EXECUTOR, .field = 33, .nfield = 4},
  {.name = "CREATEPROPBLOB", .cmd = 0xc01064bdu, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 37, .nfield = 1},
  {.name = "DESTROYPROPBLOB", .cmd = 0xc00464beu, .size = 4, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 38},
  {.name = "CREATE_LEASE", .cmd = 0xc01864c6u, .size = 24, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 38, .nfield = 2},
  {.name = "LIST_LESSEES", .cmd = 0xc01064c7u, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 40, .nfield = 1},
  {.name = "GET_LEASE", .cmd = 0xc01064c8u, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 41, .nfield = 1},
  {.name = "REVOKE_LEASE", .cmd = 0xc00464c9u, .size = 4, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GET_CRTC_CRC32", .cmd = 0xc0086440u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GET_CRTC_CRC32_V2", .cmd = 0xc01c644cu, .size = 28, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GET_DPY_ID_FOR_CONNECTOR_ID", .cmd = 0xc0086450u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GET_CONNECTOR_ID_FOR_DPY_ID", .cmd = 0xc0086451u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GET_CLIENT_CAPABILITY", .cmd = 0xc0106448u, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .field = 42},
  {.name = "NV_GRANT_PERMISSIONS", .cmd = 0xc00c6452u, .size = 12, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x2u, .field = 42, .nfield = 1},
  {.name = "NV_REVOKE_PERMISSIONS", .cmd = 0xc0086453u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x4u, .field = 43},
  {.name = "SYNCOBJ_CREATE", .cmd = 0xc00864bfu, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 43},
  {.name = "SYNCOBJ_DESTROY", .cmd = 0xc00864c0u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 43},
  {.name = "SYNCOBJ_HANDLE_TO_FD", .cmd = 0xc01864c1u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 43, .nfield = 1},
  {.name = "SYNCOBJ_FD_TO_HANDLE", .cmd = 0xc01864c2u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 44, .nfield = 2},
  {.name = "SYNCOBJ_WAIT", .cmd = 0xc02864c3u, .size = 40, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 46, .nfield = 1},
  {.name = "SYNCOBJ_RESET", .cmd = 0xc01064c4u, .size = 16, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 47, .nfield = 1},
  {.name = "SYNCOBJ_SIGNAL", .cmd = 0xc01064c5u, .size = 16, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 48, .nfield = 1},
  {.name = "SYNCOBJ_TIMELINE_WAIT", .cmd = 0xc03064cau, .size = 48, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 49, .nfield = 2},
  {.name = "SYNCOBJ_QUERY", .cmd = 0xc01864cbu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 51, .nfield = 2},
  {.name = "SYNCOBJ_TRANSFER", .cmd = 0xc02064ccu, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 53},
  {.name = "SYNCOBJ_TIMELINE_SIGNAL", .cmd = 0xc01864cdu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 53, .nfield = 2},
  {.name = "SYNCOBJ_EVENTFD", .cmd = 0xc01864cfu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 55, .nfield = 1},
  {.name = "NV_GET_CRTC_CRC32", .cmd = 0xc0086440u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 56},
  {.name = "NV_GET_CRTC_CRC32_V2", .cmd = 0xc01c644cu, .size = 28, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 56},
  {.name = "NV_GET_DPY_ID_FOR_CONNECTOR_ID", .cmd = 0xc0086450u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 56},
  {.name = "NV_GET_CONNECTOR_ID_FOR_DPY_ID", .cmd = 0xc0086451u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 56},
  {.name = "NV_GEM_IMPORT_NVKMS_MEMORY", .cmd = 0xc0206441u, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .field = 56, .nfield = 2},
  {.name = "NV_GET_DEV_INFO", .cmd = 0xc0246443u, .size = 36, .sclass = NVGPU_SCLASS_RENDER, .field = 59},
  {.name = "NV_FENCE_SUPPORTED", .cmd = 0x00006444u, .sclass = NVGPU_SCLASS_RENDER, .field = 59},
  {.name = "NV_GEM_EXPORT_NVKMS_MEMORY", .cmd = 0xc0186449u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 59, .nfield = 2},
  {.name = "NV_GEM_ALLOC_NVKMS_MEMORY", .cmd = 0xc018644bu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 62, .nfield = 1},
  {.name = "NV_GEM_EXPORT_DMABUF_MEMORY", .cmd = 0xc018644du, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 63, .nfield = 2},
  {.name = "NV_DMABUF_SUPPORTED", .cmd = 0x0000644fu, .sclass = NVGPU_SCLASS_RENDER, .field = 66},
  {.name = "NV_SEMSURF_FENCE_CTX_CREATE", .cmd = 0xc0206454u, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 66, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_CREATE", .cmd = 0xc0186455u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 68, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_WAIT", .cmd = 0x40186456u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 70, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_ATTACH", .cmd = 0x40186457u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 72, .nfield = 2},
};

static const struct nvgpu_stable nvgpu_schema_drm = {
  .name = "drm", .vmin = 0, .vmax = 0,
  .ioctls = nvgpu_schema_drm_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_drm_ioctls),
  .fields = nvgpu_schema_drm_fields, .nfields = 74,
};

static const struct nvgpu_sfield nvgpu_schema_v610_57_04_example_fields[] = {
  /*   0 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
};

static const struct nvgpu_sioctl nvgpu_schema_v610_57_04_example_ioctls[] = {
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v610_57_04_example = {
  .name = "v610_57_04_example", .vmin = NVGPU_SCHEMA_VERSION(610, 57, 4), .vmax = NVGPU_SCHEMA_VERSION(610, 57, 4),
  .ioctls = nvgpu_schema_v610_57_04_example_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v610_57_04_example_ioctls),
  .fields = nvgpu_schema_v610_57_04_example_fields, .nfields = 1,
};

/* Set 0 has no NVKMS table; the rest one per range of host driver versions. */
static const struct nvgpu_schema_set nvgpu_schema_sets[] = {
  {.drm = &nvgpu_schema_drm},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v610_57_04_example},
};

#endif /* NVGPU_SCHEMA_TABLES */

#endif /* NVGPU_SCHEMA_H */
