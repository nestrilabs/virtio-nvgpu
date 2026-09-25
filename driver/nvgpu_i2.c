// SPDX-License-Identifier: GPL-2.0
/*
 * The schema-driven IOCTL2 interpreter: gathers a caller's buffers per the
 * generated schema (gen/nvgpu_schema.h), sends them, and scatters the reply.
 */
#include "nvgpu.h"
