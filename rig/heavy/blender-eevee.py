# SPDX-License-Identifier: Apache-2.0
# EEVEE, GPU-bound (rig/heavy/heavy-run.sh blender-*): a scene of 150
# subdivided monkeys, a dozen area and point lights, ray tracing and
# volumetrics, rendered at 1920x1080 and 64 samples from Blender's UI (its GL
# or Vulkan backend), frame after frame with the camera moving. The first
# frame compiles EEVEE's shaders and is reported apart; each later one is a
# line of $HEAVY_OUT/frames.txt (ms). Quits when done.
import bpy, gpu, math, os, time

N = int(os.environ.get("HEAVY_FRAMES", "8"))
OUT = os.environ.get("HEAVY_OUT", "/tmp")


def build():
    sc = bpy.context.scene
    sc.render.engine = "BLENDER_EEVEE"
    sc.render.resolution_x, sc.render.resolution_y = 1920, 1080
    sc.render.resolution_percentage = 100
    ee = sc.eevee
    ee.taa_render_samples = 64
    for attr, val in (("use_raytracing", True), ("use_shadows", True), ("volumetric_tile_size", "4")):
        try:
            setattr(ee, attr, val)
        except (AttributeError, TypeError):
            pass
    bpy.ops.mesh.primitive_plane_add(size=80)
    for i in range(150):
        x, y = (i % 15) * 2.5 - 17.5, (i // 15) * 2.5 - 11
        bpy.ops.mesh.primitive_monkey_add(location=(x, y, 1))
        ob = bpy.context.object
        m = ob.modifiers.new("s", "SUBSURF")
        m.levels = m.render_levels = 2
        mat = bpy.data.materials.new("m%d" % i)
        mat.use_nodes = True
        bsdf = mat.node_tree.nodes.get("Principled BSDF")
        if bsdf:
            bsdf.inputs["Base Color"].default_value = ((i * 37 % 100) / 100, (i * 61 % 100) / 100, 0.5, 1)
            bsdf.inputs["Roughness"].default_value = (i % 5) / 5
            bsdf.inputs["Metallic"].default_value = (i % 2) * 0.8
        ob.data.materials.append(mat)
    for i in range(12):
        kind = "AREA" if i % 2 else "POINT"
        bpy.ops.object.light_add(type=kind, location=(math.cos(i) * 12, math.sin(i) * 9, 5 + i % 3))
        bpy.context.object.data.energy = 800
    w = bpy.data.worlds.get("World") or bpy.data.worlds.new("World")
    sc.world = w
    w.use_nodes = True
    vol = w.node_tree.nodes.new("ShaderNodeVolumePrincipled")
    vol.inputs["Density"].default_value = 0.01
    w.node_tree.links.new(vol.outputs[0], w.node_tree.nodes["World Output"].inputs["Volume"])
    cam = sc.camera
    return sc, cam


def run():
    print("HEAVY_BLENDER backend=%s renderer=%s" % (gpu.platform.backend_type_get(), gpu.platform.renderer_get()), flush=True)
    sc, cam = build()
    sc.render.filepath = os.path.join(OUT, "blender-frame.png")
    times = []
    for f in range(N + 1):
        a = f * 0.15
        cam.location = (math.cos(a) * 30, math.sin(a) * 30, 14)
        cam.rotation_euler = (math.radians(65), 0, a + math.pi / 2)
        t = time.time()
        bpy.ops.render.render(write_still=(f == N))
        dt = (time.time() - t) * 1000
        if f == 0:
            print("HEAVY_BLENDER first frame (shaders) %.0f ms" % dt, flush=True)
        else:
            times.append(dt)
            print("HEAVY_BLENDER frame %d %.1f ms" % (f, dt), flush=True)
    with open(os.path.join(OUT, "frames.txt"), "w") as fh:
        fh.writelines("%.3f\n" % x for x in times)
    print("HEAVY_BLENDER done frames=%d mean_ms=%.1f" % (len(times), sum(times) / len(times)), flush=True)
    bpy.ops.wm.quit_blender()


def later():
    run()
    return None


bpy.app.timers.register(later, first_interval=3.0)
