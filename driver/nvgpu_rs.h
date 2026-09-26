/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * The narrow ABI between the module's C and its Rust parsers (NVGPU_RUST=1).
 *
 * nvgpu_rs.rs exports the functions nvgpu.h declares for the parsers
 * (nvgpu_ioctl_fd(), nvgpu_uvm_ioctl_fd(), nvgpu_ioctl_modeset()) and the
 * nvgpu_rs_* ones below; nvgpu_rs_glue.c implements nvgpu.h's IOCTL2 API on
 * top of them, and the nvgpu_rs_* services Rust calls back: copies from and
 * to the caller, allocation, the transport, the descriptor table, the hooks,
 * pinning. Rust never sees a C struct but the ones defined here, whose layout
 * both sides assert, and the generated schema tables, whose layout
 * driver/rust/core/src/guest/schema.rs mirrors and asserts too.
 *
 * Nothing here hands Rust the caller's memory: every byte of it reaches Rust
 * through nvgpu_rs_copy_from(), into a buffer Rust owns, once.
 */

#ifndef NVGPU_RS_H
#define NVGPU_RS_H

#include <linux/build_bug.h>
#include <linux/stddef.h>
#include <linux/types.h>

#include "gen/nvgpu_schema.h"

struct nvgpu_fd;
struct nvgpu_i2_call;
struct nvgpu_device;

/* One of a schema set's tables, as slices. */
struct nvgpu_rs_table {
  const struct nvgpu_sioctl *ioctls;
  u64 nioctls;
  const struct nvgpu_sfield *fields;
  u64 nfields;
  const u8 *planes;
  u64 nplanes;
};

/* struct nvgpu_schema_set, for Rust. */
struct nvgpu_rs_tables {
  struct nvgpu_rs_table drm;
  struct nvgpu_rs_table modeset;
  u32 has_modeset;
  u32 reserved;
};

/* An IOCTL2 call, from struct nvgpu_i2_call and its device. */
struct nvgpu_rs_i2_args {
  u64 uarg;
  u64 max_req;
  u64 max_resp;
  u32 sclass;
  u32 cmd;
  u32 handle;
  u32 render;
  u32 xflags;
  u8 compat; /* in_compat_syscall() */
  u8 kernel; /* nvgpu_i2_call.kernel */
  u16 reserved;
};

/* struct nvgpu_rm_deep_control, widened. */
struct nvgpu_rs_deep_ptr {
  u32 ptr;
  u32 flags;
  u32 ncounts;
  u32 count_off[2];
  u32 count_width[2];
  u32 scale;
  u32 elem;
};

struct nvgpu_rs_deep_control {
  u32 cmd;
  u32 nptrs;
  struct nvgpu_rs_deep_ptr ptrs[4];
};

static_assert(sizeof(struct nvgpu_rs_table) == 48, "nvgpu_rs_table");
static_assert(sizeof(struct nvgpu_rs_tables) == 104, "nvgpu_rs_tables");
static_assert(sizeof(struct nvgpu_rs_i2_args) == 48, "nvgpu_rs_i2_args");
static_assert(sizeof(struct nvgpu_rs_deep_ptr) == 36, "nvgpu_rs_deep_ptr");
static_assert(sizeof(struct nvgpu_rs_deep_control) == 152,
              "nvgpu_rs_deep_control");

/* The schema tables' layout, which Rust reads in place (schema.rs). */
static_assert(sizeof(struct nvgpu_sfield) == 64, "nvgpu_sfield");
static_assert(offsetof(struct nvgpu_sfield, kind) == 4, "sfield.kind");
static_assert(offsetof(struct nvgpu_sfield, cond_off) == 8, "sfield.cond_off");
static_assert(offsetof(struct nvgpu_sfield, len_kind) == 20, "sfield.len_kind");
static_assert(offsetof(struct nvgpu_sfield, len_a) == 24, "sfield.len_a");
static_assert(offsetof(struct nvgpu_sfield, kinds) == 44, "sfield.kinds");
static_assert(offsetof(struct nvgpu_sfield, none_value) == 48, "sfield.none");
static_assert(offsetof(struct nvgpu_sfield, stride) == 52, "sfield.stride");
static_assert(offsetof(struct nvgpu_sfield, child) == 60, "sfield.child");
static_assert(sizeof(struct nvgpu_sioctl) == 32, "nvgpu_sioctl");
static_assert(offsetof(struct nvgpu_sioctl, cmd) == 8, "sioctl.cmd");
static_assert(offsetof(struct nvgpu_sioctl, sclass) == 20, "sioctl.sclass");
static_assert(offsetof(struct nvgpu_sioctl, flags) == 22, "sioctl.flags");
static_assert(offsetof(struct nvgpu_sioctl, policy) == 24, "sioctl.policy");
static_assert(offsetof(struct nvgpu_sioctl, field) == 28, "sioctl.field");

