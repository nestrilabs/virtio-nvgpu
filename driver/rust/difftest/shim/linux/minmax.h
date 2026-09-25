/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_MINMAX_H
#define DIFFTEST_LINUX_MINMAX_H
#define min(a, b) ({ __typeof__(a) _a = (a); __typeof__(b) _b = (b); _a < _b ? _a : _b; })
#define max(a, b) ({ __typeof__(a) _a = (a); __typeof__(b) _b = (b); _a > _b ? _a : _b; })
#define min_t(t, a, b) ({ t _a = (a); t _b = (b); _a < _b ? _a : _b; })
#define max_t(t, a, b) ({ t _a = (a); t _b = (b); _a > _b ? _a : _b; })
#define max3(a, b, c) max(max(a, b), c)
#endif
