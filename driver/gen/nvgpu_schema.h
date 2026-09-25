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
#define NVGPU_SFF_COND_NE (1u << 2)         /* cond: masked word != value */

/* Length rules (PTR); for an ARRAY, how many elements are the kernel's
 * (0: all `count`; COUNT: min(count, uN at len_a); PLANES: the table's
 * planes[] of the u32 format at len_a). */
#define NVGPU_SLEN_CONST 1        /* len_a bytes                            */
#define NVGPU_SLEN_COUNT 2        /* uN at len_a (width len_width) x elem   */
#define NVGPU_SLEN_SUM 3          /* sum of u32s of field len_a's buffer    */
#define NVGPU_SLEN_NVKMS_PARAMS 4 /* NvKmsIoctlParams.size, == max          */
#define NVGPU_SLEN_PLANES 5       /* ARRAY only, see above                  */

/* Copy-back rules. */
#define NVGPU_SCB_NONE 0
#define NVGPU_SCB_FULL 1
#define NVGPU_SCB_PARTIAL 2
#define NVGPU_SCB_ALL_OR_NOTHING 3
#define NVGPU_SCB_EXACT 4
#define NVGPU_SCB_RANGE 5
#define NVGPU_SCB_WRITTEN 6 /* [0, uN at cb_off as the host left it), always */

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
#define NVGPU_SPOL_NVKMS_EXACT (1u << 9)
#define NVGPU_SPOL_MASTER (1u << 10)

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
#define NVGPU_SCHEMA_MAX_DEPTH 4

#define NVGPU_NVKMS_IOCTL_IOWR 0xc0106d00u
/* NvKmsGetNextEventReply.valid, from the params block, in every table. */
#define NVGPU_NVKMS_NEXT_EVENT_VALID_OFF 8u

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
  const u8 *planes; /* numPlanes by NVKMS surface format (NVGPU_SLEN_PLANES) */
  u32 nplanes;
};

/* What a guest runs with: the DRM tables, and NVKMS's for the host version. */
struct nvgpu_schema_set {
  const struct nvgpu_stable *drm;
  const struct nvgpu_stable *modeset; /* NULL: no table for this host */
};

/*
 * A UVM command the backend lets through, and the size of its parameter
 * block on the host's release (gen/schema/uvm.py): UVM's ioctl numbers carry
 * none, and the host copies exactly this many bytes each way.
 */
struct nvgpu_uvm_cmd {
  u32 cmd; /* the whole number, as UVM's callers pass it */
  u32 size;
};

struct nvgpu_uvm_table {
  const char *name;
  u32 vmin, vmax; /* NVGPU_SCHEMA_VERSION */
  const struct nvgpu_uvm_cmd *cmds; /* by cmd */
  u32 ncmds;
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
  /*  43 NV_GRANT_PERMISSIONS_UNTYPED.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  44 SYNCOBJ_HANDLE_TO_FD.fd */
  {.off = 8, .kind = NVGPU_SF_FD_OUT, .width = 4},
  /*  45 SYNCOBJ_FD_TO_HANDLE.fd */
  {.off = 8, .cond_off = 4, .cond_mask = 1, .cond_value = 1, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20u, .none_value = -1, .flags = NVGPU_SFF_COND},
  /*  46 SYNCOBJ_FD_TO_HANDLE.fd */
  {.off = 8, .cond_off = 4, .cond_mask = 1, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x40u, .none_value = -1, .flags = NVGPU_SFF_COND},
  /*  47 SYNCOBJ_WAIT.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  48 SYNCOBJ_RESET.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*  49 SYNCOBJ_SIGNAL.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 4, .len_elem = 4},
  /*  50 SYNCOBJ_TIMELINE_WAIT.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 24, .len_width = 4, .len_elem = 4},
  /*  51 SYNCOBJ_TIMELINE_WAIT.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 24, .len_width = 4, .len_elem = 8},
  /*  52 SYNCOBJ_QUERY.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  53 SYNCOBJ_QUERY.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 8, .cb_kind = NVGPU_SCB_FULL},
  /*  54 SYNCOBJ_TIMELINE_SIGNAL.handles */
  {.kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16384, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4},
  /*  55 SYNCOBJ_TIMELINE_SIGNAL.points */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 32768, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 8},
  /*  56 SYNCOBJ_EVENTFD.fd */
  {.off = 16, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x100u, .none_value = -1},
  /*  57 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 59, .nchild = 1},
  /*  58 NV_GEM_IMPORT_NVKMS_MEMORY.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  59 NV_GEM_IMPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  60 NV_GEM_EXPORT_NVKMS_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  61 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 4, .stride = 4, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 62, .nchild = 1},
  /*  62 NV_GEM_EXPORT_NVKMS_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  63 NV_GEM_ALLOC_NVKMS_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  64 NV_GEM_EXPORT_DMABUF_MEMORY.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  65 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 4, .stride = 4, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1, .child = 66, .nchild = 1},
  /*  66 NV_GEM_EXPORT_DMABUF_MEMORY.nvkms_params_ptr.memFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10000u, .none_value = -1},
  /*  67 NV_SEMSURF_FENCE_CTX_CREATE.nvkms_params_ptr */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 8, .len_elem = 1},
  /*  68 NV_SEMSURF_FENCE_CTX_CREATE.handle */
  {.off = 24, .kind = NVGPU_SF_GEM_OUT, .width = 4},
  /*  69 NV_SEMSURF_FENCE_CREATE.fence_context_handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  70 NV_SEMSURF_FENCE_CREATE.fd */
  {.off = 16, .kind = NVGPU_SF_FD_OUT, .width = 4},
  /*  71 NV_SEMSURF_FENCE_WAIT.fence_context_handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  72 NV_SEMSURF_FENCE_WAIT.fd */
  {.off = 4, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20u, .none_value = -1},
  /*  73 NV_SEMSURF_FENCE_ATTACH.handle */
  {.kind = NVGPU_SF_GEM_IN, .width = 4},
  /*  74 NV_SEMSURF_FENCE_ATTACH.fence_context_handle */
  {.off = 4, .kind = NVGPU_SF_GEM_IN, .width = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_drm_ioctls[] = {
  {.name = "GET_CAP", .cmd = 0xc010640cu, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "SET_CLIENT_CAP", .cmd = 0x4010640du, .size = 16, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR},
  {.name = "SET_MASTER", .cmd = 0x0000641eu, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x400u},
  {.name = "DROP_MASTER", .cmd = 0x0000641fu, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x400u},
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
  {.name = "NV_GRANT_PERMISSIONS_UNTYPED", .cmd = 0xc0086452u, .size = 8, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x2u, .field = 43, .nfield = 1},
  {.name = "NV_REVOKE_PERMISSIONS_UNTYPED", .cmd = 0xc0046453u, .size = 4, .sclass = NVGPU_SCLASS_KMS, .flags = NVGPU_SIO_EXECUTOR, .policy = 0x4u, .field = 44},
  {.name = "SYNCOBJ_CREATE", .cmd = 0xc00864bfu, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 44},
  {.name = "SYNCOBJ_DESTROY", .cmd = 0xc00864c0u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 44},
  {.name = "SYNCOBJ_HANDLE_TO_FD", .cmd = 0xc01864c1u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 44, .nfield = 1},
  {.name = "SYNCOBJ_FD_TO_HANDLE", .cmd = 0xc01864c2u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 45, .nfield = 2},
  {.name = "SYNCOBJ_WAIT", .cmd = 0xc02864c3u, .size = 40, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 47, .nfield = 1},
  {.name = "SYNCOBJ_RESET", .cmd = 0xc01064c4u, .size = 16, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 48, .nfield = 1},
  {.name = "SYNCOBJ_SIGNAL", .cmd = 0xc01064c5u, .size = 16, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 49, .nfield = 1},
  {.name = "SYNCOBJ_TIMELINE_WAIT", .cmd = 0xc03064cau, .size = 48, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 50, .nfield = 2},
  {.name = "SYNCOBJ_QUERY", .cmd = 0xc01864cbu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 52, .nfield = 2},
  {.name = "SYNCOBJ_TRANSFER", .cmd = 0xc02064ccu, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 54},
  {.name = "SYNCOBJ_TIMELINE_SIGNAL", .cmd = 0xc01864cdu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 54, .nfield = 2},
  {.name = "SYNCOBJ_EVENTFD", .cmd = 0xc01864cfu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 56, .nfield = 1},
  {.name = "NV_GET_DPY_ID_FOR_CONNECTOR_ID", .cmd = 0xc0086450u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 57},
  {.name = "NV_GET_CONNECTOR_ID_FOR_DPY_ID", .cmd = 0xc0086451u, .size = 8, .sclass = NVGPU_SCLASS_RENDER, .flags = NVGPU_SIO_EXECUTOR, .field = 57},
  {.name = "NV_GEM_IMPORT_NVKMS_MEMORY", .cmd = 0xc0206441u, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .field = 57, .nfield = 2},
  {.name = "NV_GET_DEV_INFO", .cmd = 0xc0246443u, .size = 36, .sclass = NVGPU_SCLASS_RENDER, .field = 60},
  {.name = "NV_GET_DEV_INFO_V535", .cmd = 0xc0146443u, .size = 20, .sclass = NVGPU_SCLASS_RENDER, .field = 60},
  {.name = "NV_GET_DEV_INFO_V545", .cmd = 0xc01c6443u, .size = 28, .sclass = NVGPU_SCLASS_RENDER, .field = 60},
  {.name = "NV_GET_DEV_INFO_V550", .cmd = 0xc0206443u, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .field = 60},
  {.name = "NV_FENCE_SUPPORTED", .cmd = 0x00006444u, .sclass = NVGPU_SCLASS_RENDER, .field = 60},
  {.name = "NV_GEM_EXPORT_NVKMS_MEMORY", .cmd = 0xc0186449u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 60, .nfield = 2},
  {.name = "NV_GEM_ALLOC_NVKMS_MEMORY", .cmd = 0xc018644bu, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 63, .nfield = 1},
  {.name = "NV_GEM_EXPORT_DMABUF_MEMORY", .cmd = 0xc018644du, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .field = 64, .nfield = 2},
  {.name = "NV_DMABUF_SUPPORTED", .cmd = 0x0000644fu, .sclass = NVGPU_SCLASS_RENDER, .field = 67},
  {.name = "NV_SEMSURF_FENCE_CTX_CREATE", .cmd = 0xc0206454u, .size = 32, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 67, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_CREATE", .cmd = 0xc0186455u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 69, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_WAIT", .cmd = 0x40186456u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 71, .nfield = 2},
  {.name = "NV_SEMSURF_FENCE_ATTACH", .cmd = 0x40186457u, .size = 24, .sclass = NVGPU_SCLASS_RENDER, .policy = 0x1u, .field = 73, .nfield = 2},
};

static const struct nvgpu_stable nvgpu_schema_drm = {
  .name = "drm", .vmin = 0, .vmax = 0,
  .ioctls = nvgpu_schema_drm_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_drm_ioctls),
  .fields = nvgpu_schema_drm_fields, .nfields = 75,
};