/* nvgpu_rs_caps() bits. */
#define NVGPU_RS_CAP_DEEP_SEGS (1u << 0)
#define NVGPU_RS_CAP_PROC_IDS (1u << 1)
#define NVGPU_RS_CAP_PROC_EUID (1u << 2)
#define NVGPU_RS_CAP_OS_DESC (1u << 3)

/* nvgpu_rs_warn() / nvgpu_rs_i2_warn() codes, and what a and b are. */
#define NVGPU_RS_WARN_CONTROL_FD 1       /* control, descriptor */
#define NVGPU_RS_WARN_CONTROL_OS_EVENT 2 /* control, value */
#define NVGPU_RS_WARN_ALLOC_EVENT_FD 3   /* class, descriptor */
#define NVGPU_RS_WARN_EVENT_BUFFER 4     /* -, value */
#define NVGPU_RS_WARN_SURFACE_FD 5       /* -, descriptor */
#define NVGPU_RS_WARN_I2_CMD 16          /* caller's cmd, entry's cmd, size */
#define NVGPU_RS_WARN_I2_MALFORMED 17    /* used, fds, GEM handles */
#define NVGPU_RS_WARN_I2_UNNAMED 18

/* nvgpu_rs_osdesc_pin(): pinned, no memory for the array, would not pin. */
#define NVGPU_RS_PIN_OK 0
#define NVGPU_RS_PIN_NOMEM 1
#define NVGPU_RS_PIN_FAILED 2

/* ── Exported by nvgpu_rs.rs ── */

/* Run an IOCTL2 (nvgpu_i2_ioctl() after its v2 check); *ret_out is what
 * call->ret is to be after it. */
long nvgpu_rs_i2_ioctl(struct nvgpu_i2_call *call,
                       const struct nvgpu_rs_i2_args *a,
                       const struct nvgpu_rs_tables *t, s32 *ret_out);
bool nvgpu_rs_i2_has_schema(const struct nvgpu_rs_tables *t, u32 sclass,
                            u32 cmd, const void *prefix, size_t prefix_len);
/* The state is call->st while a hook runs; NULL is answered as the C does. */
int nvgpu_rs_i2_add_dyn(void *st, u32 kind, u32 buf, u32 off, u32 len);
int nvgpu_rs_i2_add_fd(void *st, u32 buf, u32 off, u32 handle, u32 flags);
void *nvgpu_rs_i2_buf(void *st, u32 buf, u32 *len);
/* Where the state keeps what nvgpu_i2_hold() was given (C's to manage). */
void **nvgpu_rs_i2_held(void *st);
/* nvgpu_atomic_parse() on the state a running ATOMIC special was handed. */
struct nvgpu_atomic_ops;
struct nvgpu_atomic_out;
int nvgpu_rs_atomic_parse(void *st, bool fences,
                          const struct nvgpu_atomic_ops *ops, void *ctx,
                          struct nvgpu_atomic_out *out);

/* ── Implemented by nvgpu_rs_glue.c, for nvgpu_rs.rs ── */

/* The caller's memory (or, for a call the driver built, kernel memory). */
int nvgpu_rs_copy_from(bool kernel, void *dst, u64 src, size_t len);
int nvgpu_rs_copy_to(bool kernel, u64 dst, const void *src, size_t len);
void *nvgpu_rs_kvzalloc(size_t len);
void nvgpu_rs_kvfree(void *p);

