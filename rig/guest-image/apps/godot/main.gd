# SPDX-License-Identifier: Apache-2.0
extends Node3D

var t := 0.0
var frames := 0

func _ready():
	print("NVGPU_GODOT driver=", RenderingServer.get_current_rendering_driver_name(),
		" method=", RenderingServer.get_current_rendering_method(),
		" adapter=", RenderingServer.get_video_adapter_name(),
		" vendor=", RenderingServer.get_video_adapter_vendor(),
		" api=", RenderingServer.get_video_adapter_api_version(),
		" display=", DisplayServer.get_name())
	for i in range(400):
		var m := MeshInstance3D.new()
		m.mesh = $Cube.mesh
		m.material_override = $Cube.material_override
		m.position = Vector3((i % 20) - 9.5, -1.5, -(i / 20) * 1.2)
		m.scale = Vector3(0.4, 0.4, 0.4)
		add_child(m)

func _process(delta):
	t += delta
	frames += 1
	$Cube.rotate_y(delta)
	$Cube.rotate_x(delta * 0.5)
	if frames % 300 == 0:
		print("NVGPU_GODOT fps=", Engine.get_frames_per_second(), " frames=", frames, " t=", snapped(t, 0.1))
