/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_TIME_H
#define DIFFTEST_LINUX_TIME_H
#include <time.h>
#ifndef CLOCK_REALTIME
#define CLOCK_REALTIME 0
#endif
#ifndef CLOCK_MONOTONIC_RAW
#define CLOCK_MONOTONIC_RAW 4
#endif
#define NSEC_PER_USEC 1000L
#endif
