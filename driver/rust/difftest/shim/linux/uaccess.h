/* SPDX-License-Identifier: GPL-2.0 */
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
/* Every address the harness hands out is the world's, a user's. */
#define access_ok(p, n) ((void)(p), (void)(n), 1)
#endif