/* The protocol-v1 paths, on the calling file. */
u32 nvgpu_rs_caps(struct nvgpu_fd *nfd);
u32 nvgpu_rs_fd_handle(struct nvgpu_fd *nfd);
/* nvgpu_handle_for_fd() on the calling file's device. */
int nvgpu_rs_handle_for_fd(struct nvgpu_fd *nfd, int fd, u32 *handle);
int nvgpu_rs_send_recv(struct nvgpu_fd *nfd, const void *req, size_t req_len,
                       void *resp, size_t resp_len, u32 *used);
void nvgpu_rs_proc_id(struct nvgpu_fd *nfd, void *dst);
const char *nvgpu_rs_driver_version(struct nvgpu_fd *nfd, size_t *len);
bool nvgpu_rs_clock_to_guest(struct nvgpu_fd *nfd, u32 raw, s64 host_ns,
                             s64 *guest_ns);
bool nvgpu_rs_rm_deep(u32 cmd, bool idle_channels,
                      struct nvgpu_rs_deep_control *out);
bool nvgpu_rs_v1v2(u32 cmd, u32 *userptr_offset, bool *info_style);
u32 nvgpu_rs_rmalloc_size(u32 hclass);
bool nvgpu_rs_fd_translation(struct nvgpu_fd *nfd, u32 key, u32 *payload);
int nvgpu_rs_uvm_size(struct nvgpu_fd *nfd, u32 cmd);
void nvgpu_rs_warn(struct nvgpu_fd *nfd, u32 code, u64 a, u64 b);
void nvgpu_rs_osdesc_reap(struct nvgpu_fd *nfd);
int nvgpu_rs_osdesc_pin(u64 start, u64 npages, bool write, void **pages);
u64 nvgpu_rs_page_phys(void *pages, u64 i);
void nvgpu_rs_osdesc_keep(struct nvgpu_fd *nfd, u64 id, void *pages,
                          u64 npages, bool write);
void nvgpu_rs_osdesc_unpin(void *pages, u64 npages, bool write);
/* nvgpu_osdesc_send(): on -EINTR/-ETIMEDOUT `pages` are the transport's. */
int nvgpu_rs_osdesc_send(struct nvgpu_fd *nfd, const void *req,
                         size_t req_len, void *resp, size_t resp_len, u32 *used,
                         void *pages, u64 npages, bool write);

/* IOCTL2: the hooks, with call->st and call->ret as C sees them. */
int nvgpu_rs_i2_fd_in(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                      u32 off, s64 value, u32 kinds, u32 *handle, u32 *flags);
int nvgpu_rs_i2_gem_in(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                       u32 off, u32 guest, u32 *owner, u32 *gem);
int nvgpu_rs_i2_fd_out(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                       u32 off, u32 handle, u32 kind, s64 *value);
int nvgpu_rs_i2_gem_out(struct nvgpu_i2_call *call, void *st, s32 *ret,
                        u32 buf, u32 off, u32 gem, u64 size, u32 *guest);
int nvgpu_rs_i2_special(struct nvgpu_i2_call *call, void *st, s32 *ret,
                        u32 id, int phase);
int nvgpu_rs_i2_phase(struct nvgpu_i2_call *call, void *st, s32 *ret,
                      int phase);
void nvgpu_rs_i2_close(struct nvgpu_i2_call *call, u32 handle);
void nvgpu_rs_i2_gem_close(struct nvgpu_i2_call *call, u32 gem);
void nvgpu_rs_i2_warn(struct nvgpu_i2_call *call, u32 code, const char *name,
                      u32 a, u32 b, u32 c);
/* The transport. */
void *nvgpu_rs_tbuf_alloc(size_t len);
void nvgpu_rs_tbuf_free(void *tb);
int nvgpu_rs_tbuf_write(void *tb, size_t off, const void *src, size_t len);
int nvgpu_rs_tbuf_read(const void *tb, size_t off, void *dst, size_t len);
/* What the hooks asked to hold goes with `req` from now on. */
void nvgpu_rs_i2_hand_over(void *req, void **held);
void nvgpu_rs_held_release(void *held);
int nvgpu_rs_i2_xfer(struct nvgpu_i2_call *call, void *req, void *resp,
                     u32 flags, u32 *used);

#endif /* NVGPU_RS_H */
