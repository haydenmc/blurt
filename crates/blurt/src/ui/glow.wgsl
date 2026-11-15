// The room glow (see glow.rs): a mirrored equalizer standing on the bottom
// edge. Low bands at the sides, high bands meeting in the middle.

struct Glow {
    // Width and height in logical px; how far a full band rises, in logical
    // px; overall opacity.
    size_rise: vec4<f32>,
    a1: vec4<f32>,
    a2: vec4<f32>,
    // The sixteen bands, lowest first, 0 to 1: four to a vec4.
    bands: array<vec4<f32>, 4>,
};

@group(0) @binding(0) var<uniform> glow: Glow;

struct Varying {
    @builtin(position) position: vec4<f32>,
    // 0..1 across the widget, y down.
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the viewport iced set to our bounds.
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> Varying {
    let xy = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: Varying;
    out.position = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

const LAST: i32 = 15;

fn band(i: i32) -> f32 {
    let j = clamp(i, 0, LAST);
    return glow.bands[j / 4][j % 4];
}

// The bands as a smooth curve over 0 (lowest) to LAST (highest):
// Catmull-Rom through the band levels.
fn curve(x: f32) -> f32 {
    let i = i32(floor(x));
    let t = x - floor(x);
    let p0 = band(i - 1);
    let p1 = band(i);
    let p2 = band(i + 1);
    let p3 = band(i + 2);
    let t2 = t * t;
    let t3 = t2 * t;
    let v = 0.5 * (2.0 * p1 + (p2 - p0) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3);
    return clamp(v, 0.0, 1.0);
}

@fragment
fn fs_main(in: Varying) -> @location(0) vec4<f32> {
    let size = glow.size_rise.xy;
    let rise = glow.size_rise.z;
    let opacity = glow.size_rise.w;

    // Mirrored: 0 at either edge, 1 in the middle.
    let towards_middle = 1.0 - abs(in.uv.x * 2.0 - 1.0);
    let height = curve(towards_middle * f32(LAST)) * rise;
    if (height < 0.5) {
        discard;
    }
    // How far up this column we are, as a fraction of its height.
    let d = (size.y - in.uv.y * size.y) / height;
    // a2 at the bar, a1 at 45 %, transparent at the top.
    let colour = mix(glow.a2.rgb, glow.a1.rgb, smoothstep(0.0, 0.45, d));
    let alpha = (1.0 - smoothstep(0.45, 1.0, d)) * opacity;
    if (alpha <= 0.0) {
        discard;
    }
    // Premultiplied.
    return vec4<f32>(colour * alpha, alpha);
}
