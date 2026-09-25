/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_MATH64_H
#define DIFFTEST_LINUX_MATH64_H
#include <linux/types.h>
static inline u64 div_u64(u64 a, u32 b) { return a / b; }
#endif
