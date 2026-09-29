#version 450
// nvgpu-bench vk-draws: a small triangle placed by a push constant.
layout(push_constant) uniform P { vec4 off; } pc;
void main() {
  vec2 p = vec2(gl_VertexIndex & 1, gl_VertexIndex >> 1);
  gl_Position = vec4(pc.off.xy + p * pc.off.z, 0.0, 1.0);
}
