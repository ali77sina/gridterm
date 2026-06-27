struct VertexInput {
    @location(0) position: vec2<f32>,
};

struct InstanceInput {
    @location(1) rect: vec4<f32>,   // x, y, w, h in pixels
    @location(2) color: vec4<f32>,  // rgba 0..1
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

struct Screen {
    size: vec2<f32>,
};

@group(0) @binding(0)
var<uniform> screen: Screen;

@vertex
fn vs_main(v: VertexInput, inst: InstanceInput) -> VertexOutput {
    // v.position is a unit quad corner in [0,1].
    let px = inst.rect.xy + v.position * inst.rect.zw;
    // Convert pixel coords (origin top-left) to clip space.
    let ndc_x = (px.x / screen.size.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (px.y / screen.size.y) * 2.0;
    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
