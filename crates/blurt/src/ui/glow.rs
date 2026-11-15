//! The room glow: soft light rising from the control bar while people in
//! your channel talk. The one decorative effect, and only with rich visuals
//! (the GPU renderer): it's a small fragment shader, which tiny-skia can't
//! run.
//!
//! It's an equalizer, mirrored: the room's sound in frequency bands, low
//! at the window's edges and high meeting in the middle, as one smooth
//! waveform standing on the bar's top edge. Each column fades from a2 at
//! the bar through a1 to transparent. How high it can rise and how opaque
//! it gets follow how many are talking, capped so text above stays
//! readable. Blended additively on dark, normally on light.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer as advanced_renderer;
use iced::advanced::widget::{Tree, Widget, tree};
use iced::wgpu;
use iced::widget::shader::{self, Viewport};
use iced::{Color, Element, Length, Rectangle, Size, mouse};

use blurt_audio::level::BANDS;

use crate::Message;
use crate::rich;

/// What the glow shows this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Per frequency band, lowest first: 0 (flat) to 1 (full height).
    pub bands: [f32; BANDS],
    /// How far a band at full height rises, in logical px.
    pub rise: f32,
    /// The whole glow's opacity, already capped.
    pub opacity: f32,
    pub a1: Color,
    pub a2: Color,
    /// Add light (dark mode) rather than paint over (light mode).
    pub additive: bool,
}

impl Params {
    /// Whether there's anything to draw.
    pub fn visible(&self) -> bool {
        self.opacity > 0.0 && self.bands.iter().any(|&b| b > 0.001)
    }
}

/// The glow, `height` px tall, standing on the bottom of its space. Draws
/// nothing, and takes no input, on the CPU renderer.
pub fn glow<'a>(params: Params, height: f32) -> Element<'a, Message> {
    Element::new(Glow {
        shader: iced::widget::Shader::new(Program { params })
            .width(Length::Fill)
            .height(Length::Fixed(height)),
    })
}

struct Glow {
    shader: iced::widget::Shader<Message, Program>,
}

impl Widget<Message, iced::Theme, iced::Renderer> for Glow {
    fn size(&self) -> Size<Length> {
        <iced::widget::Shader<Message, Program> as Widget<Message, iced::Theme, iced::Renderer>>::size(
            &self.shader,
        )
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        Widget::<Message, iced::Theme, iced::Renderer>::layout(
            &mut self.shader,
            tree,
            renderer,
            limits,
        )
    }

    fn tag(&self) -> tree::Tag {
        Widget::<Message, iced::Theme, iced::Renderer>::tag(&self.shader)
    }

    fn state(&self) -> tree::State {
        Widget::<Message, iced::Theme, iced::Renderer>::state(&self.shader)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &advanced_renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // The GPU can fail to start, leaving iced on the CPU renderer, which
        // can't run shaders: draw only on the GPU, and remember which it is.
        match renderer {
            iced::Renderer::Primary(_) => {
                rich::set_renderer(rich::Renderer::Gpu);
                Widget::<Message, iced::Theme, iced::Renderer>::draw(
                    &self.shader,
                    tree,
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor,
                    viewport,
                );
            }
            iced::Renderer::Secondary(_) => rich::set_renderer(rich::Renderer::Cpu),
        }
    }
}

struct Program {
    params: Params,
}

impl shader::Program<Message> for Program {
    type State = ();
    type Primitive = Primitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, bounds: Rectangle) -> Primitive {
        Primitive {
            params: self.params,
            size: bounds.size(),
        }
    }
}

#[derive(Debug)]
struct Primitive {
    params: Params,
    size: Size,
}

/// Floats in the shader's uniforms: three vec4s, then the bands.
const UNIFORMS: usize = 12 + BANDS;

