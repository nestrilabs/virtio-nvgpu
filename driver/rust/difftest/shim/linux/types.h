/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Userspace stand-ins for the kernel headers the module's parsers include,
 * so that the C (nvgpu_i2.c, nvgpu_rmio.c, nvgpu_schema.c) compiles as it
 * is into the differential test. Only what those files use.
 */
#ifndef DIFFTEST_LINUX_TYPES_H
#define DIFFTEST_LINUX_TYPES_H

#include <limits.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
typedef int8_t s8;
typedef int16_t s16;
typedef int32_t s32;
typedef int64_t s64;
typedef u8 __u8;
typedef u16 __u16;
typedef u32 __u32;
typedef u64 __u64;
typedef s32 __s32;
typedef s64 __s64;
typedef u16 __le16;
typedef u32 __le32;
typedef u64 __le64;
typedef unsigned int gfp_t;

#define __packed __attribute__((packed))
#define __user
#define __force
#define __iomem

#define cpu_to_le16(x) ((__le16)(x))
#define cpu_to_le32(x) ((__le32)(x))
#define cpu_to_le64(x) ((__le64)(x))
#define le16_to_cpu(x) ((u16)(x))
#define le32_to_cpu(x) ((u32)(x))
#define le64_to_cpu(x) ((u64)(x))

#endif
