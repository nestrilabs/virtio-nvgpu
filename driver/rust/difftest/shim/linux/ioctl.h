/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_IOCTL_H
#define DIFFTEST_LINUX_IOCTL_H
#define _IOC_WRITE 1U
#define _IOC_READ 2U
#define _IOC_DIR(nr) (((nr) >> 30) & 3U)
#define _IOC_TYPE(nr) (((nr) >> 8) & 0xffU)
#define _IOC_NR(nr) ((nr) & 0xffU)
#define _IOC_SIZE(nr) (((nr) >> 16) & 0x3fffU)
#endif
