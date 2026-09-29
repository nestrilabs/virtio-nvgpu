#version 450
// nvgpu-bench vk-cost: `cost` iterations of dependent transcendental math
// per pixel -- the fragment load that makes a frame GPU-bound on demand.
layout(push_constant) uniform P { uint cost; } pc;
layout(location = 0) out vec4 c;
void main() {
  float a = gl_FragCoord.x * 0.001, b = gl_FragCoord.y * 0.001;
  for (uint i = 0u; i < pc.cost; i++) {
    a = sin(a + b) * 1.0001;
    b = cos(b - a) * 0.9999;
  }
  c = vec4(a, b, 0.5, 1.0);
}