/// The shader's uniforms, as the WGSL `Glow` struct.
fn uniforms(p: &Primitive, linear: bool) -> [f32; UNIFORMS] {
    let color = |c: Color| {
        let [r, g, b, _] = if linear {
            c.into_linear()
        } else {
            [c.r, c.g, c.b, c.a]
        };
        [r, g, b, 1.0]
    };
    let mut out = [0.0; UNIFORMS];
    out[..4].copy_from_slice(&[p.size.width, p.size.height, p.params.rise, p.params.opacity]);
    out[4..8].copy_from_slice(&color(p.params.a1));
    out[8..12].copy_from_slice(&color(p.params.a2));
    out[12..].copy_from_slice(&p.params.bands);
    out
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(
        &self,
        pipeline: &mut Pipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        let bytes: Vec<u8> = uniforms(self, pipeline.linear)
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        queue.write_buffer(&pipeline.uniforms, 0, &bytes);
    }

    fn draw(&self, pipeline: &Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        // iced has set the viewport to our bounds; one triangle covers it.
        render_pass.set_pipeline(if self.params.additive {
            &pipeline.additive
        } else {
            &pipeline.over
        });
        render_pass.set_bind_group(0, &pipeline.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

struct Pipeline {
    additive: wgpu::RenderPipeline,
    over: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// The target stores linear colour (an sRGB format), so ours must be too.
    linear: bool,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blurt glow"),
            source: wgpu::ShaderSource::Wgsl(include_str!("glow.wgsl").into()),
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blurt glow uniforms"),
            size: (UNIFORMS * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blurt glow"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blurt glow"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blurt glow"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = |blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blurt glow"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        // The shader outputs premultiplied colour.
        let add = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let over = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
            operation: wgpu::BlendOperation::Add,
        };
        Self {
            additive: pipeline(wgpu::BlendState {
                color: add,
                alpha: over,
            }),
            over: pipeline(wgpu::BlendState {
                color: over,
                alpha: over,
            }),
            uniforms,
            bind_group,
            linear: format.is_srgb(),
        }
    }
}

/// The glow for your channel, from its levels as shown. How many are
/// talking sets how high it may rise (about 50 px plus 14 per talker, up
/// to four) and how opaque it gets (0.2 plus 0.05 per talker, capped low
/// so it stays in the background and text above stays readable).
pub fn params(glow: &crate::room::GlowLevels, compact: bool, t: crate::theme::Tokens) -> Params {
    let talkers = glow.talkers.min(4.0);
    let (rise, cap) = if compact {
        (70.0, 0.28)
    } else {
        (50.0 + 14.0 * talkers, 0.36)
    };
    Params {
        bands: glow.bands,
        rise,
        opacity: (0.2 + 0.05 * talkers.max(1.0)).min(cap),
        a1: t.a1,
        a2: t.a2,
        additive: t.mode == crate::theme::Mode::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::GlowLevels;
    use crate::theme::{Accent, Mode, Tokens};

    fn levels(level: f32, talkers: f32) -> GlowLevels {
        let mut glow = GlowLevels::default();
        glow.bands = [level; BANDS];
        glow.talkers = talkers;
        glow
    }

    #[test]
    fn the_shader_compiles() {
        let module = naga::front::wgsl::parse_str(include_str!("glow.wgsl"))
            .unwrap_or_else(|e| panic!("{}", e.emit_to_string(include_str!("glow.wgsl"))));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::empty(),
        )
        .validate(&module)
        .expect("valid WGSL");
        // The uniforms are as big as the struct the shader reads.
        let glow = module
            .types
            .iter()
            .find(|(_, t)| t.name.as_deref() == Some("Glow"))
            .expect("Glow struct");
        assert_eq!(glow.1.inner.size(module.to_ctx()), UNIFORMS as u32 * 4);
    }

    #[test]
    fn silence_draws_nothing() {
        let t = Tokens::new(Mode::Dark, Accent::Zune);
        assert!(!params(&GlowLevels::default(), false, t).visible());
    }

    #[test]
    fn more_talkers_rise_higher_but_opacity_is_capped() {
        let t = Tokens::new(Mode::Dark, Accent::Zune);
        let one = params(&levels(0.5, 1.0), false, t);
        let three = params(&levels(0.5, 3.0), false, t);
        let many = params(&levels(0.5, 8.0), false, t);
        assert!(one.visible());
        assert!(three.rise > one.rise);
        assert!(three.opacity > one.opacity);
        assert_eq!(many.rise, 50.0 + 14.0 * 4.0);
        assert_eq!(many.opacity, 0.36);
        let compact = params(&levels(0.5, 8.0), true, t);
        assert_eq!(compact.rise, 70.0);
        assert_eq!(compact.opacity, 0.28);
    }

    #[test]
    fn light_mode_paints_over() {
        let t = Tokens::new(Mode::Light, Accent::Zune);
        assert!(!params(&levels(0.5, 1.0), false, t).additive);
    }
}
