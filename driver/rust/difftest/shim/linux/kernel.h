/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_KERNEL_H
#define DIFFTEST_LINUX_KERNEL_H

#include <errno.h>
#include <stdio.h>
#include <string.h>

#include <linux/err.h>
#include <linux/minmax.h>
#include <linux/overflow.h>
#include <linux/types.h>

#define ARRAY_SIZE(a) (sizeof(a) / sizeof((a)[0]))
#define ALIGN(x, a) (((x) + ((__typeof__(x))(a) - 1)) & ~((__typeof__(x))(a) - 1))
#define DIV_ROUND_UP(n, d) (((n) + (d) - 1) / (d))
#define GENMASK(h, l) (((~0UL) << (l)) & (~0UL >> (63 - (h))))
#define U32_MAX ((u32)~0U)
#define U64_MAX ((u64)~0ULL)
#define pr_debug(...) do { } while (0)
#define likely(x) (x)
#define unlikely(x) (x)

/* Everything the parsers log goes to the harness, by format string. */
void harness_warn(const char *fmt);
#define dev_warn_ratelimited(dev, fmt, ...)                                     \
  do {                                                                         \
    (void)sizeof(dev);                                                         \
    harness_warn(fmt);                                                         \
  } while (0)

#endif
