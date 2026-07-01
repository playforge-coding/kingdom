// Instanced textured-quad shader used for both terrain and entities.

struct Camera {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct VertexInput {
    // Unit-quad corner in [0,1] on both axes.
    @location(0) corner: vec2<f32>,
};

struct InstanceInput {
    @location(1) pos: vec2<f32>,      // world-space top-left
    @location(2) size: vec2<f32>,     // world-space size
    @location(3) uv_min: vec2<f32>,
    @location(4) uv_max: vec2<f32>,
    @location(5) color: vec4<f32>,    // tint (multiplied)
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(vert: VertexInput, inst: InstanceInput) -> VertexOutput {
    var out: VertexOutput;
    let world = inst.pos + vert.corner * inst.size;
    out.clip_position = camera.view_proj * vec4<f32>(world, 0.0, 1.0);
    out.uv = inst.uv_min + vert.corner * (inst.uv_max - inst.uv_min);
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let tex = textureSample(atlas_tex, atlas_sampler, in.uv);
    let col = tex * in.color;
    if (col.a < 0.01) {
        discard;
    }
    return col;
}
