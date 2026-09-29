/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef DIFFTEST_LINUX_SLAB_H
#define DIFFTEST_LINUX_SLAB_H
#include <linux/types.h>
#define GFP_KERNEL 0u
/* kmalloc() hands out memory that is not zeroed but filled with 0xAA, so
 * that a path that sends bytes it never wrote shows up as a difference. */
void *harness_kmalloc(size_t n);
void *harness_kzalloc(size_t n);
void *harness_kvmalloc_array(size_t n, size_t size);
void harness_kfree(const void *p);
#define kmalloc(n, gfp) harness_kmalloc(n)
#define kzalloc(n, gfp) harness_kzalloc(n)
#define kvzalloc(n, gfp) harness_kzalloc(n)
#define kvmalloc(n, gfp) harness_kmalloc(n)
#define kvmalloc_array(n, s, gfp) harness_kvmalloc_array(n, s)
#define kfree(p) harness_kfree(p)
#define kvfree(p) harness_kfree(p)
#endif
