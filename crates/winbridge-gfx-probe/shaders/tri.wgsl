// SPDX-License-Identifier: GPL-3.0-or-later
// Triangle for the WinBridge render probe. Compiled to SPIR-V with naga (see shaders/README in the probe's doc comment).
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2<f32>(-0.6, 0.6), vec2<f32>(0.6, 0.6), vec2<f32>(0.0, -0.6));
    return vec4<f32>(p[i], 0.0, 1.0);
}

@fragment
fn fs() -> @location(0) vec4<f32> {
    return vec4<f32>(0.9, 0.4, 0.1, 1.0);
}
