# SPDX-License-Identifier: Apache-2.0
# Blender with its UI (apps.sh blender, blendervk): the GPU module's account
# of the backend and renderer, then one EEVEE frame rendered from the UI.
import bpy, gpu, time

def report():
    print("NVGPU_BLENDER backend=%s vendor=%s renderer=%s version=%s" % (
        gpu.platform.backend_type_get(), gpu.platform.vendor_get(),
        gpu.platform.renderer_get(), gpu.platform.version_get()), flush=True)
    sc = bpy.context.scene
    for eng in ("BLENDER_EEVEE", "BLENDER_EEVEE_NEXT"):
        try:
            sc.render.engine = eng
            break
        except TypeError:
            pass
    sc.render.resolution_x, sc.render.resolution_y = 960, 540
    sc.render.filepath = "/tmp/apps/blender-eevee-%s.png" % gpu.platform.backend_type_get().lower()
    t = time.time()
    bpy.ops.render.render(write_still=True)
    print("NVGPU_BLENDER eevee frame rendered in %.2fs to %s" % (time.time() - t, sc.render.filepath), flush=True)
    return None

bpy.app.timers.register(report, first_interval=4.0)
