/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_OVERFLOW_H
#define DIFFTEST_LINUX_OVERFLOW_H
#define check_add_overflow(a, b, d) __builtin_add_overflow(a, b, d)
#define check_mul_overflow(a, b, d) __builtin_mul_overflow(a, b, d)
#endif
