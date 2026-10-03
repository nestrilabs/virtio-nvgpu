#version 450
// No vertex buffer: the triangle is built from gl_VertexIndex so the pipeline
// has no vertex input state to get wrong. Spans 1.2 x 1.2 in NDC, which covers
// 18% of the target -- offscreen-draw.c checks for that fraction.
void main() {
    vec2 p[3] = vec2[3](vec2(0.0, -0.6), vec2(-0.6, 0.6), vec2(0.6, 0.6));
    gl_Position = vec4(p[gl_VertexIndex], 0.0, 1.0);
}
