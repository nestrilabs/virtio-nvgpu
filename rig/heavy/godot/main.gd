# SPDX-License-Identifier: Apache-2.0
# Two heavy scenes, picked by HEAVY_SCENE, built here so the project needs no
# imported assets. Both run unpaced (vsync off, no fps cap) for HEAVY_WARM
# seconds, then record every frame's time for HEAVY_SECS seconds into
# $HEAVY_OUT/frames.txt (ms, one a line), print a summary and quit.
#
#   gpu    GPU-bound: SDFGI, volumetric fog, SSAO, SSIL, SSR, glow, a
#          4-split shadowed sun and 48 shadowed omni lights moving over 900
#          meshes (spheres, tori, boxes) under an orbiting camera
#   draws  CPU-bound: 20,000 mesh instances of 64 meshes and 97 materials
#          (so the renderer cannot merge them into instanced draws), no
#          shadows or effects, the whole field turning every frame
extends Node3D

var scene := "gpu"
var warm := 6.0
var secs := 20.0
var t := 0.0
var last_us := 0
var times := PackedFloat32Array()
var lights: Array[OmniLight3D] = []
var cam: Camera3D
var field: Node3D
var rng := RandomNumberGenerator.new()

func env_f(name: String, dflt: float) -> float:
	var v := OS.get_environment(name)
	return v.to_float() if v != "" else dflt

func _ready():
	scene = OS.get_environment("HEAVY_SCENE") if OS.get_environment("HEAVY_SCENE") != "" else "gpu"
	warm = env_f("HEAVY_WARM", warm)
	secs = env_f("HEAVY_SECS", secs)
	rng.seed = 12345
	DisplayServer.window_set_vsync_mode(DisplayServer.VSYNC_DISABLED)
	Engine.max_fps = 0
	print("HEAVY_GODOT scene=", scene, " driver=", RenderingServer.get_current_rendering_driver_name(),
		" method=", RenderingServer.get_current_rendering_method(),
		" adapter=", RenderingServer.get_video_adapter_name(),
		" size=", DisplayServer.window_get_size())
	cam = Camera3D.new()
	add_child(cam)
	if scene == "draws":
		build_draws()
	else:
		build_gpu()

func sky_env() -> Environment:
	var env := Environment.new()
	var sky := Sky.new()
	sky.sky_material = ProceduralSkyMaterial.new()
	env.background_mode = Environment.BG_SKY
	env.sky = sky
	env.tonemap_mode = Environment.TONE_MAPPER_ACES
	return env

func build_gpu():
	var env := sky_env()
	env.sdfgi_enabled = true
	env.volumetric_fog_enabled = true
	env.volumetric_fog_density = 0.03
	env.ssao_enabled = true
	env.ssil_enabled = true
	env.ssr_enabled = true
	env.glow_enabled = true
	var we := WorldEnvironment.new()
	we.environment = env
	add_child(we)
	var sun := DirectionalLight3D.new()
	sun.shadow_enabled = true
	sun.directional_shadow_mode = DirectionalLight3D.SHADOW_PARALLEL_4_SPLITS
	sun.rotation_degrees = Vector3(-50, 30, 0)
	add_child(sun)
	var floor_ := MeshInstance3D.new()
	var pm := PlaneMesh.new()
	pm.size = Vector2(160, 160)
	floor_.mesh = pm
	add_child(floor_)
	var meshes: Array[Mesh] = []
	var s := SphereMesh.new()
	s.radial_segments = 64
	s.rings = 32
	meshes.append(s)
	var tor := TorusMesh.new()
	tor.rings = 48
	tor.ring_segments = 32
	meshes.append(tor)
	meshes.append(BoxMesh.new())
	var mats: Array[StandardMaterial3D] = []
	for i in range(8):
		var m := StandardMaterial3D.new()
		m.albedo_color = Color.from_hsv(i / 8.0, 0.7, 0.9)
		m.metallic = (i % 4) / 3.0
		m.roughness = 0.15 + (i % 3) * 0.3
		mats.append(m)
	for i in range(900):
		var mi := MeshInstance3D.new()
		mi.mesh = meshes[i % meshes.size()]
		mi.material_override = mats[i % mats.size()]
		mi.position = Vector3((i % 30) * 2.2 - 32.0, 0.6 + rng.randf() * 2.0, (i / 30) * 2.2 - 32.0)
		mi.rotation = Vector3(rng.randf() * TAU, rng.randf() * TAU, 0)
		add_child(mi)
	for i in range(48):
		var l := OmniLight3D.new()
		l.light_color = Color.from_hsv(rng.randf(), 0.8, 1.0)
		l.light_energy = 4.0
		l.omni_range = 7.0
		l.shadow_enabled = true
		add_child(l)
		lights.append(l)

func build_draws():
	var env := sky_env()
	var we := WorldEnvironment.new()
	we.environment = env
	add_child(we)
	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-50, 30, 0)
	add_child(sun)
	var meshes: Array[Mesh] = []
	for i in range(64):
		var b := BoxMesh.new()
		b.size = Vector3(0.3 + (i % 4) * 0.05, 0.3 + (i / 4 % 4) * 0.05, 0.3 + (i / 16) * 0.05)
		meshes.append(b)
	var mats: Array[StandardMaterial3D] = []
	for i in range(97):
		var m := StandardMaterial3D.new()
		m.albedo_color = Color.from_hsv(i / 97.0, 0.6, 0.9)
		m.roughness = 0.2 + (i % 5) * 0.15
		mats.append(m)
	field = Node3D.new()
	add_child(field)
	for i in range(20000):
		var mi := MeshInstance3D.new()
		mi.mesh = meshes[(i * 7) % meshes.size()]
		mi.material_override = mats[(i * 13) % mats.size()]
		mi.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
		mi.position = Vector3((i % 200) * 0.5 - 50.0, (i / 200) * 0.5 - 25.0, 0)
		field.add_child(mi)
	field.position = Vector3(0, 0, -60)

func _process(delta):
	t += delta
	if scene == "draws":
		field.rotation.y = sin(t * 0.5) * 0.3
		field.rotation.x = cos(t * 0.3) * 0.2
		cam.position = Vector3(0, 0, 0)
	else:
		cam.position = Vector3(cos(t * 0.2) * 30.0, 9.0, sin(t * 0.2) * 30.0)
		cam.look_at(Vector3(0, 1, 0))
		for i in range(lights.size()):
			var a := t * (0.3 + (i % 7) * 0.05) + i
			lights[i].position = Vector3(cos(a) * (6.0 + i * 0.5), 2.5 + sin(a * 1.3), sin(a) * (6.0 + i * 0.5))
	# The wall clock between frames, not `delta`: Godot smooths delta
	# (application/run/delta_smoothing), snapping it to the refresh period.
	var now := Time.get_ticks_usec()
	if t > warm and last_us > 0:
		times.append((now - last_us) / 1000.0)
	last_us = now
	if t > warm + secs:
		finish()

func finish():
	var out := OS.get_environment("HEAVY_OUT")
	if out != "":
		var f := FileAccess.open(out + "/frames.txt", FileAccess.WRITE)
		for x in times:
			f.store_line("%.4f" % x)
		f.close()
	var s := 0.0
	for x in times:
		s += x
	print("HEAVY_GODOT done frames=", times.size(), " mean_ms=", s / max(times.size(), 1), " fps=", times.size() / (s / 1000.0),
		" size=", DisplayServer.window_get_size())
	get_tree().quit()
