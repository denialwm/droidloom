#version 450
// Vertices live in the shader so the first forwarded frames need no buffers.
layout(push_constant) uniform Frame { float angle; float aspect; vec2 offset; float scale; } frame;
layout(location = 0) out vec3 color;
const vec2 positions[3] = vec2[](vec2(0.0, -0.62), vec2(0.54, 0.31), vec2(-0.54, 0.31));
const vec3 colors[3] = vec3[](vec3(1.0, 0.25, 0.35), vec3(0.25, 1.0, 0.45), vec3(0.3, 0.45, 1.0));
void main() {
    float c = cos(frame.angle), s = sin(frame.angle);
    vec2 v = positions[gl_VertexIndex];
    vec2 r = vec2(c * v.x - s * v.y, s * v.x + c * v.y) * frame.scale;
    gl_Position = vec4(r.x / frame.aspect + frame.offset.x, r.y + frame.offset.y, 0.0, 1.0);
    color = colors[gl_VertexIndex];
}
