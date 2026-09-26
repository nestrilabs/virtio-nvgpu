// SPDX-License-Identifier: GPL-2.0-only
/*
 * The generated IOCTL2 schema and UVM tables (gen/nvgpu_schema.h), and which
 * of them a host gets. The tables are the module's one copy: the C
 * interpreter (nvgpu_i2.c) and the Rust one (nvgpu_rs.rs) both read them
 * from here.
 */

#include <linux/kernel.h>

#include "nvgpu.h"

#define NVGPU_SCHEMA_TABLES
#include "gen/nvgpu_schema.h"

/* The tables to use before HELLO has picked the host's (DRM only). */
const struct nvgpu_schema_set *nvgpu_schema_default(void) {
  return &nvgpu_schema_sets[0];
}

/*
 * A host release as NVGPU_SCHEMA_VERSION, or 0 if it does not parse. NVIDIA
 * numbers some releases in two parts (550.67, 595.80): those are .0, as the
 * backend reads them too (abi::version::DriverVersion::parse), so both halves
 * pick the same tables.
 */
static u32 nvgpu_host_version(const char *driver_version) {
  unsigned int a, b, c = 0;

  if (!driver_version || sscanf(driver_version, "%u.%u.%u", &a, &b, &c) < 2)
    return 0;
  return NVGPU_SCHEMA_VERSION(a, b, c);
}

const struct nvgpu_schema_set *nvgpu_schema_select(const char *driver_version) {
  u32 v = nvgpu_host_version(driver_version), i;

  /* The DRM tables are the same for every host; only NVKMS's layouts move
   * between releases (REGISTER_SURFACE is command 16 in one and 17 in the
   * next, R:nvdirect §0.6a), so an unparsable version gets no NVKMS table,
   * never a guessed one. */
  if (!v)
    return &nvgpu_schema_sets[0];
  for (i = 1; i < ARRAY_SIZE(nvgpu_schema_sets); i++) {
    const struct nvgpu_stable *t = nvgpu_schema_sets[i].modeset;

    if (t->vmin <= v && v <= t->vmax)
      return &nvgpu_schema_sets[i];
  }
  return &nvgpu_schema_sets[0];
}

const struct nvgpu_uvm_table *nvgpu_uvm_select(const char *driver_version) {
  u32 v = nvgpu_host_version(driver_version), i;

  for (i = 0; v && i < ARRAY_SIZE(nvgpu_uvm_tables); i++)
    if (nvgpu_uvm_tables[i].vmin <= v && v <= nvgpu_uvm_tables[i].vmax)
      return &nvgpu_uvm_tables[i];
  return NULL;
}
