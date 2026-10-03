#version 450
// Red, against the blue clear. Two distinct channels so a half-working copy
// cannot be mistaken for a pass.
layout(location = 0) out vec4 colour;
void main() { colour = vec4(1.0, 0.0, 0.0, 1.0); }