static const u8 nvgpu_schema_v535_129_03_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v535_129_03_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1080, .stride = 1080, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 616, .cb_arg = 464},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 92, .stride = 92, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 80},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37096, .stride = 37096, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35024},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 696, .stride = 696, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 200, .cb_arg = 496, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 192, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 184, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 500, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 624, .stride = 624, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 272, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 264, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 256, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 424, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 94880, .stride = 94880, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 78936, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 9864, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 2464, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.lut.input.pRamps */
  {.off = 280, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.lut.output.pRamps */
  {.off = 296, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  22 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3104, .stride = 3104, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3080, .child = 23, .nchild = 1},
  /*  23 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 65792, .stride = 2056, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 2056},
  /*  24 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  25 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 26, .nchild = 1},
  /*  26 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 27, .nchild = 1},
  /*  27 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  28 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  29 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 30, .nchild = 1},
  /*  30 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  31 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 32, .nchild = 1},
  /*  32 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  33 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  34 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  35 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  36 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  37 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  38 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  39 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  40 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  41 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  42 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  43 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  44 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  45 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  46 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 47, .nchild = 1},
  /*  47 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  48 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  49 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  50 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  51 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 144, .stride = 144, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 140, .cb_arg = 4, .child = 52, .nchild = 1},
  /*  52 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  53 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 140, .stride = 140, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 136, .child = 54, .nchild = 1},
  /*  54 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  55 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 144, .stride = 144, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 140, .cb_arg = 4},
  /*  56 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  57 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  58 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  59 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 36, .cb_arg = 4},
  /*  60 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 62, .nchild = 1},
  /*  62 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 63, .nchild = 1},
  /*  63 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  64 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  65 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 66, .nchild = 1},
  /*  66 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  67 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 68, .nchild = 1},
  /*  68 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  69 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 70, .nchild = 1},
  /*  70 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  71 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  72 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  73 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  74 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  75 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  76 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 77, .nchild = 1},
  /*  77 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
};

static const struct nvgpu_sioctl nvgpu_schema_v535_129_03_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 21, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 22, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 24, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 25, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 28, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 29, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 31, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 33, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 34, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 35, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 36, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 38, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 39, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 40, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 41, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 42, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 43, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 44, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 45, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 46, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 36, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 48, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 49, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 50, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 51, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 53, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 55, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 56, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 57, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 58, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 59, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 60, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 61, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 64, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 65, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 67, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 69, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 71, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 72, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 73, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 56, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 74, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 75, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 76, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v535_129_03 = {
  .name = "v535_129_03", .vmin = NVGPU_SCHEMA_VERSION(535, 129, 3), .vmax = NVGPU_SCHEMA_VERSION(580, 178, 3),
  .ioctls = nvgpu_schema_v535_129_03_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v535_129_03_ioctls),
  .fields = nvgpu_schema_v535_129_03_fields, .nfields = 78,
  .planes = nvgpu_schema_v535_129_03_planes, .nplanes = 36,
};

static const u8 nvgpu_schema_v580_178_04_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v580_178_04_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1512, .stride = 1512, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 624, .cb_arg = 888},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 92, .stride = 92, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 80},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37160, .stride = 37160, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35088},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 720, .stride = 720, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 208, .cb_arg = 512, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 200, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 192, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 516, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 640, .stride = 640, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 288, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 280, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 272, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 440, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 171936, .stride = 171936, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 155992, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 19496, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 4872, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps */
  {.off = 360, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps */
  {.off = 376, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_CHECK_LUT_NOTIFIER.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 1},
  /*  22 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  23 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3112, .stride = 3112, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3084, .child = 24, .nchild = 1},
  /*  24 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 143616, .stride = 4488, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4488, .child = 25, .nchild = 2},
  /*  25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps */
  {.off = 88, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps */
  {.off = 104, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  28 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 29, .nchild = 1},
  /*  29 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 30, .nchild = 1},
  /*  30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  31 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  32 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 33, .nchild = 1},
  /*  33 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  34 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 35, .nchild = 1},
  /*  35 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  36 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  37 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  38 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  40 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  41 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  43 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  47 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  48 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  49 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 50, .nchild = 1},
  /*  50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  51 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  52 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  53 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  54 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 144, .stride = 144, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 140, .cb_arg = 4, .child = 55, .nchild = 1},
  /*  55 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  56 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 140, .stride = 140, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 136, .child = 57, .nchild = 1},
  /*  57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  58 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 144, .stride = 144, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 140, .cb_arg = 4},
  /*  59 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  62 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 36, .cb_arg = 4},
  /*  63 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  64 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 65, .nchild = 1},
  /*  65 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 66, .nchild = 1},
  /*  66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  67 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 69, .nchild = 1},
  /*  69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  70 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 71, .nchild = 1},
  /*  71 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  72 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 73, .nchild = 1},
  /*  73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  74 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  75 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  76 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  79 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 80, .nchild = 1},
  /*  80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  81 NVKMS_SET_FLIPLOCK_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 328, .stride = 328, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 324, .cb_arg = 4},
  /*  82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_v580_178_04_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_CHECK_LUT_NOTIFIER", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 21, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 22, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 23, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 27, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 28, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 31, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 32, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 34, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 36, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 38, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 39, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 40, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 41, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 42, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 43, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 44, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 45, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 46, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 47, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 48, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 34, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 49, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 51, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 52, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 53, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 54, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 56, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 58, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 59, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 60, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 61, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 62, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 63, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 64, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 67, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 68, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 70, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 72, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 74, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 75, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 55, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 76, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 77, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 78, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 59, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 79, .nfield = 1},
  {.name = "NVKMS_SET_FLIPLOCK_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 60, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 81, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 61, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 82, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 62, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 83, .nfield = 1},
  {.name = "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", .cmd = 0xc0106d00u, .nvkms_cmd = 63, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 84, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v580_178_04 = {
  .name = "v580_178_04", .vmin = NVGPU_SCHEMA_VERSION(580, 178, 4), .vmax = NVGPU_SCHEMA_VERSION(595, 71, 4),
  .ioctls = nvgpu_schema_v580_178_04_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v580_178_04_ioctls),
  .fields = nvgpu_schema_v580_178_04_fields, .nfields = 85,
  .planes = nvgpu_schema_v580_178_04_planes, .nplanes = 36,
};

static const u8 nvgpu_schema_v595_71_05_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v595_71_05_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1512, .stride = 1512, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 624, .cb_arg = 888},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 92, .stride = 92, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 80},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37168, .stride = 37168, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35096},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 736, .stride = 736, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 224, .cb_arg = 512, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 216, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 208, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 532, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 656, .stride = 656, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 304, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 296, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 288, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 456, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 171936, .stride = 171936, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 155992, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 19496, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 4872, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps */
  {.off = 360, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps */
  {.off = 376, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_CHECK_LUT_NOTIFIER.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 1},
  /*  22 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  23 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3104, .stride = 3104, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3080, .child = 24, .nchild = 1},
  /*  24 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 143616, .stride = 4488, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4488, .child = 25, .nchild = 2},
  /*  25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps */
  {.off = 88, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps */
  {.off = 104, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  28 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 29, .nchild = 1},
  /*  29 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 30, .nchild = 1},
  /*  30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  31 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  32 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 33, .nchild = 1},
  /*  33 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  34 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 35, .nchild = 1},
  /*  35 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  36 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  37 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  38 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  40 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  41 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  43 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  47 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  48 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  49 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 50, .nchild = 1},
  /*  50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  51 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  52 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  53 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  54 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4, .child = 55, .nchild = 1},
  /*  55 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  56 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 24, .child = 57, .nchild = 1},
  /*  57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  58 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4},
  /*  59 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  62 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  63 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  64 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 65, .nchild = 1},
  /*  65 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 66, .nchild = 1},
  /*  66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  67 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 69, .nchild = 1},
  /*  69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  70 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 71, .nchild = 1},
  /*  71 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  72 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 73, .nchild = 1},
  /*  73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  74 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  75 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  76 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  79 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 80, .nchild = 1},
  /*  80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  81 NVKMS_SET_FLIPLOCK_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 68, .cb_arg = 4},
  /*  82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_v595_71_05_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_CHECK_LUT_NOTIFIER", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 21, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 22, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 23, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 27, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 28, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 31, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 32, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 34, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 36, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 38, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 39, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 40, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 41, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 42, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 43, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 44, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 45, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 46, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 47, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 48, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 34, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 49, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 51, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 52, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 53, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 54, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 56, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 58, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 59, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 60, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 61, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 62, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 63, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 64, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 67, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 68, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 70, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 72, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 74, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 75, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 55, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 76, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 56, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 77, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 78, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 79, .nfield = 1},
  {.name = "NVKMS_SET_FLIPLOCK_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 59, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 81, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 60, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 82, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 61, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 83, .nfield = 1},
  {.name = "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", .cmd = 0xc0106d00u, .nvkms_cmd = 62, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 84, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v595_71_05 = {
  .name = "v595_71_05", .vmin = NVGPU_SCHEMA_VERSION(595, 71, 5), .vmax = NVGPU_SCHEMA_VERSION(595, 99, 1),
  .ioctls = nvgpu_schema_v595_71_05_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v595_71_05_ioctls),
  .fields = nvgpu_schema_v595_71_05_fields, .nfields = 85,
  .planes = nvgpu_schema_v595_71_05_planes, .nplanes = 36,
};

static const u8 nvgpu_schema_v595_99_02_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v595_99_02_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1512, .stride = 1512, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 624, .cb_arg = 888},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 92, .stride = 92, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 80},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37168, .stride = 37168, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35096},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 736, .stride = 736, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 224, .cb_arg = 512, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 216, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 208, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 532, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 656, .stride = 656, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 304, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 296, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 288, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 456, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 171936, .stride = 171936, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 155992, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 19496, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 4872, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps */
  {.off = 360, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps */
  {.off = 376, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_CHECK_LUT_NOTIFIER.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 1},
  /*  22 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  23 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3104, .stride = 3104, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3080, .child = 24, .nchild = 1},
  /*  24 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 143616, .stride = 4488, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4488, .child = 25, .nchild = 2},
  /*  25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps */
  {.off = 88, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps */
  {.off = 104, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  28 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 29, .nchild = 1},
  /*  29 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 30, .nchild = 1},
  /*  30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  31 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  32 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 33, .nchild = 1},
  /*  33 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  34 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 35, .nchild = 1},
  /*  35 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  36 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  37 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  38 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  40 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  41 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  43 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  47 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  48 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  49 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 50, .nchild = 1},
  /*  50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  51 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  52 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  53 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  54 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4, .child = 55, .nchild = 1},
  /*  55 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  56 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 24, .child = 57, .nchild = 1},
  /*  57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  58 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4},
  /*  59 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  62 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  63 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  64 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 65, .nchild = 1},
  /*  65 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 66, .nchild = 1},
  /*  66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  67 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 69, .nchild = 1},
  /*  69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  70 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 71, .nchild = 1},
  /*  71 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  72 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 73, .nchild = 1},
  /*  73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  74 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  75 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  76 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  79 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 80, .nchild = 1},
  /*  80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  81 NVKMS_SET_FLIPLOCK_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 68, .cb_arg = 4},
  /*  82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_v595_99_02_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_CHECK_LUT_NOTIFIER", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 21, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 22, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 23, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 27, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 28, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 31, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 32, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 34, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 36, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 38, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 39, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 40, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 41, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 42, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 43, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 44, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 45, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 46, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 47, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 48, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 34, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 49, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 51, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 52, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 53, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 54, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 56, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 58, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 59, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 60, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 61, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 62, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 63, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 64, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 67, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 68, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 70, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 72, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 74, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 75, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 55, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 76, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 56, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 77, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 78, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 79, .nfield = 1},
  {.name = "NVKMS_SET_FLIPLOCK_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 59, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 81, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 60, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 82, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 61, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 83, .nfield = 1},
  {.name = "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", .cmd = 0xc0106d00u, .nvkms_cmd = 62, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 84, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v595_99_02 = {
  .name = "v595_99_02", .vmin = NVGPU_SCHEMA_VERSION(595, 99, 2), .vmax = NVGPU_SCHEMA_VERSION(610, 57, 3),
  .ioctls = nvgpu_schema_v595_99_02_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v595_99_02_ioctls),
  .fields = nvgpu_schema_v595_99_02_fields, .nfields = 85,
  .planes = nvgpu_schema_v595_99_02_planes, .nplanes = 36,
};

static const u8 nvgpu_schema_v610_57_04_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v610_57_04_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1440, .stride = 1440, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 624, .cb_arg = 816},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 96, .stride = 96, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 84},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37168, .stride = 37168, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35096},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 736, .stride = 736, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 224, .cb_arg = 512, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 216, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 208, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 532, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 656, .stride = 656, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 304, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 296, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 288, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 456, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 186784, .stride = 186784, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 170840, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 21352, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 5336, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps */
  {.off = 360, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps */
  {.off = 376, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_CHECK_LUT_NOTIFIER.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 1},
  /*  22 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  23 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3104, .stride = 3104, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3080, .child = 24, .nchild = 1},
  /*  24 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 158464, .stride = 4952, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4952, .child = 25, .nchild = 2},
  /*  25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps */
  {.off = 88, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  26 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps */
  {.off = 104, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  27 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  28 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 29, .nchild = 1},
  /*  29 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 30, .nchild = 1},
  /*  30 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  31 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  32 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 33, .nchild = 1},
  /*  33 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  34 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 35, .nchild = 1},
  /*  35 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  36 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  37 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  38 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  39 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  40 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  41 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  42 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  43 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  44 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  45 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  46 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  47 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  48 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  49 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 50, .nchild = 1},
  /*  50 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  51 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  52 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  53 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  54 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4, .child = 55, .nchild = 1},
  /*  55 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  56 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 24, .child = 57, .nchild = 1},
  /*  57 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  58 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4},
  /*  59 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  60 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  62 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  63 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  64 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 65, .nchild = 1},
  /*  65 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 66, .nchild = 1},
  /*  66 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  67 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 69, .nchild = 1},
  /*  69 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  70 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 71, .nchild = 1},
  /*  71 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  72 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 73, .nchild = 1},
  /*  73 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  74 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  75 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  76 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  77 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  78 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  79 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 80, .nchild = 1},
  /*  80 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  81 NVKMS_SET_FLIPLOCK_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 68, .cb_arg = 4},
  /*  82 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  83 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  84 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_v610_57_04_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_CHECK_LUT_NOTIFIER", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 21, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 22, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 23, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 27, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 28, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 31, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 32, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 34, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 36, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 38, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 39, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 40, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 41, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 42, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 43, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 44, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 45, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 46, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 47, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 48, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 34, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 49, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 51, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 52, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 53, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 54, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 56, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 58, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 59, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 60, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 61, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 62, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 63, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 64, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 67, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 68, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 70, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 72, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 74, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 75, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 55, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 76, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 56, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 77, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 78, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 79, .nfield = 1},
  {.name = "NVKMS_SET_FLIPLOCK_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 59, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 81, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 60, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 82, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 61, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 83, .nfield = 1},
  {.name = "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", .cmd = 0xc0106d00u, .nvkms_cmd = 62, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x300u, .field = 84, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v610_57_04 = {
  .name = "v610_57_04", .vmin = NVGPU_SCHEMA_VERSION(610, 57, 4), .vmax = NVGPU_SCHEMA_VERSION(615, 71, 8),
  .ioctls = nvgpu_schema_v610_57_04_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v610_57_04_ioctls),
  .fields = nvgpu_schema_v610_57_04_fields, .nfields = 85,
  .planes = nvgpu_schema_v610_57_04_planes, .nplanes = 36,
};

static const u8 nvgpu_schema_v615_71_09_planes[] = {
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 1,
};

static const struct nvgpu_sfield nvgpu_schema_v615_71_09_fields[] = {
  /*   0 NVKMS_ALLOC_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1448, .stride = 1448, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 624, .cb_arg = 824},
  /*   1 NVKMS_FREE_DEVICE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*   2 NVKMS_QUERY_DISP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 172, .stride = 172, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 164},
  /*   3 NVKMS_QUERY_CONNECTOR_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 44, .stride = 44, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 32},
  /*   4 NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*   5 NVKMS_QUERY_DPY_STATIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 96, .stride = 96, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 84},
  /*   6 NVKMS_QUERY_DPY_DYNAMIC_DATA.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 37168, .stride = 37168, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2072, .cb_arg = 35096},
  /*   7 NVKMS_VALIDATE_MODE_INDEX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 736, .stride = 736, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 224, .cb_arg = 512, .child = 8, .nchild = 1},
  /*   8 NVKMS_VALIDATE_MODE_INDEX.address.request.pInfoString */
  {.off = 216, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 208, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 532, .cb_width = 4},
  /*   9 NVKMS_VALIDATE_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 656, .stride = 656, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 304, .cb_arg = 352, .child = 10, .nchild = 1},
  /*  10 NVKMS_VALIDATE_MODE.address.request.pInfoString */
  {.off = 296, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_OUT, .max = 2048, .len_kind = NVGPU_SLEN_COUNT, .len_a = 288, .len_width = 4, .len_elem = 1, .cb_kind = NVGPU_SCB_WRITTEN, .cb_off = 456, .cb_width = 4},
  /*  11 NVKMS_SET_MODE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 187808, .stride = 187808, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 171864, .cb_arg = 15944, .child = 12, .nchild = 1},
  /*  12 NVKMS_SET_MODE.address.request.disp */
  {.off = 16, .kind = NVGPU_SF_ARRAY, .stride = 21480, .count = 8, .child = 13, .nchild = 1},
  /*  13 NVKMS_SET_MODE.address.request.disp.head */
  {.off = 8, .kind = NVGPU_SF_ARRAY, .stride = 5368, .count = 4, .child = 14, .nchild = 2},
  /*  14 NVKMS_SET_MODE.address.request.disp.head.flip.lut.input.pRamps */
  {.off = 360, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  15 NVKMS_SET_MODE.address.request.disp.head.flip.lut.output.pRamps */
  {.off = 376, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  16 NVKMS_SET_CURSOR_IMAGE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 52, .stride = 52, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 48, .cb_arg = 4},
  /*  17 NVKMS_MOVE_CURSOR.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  18 NVKMS_SET_LUT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 64, .cb_arg = 4, .child = 19, .nchild = 2},
  /*  19 NVKMS_SET_LUT.address.request.common.input.pRamps */
  {.off = 32, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  20 NVKMS_SET_LUT.address.request.common.output.pRamps */
  {.off = 48, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  21 NVKMS_IDLE_BASE_CHANNEL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 16},
  /*  22 NVKMS_FLIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 3104, .stride = 3104, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 3080, .child = 23, .nchild = 1},
  /*  23 NVKMS_FLIP.address.request.pFlipHead */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 159488, .stride = 4984, .len_kind = NVGPU_SLEN_COUNT, .len_a = 16, .len_width = 4, .len_elem = 4984, .child = 24, .nchild = 2},
  /*  24 NVKMS_FLIP.address.request.pFlipHead.flip.lut.input.pRamps */
  {.off = 88, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  25 NVKMS_FLIP.address.request.pFlipHead.flip.lut.output.pRamps */
  {.off = 104, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 6144, .stride = 6144, .len_kind = NVGPU_SLEN_CONST, .len_a = 6144},
  /*  26 NVKMS_DECLARE_DYNAMIC_DPY_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  27 NVKMS_REGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 152, .stride = 152, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 144, .cb_arg = 4, .child = 28, .nchild = 1},
  /*  28 NVKMS_REGISTER_SURFACE.address.request.planes */
  {.off = 16, .cond_off = 4, .cond_mask = 255, .kind = NVGPU_SF_ARRAY, .stride = 32, .count = 3, .len_kind = NVGPU_SLEN_PLANES, .len_a = 124, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE, .child = 29, .nchild = 1},
  /*  29 NVKMS_REGISTER_SURFACE.address.request.planes.u.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x10080u, .none_value = -1},
  /*  30 NVKMS_UNREGISTER_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  31 NVKMS_GRANT_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 32, .nchild = 1},
  /*  32 NVKMS_GRANT_SURFACE.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  33 NVKMS_ACQUIRE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 34, .nchild = 1},
  /*  34 NVKMS_ACQUIRE_SURFACE.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  35 NVKMS_RELEASE_SURFACE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  36 NVKMS_SET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  37 NVKMS_GET_DPY_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  38 NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  39 NVKMS_SET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  40 NVKMS_GET_DISP_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 8},
  /*  41 NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 40, .stride = 40, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 24},
  /*  42 NVKMS_QUERY_FRAMELOCK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 16},
  /*  43 NVKMS_SET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  44 NVKMS_GET_FRAMELOCK_ATTRIBUTE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 8},
  /*  45 NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 24},
  /*  46 NVKMS_GET_NEXT_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 48, .stride = 48, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 40},
  /*  47 NVKMS_DECLARE_EVENT_INTEREST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  48 NVKMS_CLEAR_UNICAST_EVENT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4, .child = 49, .nchild = 1},
  /*  49 NVKMS_CLEAR_UNICAST_EVENT.address.request.unicastEventFd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  50 NVKMS_SET_LAYER_POSITION.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1196, .stride = 1196, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1192, .cb_arg = 4},
  /*  51 NVKMS_GRAB_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  52 NVKMS_RELEASE_OWNERSHIP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 8, .stride = 8, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 4},
  /*  53 NVKMS_GRANT_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4, .child = 54, .nchild = 1},
  /*  54 NVKMS_GRANT_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  55 NVKMS_ACQUIRE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 28, .stride = 28, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 24, .child = 56, .nchild = 1},
  /*  56 NVKMS_ACQUIRE_PERMISSIONS.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  57 NVKMS_REVOKE_PERMISSIONS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 28, .cb_arg = 4},
  /*  58 NVKMS_QUERY_DPY_CRC32.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 36, .stride = 36, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 24},
  /*  59 NVKMS_REGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  60 NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  61 NVKMS_ALLOC_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  62 NVKMS_FREE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  63 NVKMS_JOIN_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 2568, .stride = 2568, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 2564, .cb_arg = 4, .child = 64, .nchild = 1},
  /*  64 NVKMS_JOIN_SWAP_GROUP.address.request.member */
  {.off = 4, .kind = NVGPU_SF_ARRAY, .stride = 20, .count = 128, .len_kind = NVGPU_SLEN_COUNT, .len_width = 4, .child = 65, .nchild = 1},
  /*  65 NVKMS_JOIN_SWAP_GROUP.address.request.member.unicastEvent.fd */
  {.off = 12, .cond_off = 16, .cond_mask = 255, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1, .flags = NVGPU_SFF_COND | NVGPU_SFF_COND_NE},
  /*  66 NVKMS_LEAVE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 1032, .stride = 1032, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 1028, .cb_arg = 4},
  /*  67 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4, .child = 68, .nchild = 1},
  /*  68 NVKMS_SET_SWAP_GROUP_CLIP_LIST.address.request.pClipList */
  {.off = 16, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_IN, .max = 524280, .stride = 8, .len_kind = NVGPU_SLEN_COUNT, .len_a = 8, .len_width = 2, .len_elem = 8},
  /*  69 NVKMS_GRANT_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4, .child = 70, .nchild = 1},
  /*  70 NVKMS_GRANT_SWAP_GROUP.address.request.fd */
  {.off = 8, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  71 NVKMS_ACQUIRE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 4, .cb_arg = 8, .child = 72, .nchild = 1},
  /*  72 NVKMS_ACQUIRE_SWAP_GROUP.address.request.fd */
  {.kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  73 NVKMS_RELEASE_SWAP_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 12, .stride = 12, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 8, .cb_arg = 4},
  /*  74 NVKMS_SWITCH_MUX.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 24, .stride = 24, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 20, .cb_arg = 4},
  /*  75 NVKMS_GET_MUX_STATE.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  76 NVKMS_ENABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 8},
  /*  77 NVKMS_DISABLE_VBLANK_SYNC_OBJECT.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4},
  /*  78 NVKMS_NOTIFY_VBLANK.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 20, .stride = 20, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 16, .cb_arg = 4, .child = 79, .nchild = 1},
  /*  79 NVKMS_NOTIFY_VBLANK.address.request.unicastEvent.fd */
  {.off = 12, .kind = NVGPU_SF_FD_IN, .width = 4, .kinds = 0x20000u, .none_value = -1},
  /*  80 NVKMS_SET_FLIPLOCK_GROUP.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 72, .stride = 72, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 68, .cb_arg = 4},
  /*  81 NVKMS_ENABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 32, .stride = 32, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 24, .cb_arg = 4},
  /*  82 NVKMS_DISABLE_VBLANK_SEM_CONTROL.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
  /*  83 NVKMS_ACCEL_VBLANK_SEM_CONTROLS.address */
  {.off = 8, .kind = NVGPU_SF_PTR, .width = 8, .dir = NVGPU_SDIR_INOUT, .max = 16, .stride = 16, .len_kind = NVGPU_SLEN_NVKMS_PARAMS, .cb_kind = NVGPU_SCB_RANGE, .cb_off = 12, .cb_arg = 4},
};

static const struct nvgpu_sioctl nvgpu_schema_v615_71_09_ioctls[] = {
  {.name = "NVKMS_ALLOC_DEVICE", .cmd = 0xc0106d00u, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .nfield = 1},
  {.name = "NVKMS_FREE_DEVICE", .cmd = 0xc0106d00u, .nvkms_cmd = 1, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 1, .nfield = 1},
  {.name = "NVKMS_QUERY_DISP", .cmd = 0xc0106d00u, .nvkms_cmd = 2, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 2, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 3, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 3, .nfield = 1},
  {.name = "NVKMS_QUERY_CONNECTOR_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 4, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 4, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_STATIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 5, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 5, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_DYNAMIC_DATA", .cmd = 0xc0106d00u, .nvkms_cmd = 6, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 6, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE_INDEX", .cmd = 0xc0106d00u, .nvkms_cmd = 7, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 7, .nfield = 1},
  {.name = "NVKMS_VALIDATE_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 8, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 9, .nfield = 1},
  {.name = "NVKMS_SET_MODE", .cmd = 0xc0106d00u, .nvkms_cmd = 9, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 11, .nfield = 1},
  {.name = "NVKMS_SET_CURSOR_IMAGE", .cmd = 0xc0106d00u, .nvkms_cmd = 10, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 16, .nfield = 1},
  {.name = "NVKMS_MOVE_CURSOR", .cmd = 0xc0106d00u, .nvkms_cmd = 11, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 17, .nfield = 1},
  {.name = "NVKMS_SET_LUT", .cmd = 0xc0106d00u, .nvkms_cmd = 12, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 18, .nfield = 1},
  {.name = "NVKMS_IDLE_BASE_CHANNEL", .cmd = 0xc0106d00u, .nvkms_cmd = 13, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 21, .nfield = 1},
  {.name = "NVKMS_FLIP", .cmd = 0xc0106d00u, .nvkms_cmd = 14, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 22, .nfield = 1},
  {.name = "NVKMS_DECLARE_DYNAMIC_DPY_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 15, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 26, .nfield = 1},
  {.name = "NVKMS_REGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 16, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 27, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 17, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 30, .nfield = 1},
  {.name = "NVKMS_GRANT_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 18, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 31, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 19, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 33, .nfield = 1},
  {.name = "NVKMS_RELEASE_SURFACE", .cmd = 0xc0106d00u, .nvkms_cmd = 20, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 35, .nfield = 1},
  {.name = "NVKMS_SET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 21, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 36, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 22, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 37, .nfield = 1},
  {.name = "NVKMS_GET_DPY_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 23, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 38, .nfield = 1},
  {.name = "NVKMS_SET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 24, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 39, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 25, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 40, .nfield = 1},
  {.name = "NVKMS_GET_DISP_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 26, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 41, .nfield = 1},
  {.name = "NVKMS_QUERY_FRAMELOCK", .cmd = 0xc0106d00u, .nvkms_cmd = 27, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 42, .nfield = 1},
  {.name = "NVKMS_SET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 28, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 43, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE", .cmd = 0xc0106d00u, .nvkms_cmd = 29, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 44, .nfield = 1},
  {.name = "NVKMS_GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES", .cmd = 0xc0106d00u, .nvkms_cmd = 30, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 45, .nfield = 1},
  {.name = "NVKMS_GET_NEXT_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 31, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 46, .nfield = 1},
  {.name = "NVKMS_DECLARE_EVENT_INTEREST", .cmd = 0xc0106d00u, .nvkms_cmd = 32, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 47, .nfield = 1},
  {.name = "NVKMS_CLEAR_UNICAST_EVENT", .cmd = 0xc0106d00u, .nvkms_cmd = 33, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 48, .nfield = 1},
  {.name = "NVKMS_SET_LAYER_POSITION", .cmd = 0xc0106d00u, .nvkms_cmd = 36, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 50, .nfield = 1},
  {.name = "NVKMS_GRAB_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 37, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 51, .nfield = 1},
  {.name = "NVKMS_RELEASE_OWNERSHIP", .cmd = 0xc0106d00u, .nvkms_cmd = 38, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 52, .nfield = 1},
  {.name = "NVKMS_GRANT_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 39, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 53, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 40, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 55, .nfield = 1},
  {.name = "NVKMS_REVOKE_PERMISSIONS", .cmd = 0xc0106d00u, .nvkms_cmd = 41, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 57, .nfield = 1},
  {.name = "NVKMS_QUERY_DPY_CRC32", .cmd = 0xc0106d00u, .nvkms_cmd = 42, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 58, .nfield = 1},
  {.name = "NVKMS_REGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 43, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 59, .nfield = 1},
  {.name = "NVKMS_UNREGISTER_DEFERRED_REQUEST_FIFO", .cmd = 0xc0106d00u, .nvkms_cmd = 44, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 60, .nfield = 1},
  {.name = "NVKMS_ALLOC_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 45, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 61, .nfield = 1},
  {.name = "NVKMS_FREE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 46, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 62, .nfield = 1},
  {.name = "NVKMS_JOIN_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 47, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 63, .nfield = 1},
  {.name = "NVKMS_LEAVE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 48, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 66, .nfield = 1},
  {.name = "NVKMS_SET_SWAP_GROUP_CLIP_LIST", .cmd = 0xc0106d00u, .nvkms_cmd = 49, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 67, .nfield = 1},
  {.name = "NVKMS_GRANT_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 50, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 69, .nfield = 1},
  {.name = "NVKMS_ACQUIRE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 51, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 71, .nfield = 1},
  {.name = "NVKMS_RELEASE_SWAP_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 52, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 73, .nfield = 1},
  {.name = "NVKMS_SWITCH_MUX", .cmd = 0xc0106d00u, .nvkms_cmd = 53, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 74, .nfield = 1},
  {.name = "NVKMS_GET_MUX_STATE", .cmd = 0xc0106d00u, .nvkms_cmd = 54, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 75, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 55, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 76, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SYNC_OBJECT", .cmd = 0xc0106d00u, .nvkms_cmd = 56, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 77, .nfield = 1},
  {.name = "NVKMS_NOTIFY_VBLANK", .cmd = 0xc0106d00u, .nvkms_cmd = 57, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 78, .nfield = 1},
  {.name = "NVKMS_SET_FLIPLOCK_GROUP", .cmd = 0xc0106d00u, .nvkms_cmd = 58, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 80, .nfield = 1},
  {.name = "NVKMS_ENABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 59, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 81, .nfield = 1},
  {.name = "NVKMS_DISABLE_VBLANK_SEM_CONTROL", .cmd = 0xc0106d00u, .nvkms_cmd = 60, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 82, .nfield = 1},
  {.name = "NVKMS_ACCEL_VBLANK_SEM_CONTROLS", .cmd = 0xc0106d00u, .nvkms_cmd = 61, .size = 16, .sclass = NVGPU_SCLASS_MODESET, .special = NVGPU_SSPECIAL_NVKMS_PARAMS, .flags = NVGPU_SIO_EXECUTOR | NVGPU_SIO_ARG_IN_ONLY, .policy = 0x100u, .field = 83, .nfield = 1},
};

static const struct nvgpu_stable nvgpu_schema_v615_71_09 = {
  .name = "v615_71_09", .vmin = NVGPU_SCHEMA_VERSION(615, 71, 9), .vmax = NVGPU_SCHEMA_VERSION(999, 999, 999),
  .ioctls = nvgpu_schema_v615_71_09_ioctls, .nioctls = ARRAY_SIZE(nvgpu_schema_v615_71_09_ioctls),
  .fields = nvgpu_schema_v615_71_09_fields, .nfields = 84,
  .planes = nvgpu_schema_v615_71_09_planes, .nplanes = 36,
};

/* Set 0 has no NVKMS table; the rest one per range of host driver versions. */
static const struct nvgpu_schema_set nvgpu_schema_sets[] = {
  {.drm = &nvgpu_schema_drm},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v535_129_03},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v580_178_04},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v595_71_05},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v595_99_02},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v610_57_04},
  {.drm = &nvgpu_schema_drm, .modeset = &nvgpu_schema_v615_71_09},
};

static const struct nvgpu_uvm_cmd nvgpu_uvm_v535_129_03[] = {
  {0x00000019u, 32}, /* REGISTER_GPU_VASPACE */
  {0x0000001au, 20}, /* UNREGISTER_GPU_VASPACE */
  {0x0000001bu, 56}, /* REGISTER_CHANNEL */
  {0x0000001cu, 28}, /* UNREGISTER_CHANNEL */
  {0x0000001du, 36}, /* ENABLE_PEER_ACCESS */
  {0x0000001eu, 36}, /* DISABLE_PEER_ACCESS */
  {0x00000021u, 1200}, /* MAP_EXTERNAL_ALLOCATION */
  {0x00000022u, 24}, /* FREE */
  {0x00000025u, 40}, /* REGISTER_GPU */
  {0x00000026u, 20}, /* UNREGISTER_GPU */
  {0x00000027u, 8}, /* PAGEABLE_MEM_ACCESS */
  {0x0000002au, 40}, /* SET_PREFERRED_LOCATION */
  {0x0000002bu, 24}, /* UNSET_PREFERRED_LOCATION */
  {0x0000002cu, 24}, /* ENABLE_READ_DUPLICATION */
  {0x0000002du, 24}, /* DISABLE_READ_DUPLICATION */
  {0x0000002eu, 40}, /* SET_ACCESSED_BY */
  {0x0000002fu, 40}, /* UNSET_ACCESSED_BY */
  {0x00000033u, 80}, /* MIGRATE */
  {0x00000041u, 40}, /* MAP_DYNAMIC_PARALLELISM_REGION */
  {0x00000042u, 40}, /* UNMAP_EXTERNAL */
  {0x00000043u, 4}, /* TOOLS_FLUSH_EVENTS */
  {0x00000044u, 1184}, /* ALLOC_SEMAPHORE_POOL */
  {0x00000045u, 4}, /* CLEAN_UP_ZOMBIE_RESOURCES */
  {0x00000046u, 24}, /* PAGEABLE_MEM_ACCESS_ON_GPU */
  {0x00000048u, 24}, /* VALIDATE_VA_RANGE */
  {0x00000049u, 24}, /* CREATE_EXTERNAL_RANGE */
  {0x0000004au, 40}, /* MAP_EXTERNAL_SPARSE */
  {0x0000004bu, 8}, /* MM_INITIALIZE */
  {0x30000001u, 16}, /* INITIALIZE */
  {0x30000002u, 0}, /* DEINITIALIZE */
};

static const struct nvgpu_uvm_cmd nvgpu_uvm_v550_40_53[] = {
  {0x00000019u, 32}, /* REGISTER_GPU_VASPACE */
  {0x0000001au, 20}, /* UNREGISTER_GPU_VASPACE */
  {0x0000001bu, 56}, /* REGISTER_CHANNEL */
  {0x0000001cu, 28}, /* UNREGISTER_CHANNEL */
  {0x0000001du, 36}, /* ENABLE_PEER_ACCESS */
  {0x0000001eu, 36}, /* DISABLE_PEER_ACCESS */
  {0x00000021u, 9264}, /* MAP_EXTERNAL_ALLOCATION */
  {0x00000022u, 24}, /* FREE */
  {0x00000025u, 40}, /* REGISTER_GPU */
  {0x00000026u, 20}, /* UNREGISTER_GPU */
  {0x00000027u, 8}, /* PAGEABLE_MEM_ACCESS */
  {0x0000002au, 40}, /* SET_PREFERRED_LOCATION */
  {0x0000002bu, 24}, /* UNSET_PREFERRED_LOCATION */
  {0x0000002cu, 24}, /* ENABLE_READ_DUPLICATION */
  {0x0000002du, 24}, /* DISABLE_READ_DUPLICATION */
  {0x0000002eu, 40}, /* SET_ACCESSED_BY */
  {0x0000002fu, 40}, /* UNSET_ACCESSED_BY */
  {0x00000033u, 80}, /* MIGRATE */
  {0x00000041u, 40}, /* MAP_DYNAMIC_PARALLELISM_REGION */
  {0x00000042u, 40}, /* UNMAP_EXTERNAL */
  {0x00000043u, 4}, /* TOOLS_FLUSH_EVENTS */
  {0x00000044u, 9248}, /* ALLOC_SEMAPHORE_POOL */
  {0x00000045u, 4}, /* CLEAN_UP_ZOMBIE_RESOURCES */
  {0x00000046u, 24}, /* PAGEABLE_MEM_ACCESS_ON_GPU */
  {0x00000048u, 24}, /* VALIDATE_VA_RANGE */
  {0x00000049u, 24}, /* CREATE_EXTERNAL_RANGE */
  {0x0000004au, 40}, /* MAP_EXTERNAL_SPARSE */
  {0x0000004bu, 8}, /* MM_INITIALIZE */
  {0x30000001u, 16}, /* INITIALIZE */
  {0x30000002u, 0}, /* DEINITIALIZE */
};

static const struct nvgpu_uvm_cmd nvgpu_uvm_v565_57_01[] = {
  {0x00000019u, 32}, /* REGISTER_GPU_VASPACE */
  {0x0000001au, 20}, /* UNREGISTER_GPU_VASPACE */
  {0x0000001bu, 56}, /* REGISTER_CHANNEL */
  {0x0000001cu, 28}, /* UNREGISTER_CHANNEL */
  {0x0000001du, 36}, /* ENABLE_PEER_ACCESS */
  {0x0000001eu, 36}, /* DISABLE_PEER_ACCESS */
  {0x00000021u, 9264}, /* MAP_EXTERNAL_ALLOCATION */
  {0x00000022u, 24}, /* FREE */
  {0x00000025u, 40}, /* REGISTER_GPU */
  {0x00000026u, 20}, /* UNREGISTER_GPU */
  {0x00000027u, 8}, /* PAGEABLE_MEM_ACCESS */
  {0x0000002au, 40}, /* SET_PREFERRED_LOCATION */
  {0x0000002bu, 24}, /* UNSET_PREFERRED_LOCATION */
  {0x0000002cu, 24}, /* ENABLE_READ_DUPLICATION */
  {0x0000002du, 24}, /* DISABLE_READ_DUPLICATION */
  {0x0000002eu, 40}, /* SET_ACCESSED_BY */
  {0x0000002fu, 40}, /* UNSET_ACCESSED_BY */
  {0x00000033u, 80}, /* MIGRATE */
  {0x00000041u, 40}, /* MAP_DYNAMIC_PARALLELISM_REGION */
  {0x00000042u, 40}, /* UNMAP_EXTERNAL */
  {0x00000043u, 4}, /* TOOLS_FLUSH_EVENTS */
  {0x00000044u, 9248}, /* ALLOC_SEMAPHORE_POOL */
  {0x00000045u, 4}, /* CLEAN_UP_ZOMBIE_RESOURCES */
  {0x00000046u, 24}, /* PAGEABLE_MEM_ACCESS_ON_GPU */
  {0x00000048u, 24}, /* VALIDATE_VA_RANGE */
  {0x00000049u, 24}, /* CREATE_EXTERNAL_RANGE */
  {0x0000004au, 40}, /* MAP_EXTERNAL_SPARSE */
  {0x0000004bu, 8}, /* MM_INITIALIZE */
  {0x0000004eu, 56}, /* ALLOC_DEVICE_P2P */
  {0x0000004fu, 4}, /* CLEAR_ALL_ACCESS_COUNTERS */
  {0x30000001u, 16}, /* INITIALIZE */
  {0x30000002u, 0}, /* DEINITIALIZE */
};

static const struct nvgpu_uvm_cmd nvgpu_uvm_v580_65_06[] = {
  {0x00000019u, 32}, /* REGISTER_GPU_VASPACE */
  {0x0000001au, 20}, /* UNREGISTER_GPU_VASPACE */
  {0x0000001bu, 56}, /* REGISTER_CHANNEL */
  {0x0000001cu, 28}, /* UNREGISTER_CHANNEL */
  {0x0000001du, 36}, /* ENABLE_PEER_ACCESS */
  {0x0000001eu, 36}, /* DISABLE_PEER_ACCESS */
  {0x00000021u, 9264}, /* MAP_EXTERNAL_ALLOCATION */
  {0x00000022u, 24}, /* FREE */
  {0x00000025u, 40}, /* REGISTER_GPU */
  {0x00000026u, 20}, /* UNREGISTER_GPU */
  {0x00000027u, 8}, /* PAGEABLE_MEM_ACCESS */
  {0x0000002au, 40}, /* SET_PREFERRED_LOCATION */
  {0x0000002bu, 24}, /* UNSET_PREFERRED_LOCATION */
  {0x0000002cu, 24}, /* ENABLE_READ_DUPLICATION */
  {0x0000002du, 24}, /* DISABLE_READ_DUPLICATION */
  {0x0000002eu, 40}, /* SET_ACCESSED_BY */
  {0x0000002fu, 40}, /* UNSET_ACCESSED_BY */
  {0x00000033u, 80}, /* MIGRATE */
  {0x00000041u, 40}, /* MAP_DYNAMIC_PARALLELISM_REGION */
  {0x00000042u, 40}, /* UNMAP_EXTERNAL */
  {0x00000043u, 4}, /* TOOLS_FLUSH_EVENTS */
  {0x00000044u, 9248}, /* ALLOC_SEMAPHORE_POOL */
  {0x00000045u, 4}, /* CLEAN_UP_ZOMBIE_RESOURCES */
  {0x00000046u, 24}, /* PAGEABLE_MEM_ACCESS_ON_GPU */
  {0x00000048u, 24}, /* VALIDATE_VA_RANGE */
  {0x00000049u, 24}, /* CREATE_EXTERNAL_RANGE */
  {0x0000004au, 40}, /* MAP_EXTERNAL_SPARSE */
  {0x0000004bu, 8}, /* MM_INITIALIZE */
  {0x0000004eu, 56}, /* ALLOC_DEVICE_P2P */
  {0x0000004fu, 4}, /* CLEAR_ALL_ACCESS_COUNTERS */
  {0x00000050u, 32}, /* DISCARD */
  {0x30000001u, 16}, /* INITIALIZE */
  {0x30000002u, 0}, /* DEINITIALIZE */
};

static const struct nvgpu_uvm_cmd nvgpu_uvm_v590_44_01[] = {
  {0x00000019u, 32}, /* REGISTER_GPU_VASPACE */
  {0x0000001au, 20}, /* UNREGISTER_GPU_VASPACE */
  {0x0000001bu, 56}, /* REGISTER_CHANNEL */
  {0x0000001cu, 12}, /* UNREGISTER_CHANNEL */
  {0x0000001du, 36}, /* ENABLE_PEER_ACCESS */
  {0x0000001eu, 36}, /* DISABLE_PEER_ACCESS */
  {0x00000021u, 9264}, /* MAP_EXTERNAL_ALLOCATION */
  {0x00000022u, 16}, /* FREE */
  {0x00000025u, 40}, /* REGISTER_GPU */
  {0x00000026u, 20}, /* UNREGISTER_GPU */
  {0x00000027u, 8}, /* PAGEABLE_MEM_ACCESS */
  {0x0000002au, 40}, /* SET_PREFERRED_LOCATION */
  {0x0000002bu, 24}, /* UNSET_PREFERRED_LOCATION */
  {0x0000002cu, 24}, /* ENABLE_READ_DUPLICATION */
  {0x0000002du, 24}, /* DISABLE_READ_DUPLICATION */
  {0x0000002eu, 40}, /* SET_ACCESSED_BY */
  {0x0000002fu, 40}, /* UNSET_ACCESSED_BY */
  {0x00000033u, 80}, /* MIGRATE */
  {0x00000041u, 40}, /* MAP_DYNAMIC_PARALLELISM_REGION */
  {0x00000042u, 40}, /* UNMAP_EXTERNAL */
  {0x00000043u, 4}, /* TOOLS_FLUSH_EVENTS */
  {0x00000044u, 9248}, /* ALLOC_SEMAPHORE_POOL */
  {0x00000045u, 4}, /* CLEAN_UP_ZOMBIE_RESOURCES */
  {0x00000046u, 24}, /* PAGEABLE_MEM_ACCESS_ON_GPU */
  {0x00000048u, 24}, /* VALIDATE_VA_RANGE */
  {0x00000049u, 24}, /* CREATE_EXTERNAL_RANGE */
  {0x0000004au, 40}, /* MAP_EXTERNAL_SPARSE */
  {0x0000004bu, 8}, /* MM_INITIALIZE */
  {0x0000004eu, 56}, /* ALLOC_DEVICE_P2P */
  {0x0000004fu, 4}, /* CLEAR_ALL_ACCESS_COUNTERS */
  {0x00000050u, 32}, /* DISCARD */
  {0x30000001u, 16}, /* INITIALIZE */
  {0x30000002u, 0}, /* DEINITIALIZE */
};

/* One per range of host releases; none before the first. */
static const struct nvgpu_uvm_table nvgpu_uvm_tables[] = {
  {.name = "v535_129_03", .vmin = NVGPU_SCHEMA_VERSION(535, 129, 3), .vmax = NVGPU_SCHEMA_VERSION(550, 40, 52),
   .cmds = nvgpu_uvm_v535_129_03, .ncmds = ARRAY_SIZE(nvgpu_uvm_v535_129_03)},
  {.name = "v550_40_53", .vmin = NVGPU_SCHEMA_VERSION(550, 40, 53), .vmax = NVGPU_SCHEMA_VERSION(565, 57, 0),
   .cmds = nvgpu_uvm_v550_40_53, .ncmds = ARRAY_SIZE(nvgpu_uvm_v550_40_53)},
  {.name = "v565_57_01", .vmin = NVGPU_SCHEMA_VERSION(565, 57, 1), .vmax = NVGPU_SCHEMA_VERSION(580, 65, 5),
   .cmds = nvgpu_uvm_v565_57_01, .ncmds = ARRAY_SIZE(nvgpu_uvm_v565_57_01)},
  {.name = "v580_65_06", .vmin = NVGPU_SCHEMA_VERSION(580, 65, 6), .vmax = NVGPU_SCHEMA_VERSION(590, 44, 0),
   .cmds = nvgpu_uvm_v580_65_06, .ncmds = ARRAY_SIZE(nvgpu_uvm_v580_65_06)},
  {.name = "v590_44_01", .vmin = NVGPU_SCHEMA_VERSION(590, 44, 1), .vmax = NVGPU_SCHEMA_VERSION(999, 999, 999),
   .cmds = nvgpu_uvm_v590_44_01, .ncmds = ARRAY_SIZE(nvgpu_uvm_v590_44_01)},
};

#endif /* NVGPU_SCHEMA_TABLES */

#endif /* NVGPU_SCHEMA_H */
