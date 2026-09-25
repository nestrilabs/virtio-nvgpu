/* SPDX-License-Identifier: GPL-2.0 */
#ifndef DIFFTEST_LINUX_MM_H
#define DIFFTEST_LINUX_MM_H
#include <linux/slab.h>
#include <linux/types.h>
#define PAGE_SIZE 4096UL
#define PAGE_MASK (~(PAGE_SIZE - 1))
/* A pinned page, as the harness hands them out: its address is its
 * guest-physical address. */
struct page;
#define page_to_phys(p) ((u64)(uintptr_t)(p))
#endif
