# SPDX-License-Identifier: Apache-2.0
# Cycles in the background (apps.sh cycles): one frame of the default scene on
# the device named after "--" (CUDA, OPTIX or CPU), with the devices Cycles
# found and the time it took.
import bpy, sys, time

dev = sys.argv[sys.argv.index("--") + 1] if "--" in sys.argv else "CUDA"
sc = bpy.context.scene
sc.render.engine = "CYCLES"
if dev != "CPU":
    prefs = bpy.context.preferences.addons["cycles"].preferences
    prefs.compute_device_type = dev
    prefs.get_devices()
    print("NVGPU_CYCLES devices", [(d.name, d.type) for d in prefs.devices], flush=True)
    n = 0
    for d in prefs.devices:
        d.use = d.type == dev
        n += d.use
    if not n:
        print("NVGPU_CYCLES no %s device" % dev, flush=True)
        sys.exit(3)
    sc.cycles.device = "GPU"
sc.cycles.samples = 128
sc.render.resolution_x, sc.render.resolution_y = 1280, 720
sc.render.filepath = "/tmp/apps/cycles-%s.png" % dev.lower()
t = time.time()
bpy.ops.render.render(write_still=True)
print("NVGPU_CYCLES %s frame rendered in %.2fs to %s" % (dev, time.time() - t, sc.render.filepath), flush=True)
