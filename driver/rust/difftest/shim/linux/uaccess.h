/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef DIFFTEST_LINUX_UACCESS_H
#define DIFFTEST_LINUX_UACCESS_H
#include <errno.h>
#include <linux/types.h>
/* The caller's memory is the harness's; each returns the bytes not copied. */
unsigned long harness_copy_from_user(void *to, u64 from, unsigned long n);
unsigned long harness_copy_to_user(u64 to, const void *from, unsigned long n);
#define copy_from_user(to, from, n) harness_copy_from_user(to, (u64)(uintptr_t)(from), n)
#define copy_to_user(to, from, n) harness_copy_to_user((u64)(uintptr_t)(to), from, n)
#define get_user(x, ptr)                                                       \
  ({                                                                           \
    __typeof__(*(ptr)) __v;                                                    \
    unsigned long __r =                                                        \
        harness_copy_from_user(&__v, (u64)(uintptr_t)(ptr), sizeof(__v));      \
    (x) = __r ? 0 : __v;                                                       \
    __r ? -EFAULT : 0;                                                         \
  })
#define u64_to_user_ptr(x) ((void *)(uintptr_t)(x))
/*
 * The user half of the address space ends where x86-64's does (below
 * 2^47); the world's memory above it is kernel memory, which a driver-built
 * call (.kernel) or the DRM entry's copy of the argument (.karg) names, read
 * and written with plain copies (nvgpu_i2_kread / _kwrite) -- here, through
 * the harness. copy_from_user() / copy_to_user() refuse it, as the kernel's
 * do.
 */
#define DIFFTEST_USER_END 0x0000800000000000ull
#define access_ok(p, n)                                                        \
  ((u64)(uintptr_t)(p) < DIFFTEST_USER_END &&                                  \
   (u64)(n) <= DIFFTEST_USER_END - (u64)(uintptr_t)(p))
void harness_kread(void *dst, u64 src, unsigned long n);
void harness_kwrite(u64 dst, const void *src, unsigned long n);
#define nvgpu_i2_kread(dst, src, n) harness_kread(dst, (u64)(uintptr_t)(src), n)
#define nvgpu_i2_kwrite(dst, src, n)                                           \
  harness_kwrite((u64)(uintptr_t)(dst), src, n)
#endif
