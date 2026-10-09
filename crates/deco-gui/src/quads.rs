//! Filled rectangles: the current line, the selections and the caret.
//!
//! [`rectangles`] and [`vertices`] are pure, so what is drawn and where can be
//! tested without a graphics device. [`QuadRenderer`] uploads the vertices and
//! draws them before the text, so text stays readable on top of them.

use deco_config::CursorStyle;
use deco_theme::Rgba;

use crate::layout::{Layout, Rect};

/// A rectangle filled with one colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quad {
    pub rect: Rect,
    pub color: Rgba,
}

/// The rectangles to draw for `layout`, back to front.
///
/// The current-line highlight comes first, then the selections over it, then
/// the caret. An outline caret style is drawn as its four edges. Every
/// rectangle is clipped to the editor region.
pub fn rectangles(layout: &Layout) -> Vec<Quad> {
    let mut quads = unclipped(layout);
    let area = layout.editor_area;
    quads.retain_mut(|quad| match clip(quad.rect, area) {
        Some(rect) => {
            quad.rect = rect;
            true
        }
        None => false,
    });
    quads
}

/// The part of `rect` inside `area`, if any.
fn clip(rect: Rect, area: Rect) -> Option<Rect> {
    let left = rect.x.max(area.x);
    let top = rect.y.max(area.y);
    let right = (rect.x + rect.width).min(area.x + area.width);
    let bottom = (rect.y + rect.height).min(area.y + area.height);
    (right > left && bottom > top).then_some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn unclipped(layout: &Layout) -> Vec<Quad> {
    let colors = &layout.colors;
    let mut quads = Vec::new();
    if let Some(rect) = layout.current_line {
        quads.push(Quad {
            rect,
            color: colors.current_line,
        });
    }
    quads.extend(layout.selections.iter().map(|&rect| Quad {
        rect,
        color: colors.selection,
    }));
    if let Some(caret) = layout.cursor {
        let color = colors.cursor;
        if layout.cursor_style == CursorStyle::BlockOutline {
            let edge = (caret.width * 0.12).max(1.0);
            let Rect {
                x,
                y,
                width,
                height,
            } = caret;
            for rect in [
                Rect {
                    x,
                    y,
                    width,
                    height: edge,
                },
                Rect {
                    x,
                    y: y + height - edge,
                    width,
                    height: edge,
                },
                Rect {
                    x,
                    y,
                    width: edge,
                    height,
                },
                Rect {
                    x: x + width - edge,
                    y,
                    width: edge,
                    height,
                },
            ] {
                quads.push(Quad { rect, color });
            }
        } else {
            quads.push(Quad { rect: caret, color });
        }
    }
    quads
}

/// The floats one vertex takes: a position in clip space, then a colour.
pub const FLOATS_PER_VERTEX: usize = 6;

/// Two triangles per quad, as `x, y, r, g, b, a` per vertex.
///
/// Pixels are converted to clip space for a target `width` by `height`
/// pixels, with the origin at the top left. With `linear`, colours are
/// converted from sRGB, as an sRGB target expects; the clear colour in
/// [`crate::app`] is converted the same way.
pub fn vertices(quads: &[Quad], width: f32, height: f32, linear: bool) -> Vec<f32> {
    let mut out = Vec::with_capacity(quads.len() * 6 * FLOATS_PER_VERTEX);
    let channel = |value: u8| {
        let value = f32::from(value) / 255.0;
        if !linear {
            value
        } else if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    for quad in quads {
        let Rect {
            x,
            y,
            width: w,
            height: h,
        } = quad.rect;
        let left = x / width * 2.0 - 1.0;
        let right = (x + w) / width * 2.0 - 1.0;
        let top = 1.0 - y / height * 2.0;
        let bottom = 1.0 - (y + h) / height * 2.0;
        let color = [
            channel(quad.color.r),
            channel(quad.color.g),
            channel(quad.color.b),
            f32::from(quad.color.a) / 255.0,
        ];
        for (px, py) in [
            (left, top),
            (left, bottom),
            (right, top),
            (right, top),
            (left, bottom),
            (right, bottom),
        ] {
            out.extend_from_slice(&[px, py]);
            out.extend_from_slice(&color);
        }
    }
    out
}

const SHADER: &str = r"
struct Vertex {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct Fragment {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(vertex: Vertex) -> Fragment {
    var out: Fragment;
    out.position = vec4<f32>(vertex.position, 0.0, 1.0);
    out.color = vertex.color;
    return out;
}

@fragment
fn fs_main(fragment: Fragment) -> @location(0) vec4<f32> {
    return fragment.color;
}
";

/// The pipeline and vertex buffer that draw [`Quad`]s.
pub struct QuadRenderer {
    pipeline: wgpu::RenderPipeline,
    buffer: wgpu::Buffer,
    /// How many bytes `buffer` holds.
    capacity: u64,
    /// How many vertices the last [`QuadRenderer::prepare`] uploaded.
    count: u32,
}

impl QuadRenderer {
    /// Builds the pipeline for a target of `format`.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("deco quads"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("deco quads"),
            ..Default::default()
        });
        let float = std::mem::size_of::<f32>() as u64;
        let buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: FLOATS_PER_VERTEX as u64 * float,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 2 * float,
                    shader_location: 1,
                },
            ],
        })];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("deco quads"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &buffers,
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::default(),
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let capacity = 1024;
        Self {
            pipeline,
            buffer: Self::buffer(device, capacity),
            capacity,
            count: 0,
        }
    }

    fn buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("deco quads"),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Uploads `vertices`, as made by [`vertices`], growing the buffer when
    /// they do not fit.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, vertices: &[f32]) {
        let bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let size = bytes.len() as u64;
        if size > self.capacity {
            self.capacity = size.next_power_of_two();
            self.buffer = Self::buffer(device, self.capacity);
        }
        if !bytes.is_empty() {
            queue.write_buffer(&self.buffer, 0, &bytes);
        }
        self.count = (vertices.len() / FLOATS_PER_VERTEX) as u32;
    }

    /// Draws what [`QuadRenderer::prepare`] uploaded.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.buffer.slice(..));
        pass.draw(0..self.count, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn the_shader_is_valid_wgsl() {
        // Checked here because CI has no GPU to build the pipeline on.
        use wgpu::naga;
        let module = naga::front::wgsl::parse_str(SHADER).expect("the shader parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::default(),
        )
        .validate(&module)
        .expect("the shader validates");
    }

    #[test]
    fn the_caret_is_drawn_over_the_selection_and_an_outline_as_four_edges() {
        let mut session = deco_editor::Session::with_defaults();
        session.open(std::path::PathBuf::from("/w/file.rs"), "hello");
        session.view.selections = deco_core::SelectionSet::single(deco_core::Selection::new(
            deco_core::Position::new(0, 1),
            deco_core::Position::new(0, 3),
        ));
        let metrics = crate::layout::Metrics {
            font_size: 14.0,
            line_height: 20.0,
            cell_width: 8.0,
            padding: 8.0,
        };
        let laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        let quads = rectangles(&laid);
        let colors: Vec<Rgba> = quads.iter().map(|quad| quad.color).collect();
        assert_eq!(
            colors,
            [
                laid.colors.current_line,
                laid.colors.selection,
                laid.colors.cursor
            ]
        );

        session.document.settings.cursor_style = CursorStyle::BlockOutline;
        let laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        let caret: Vec<Quad> = rectangles(&laid)
            .into_iter()
            .filter(|quad| quad.color == laid.colors.cursor)
            .collect();
        assert_eq!(caret.len(), 4);
    }

    fn selected_session(theme: Option<&str>) -> (deco_editor::Session, crate::layout::Metrics) {
        let mut session = deco_editor::Session::with_defaults();
        if let Some(source) = theme {
            session.set_theme(deco_theme::ColorTheme::from_json(source).expect("a theme"));
        }
        session.open(std::path::PathBuf::from("/w/file.rs"), "hello world");
        session.view.selections = deco_core::SelectionSet::single(deco_core::Selection::new(
            deco_core::Position::new(0, 0),
            deco_core::Position::new(0, 11),
        ));
        let metrics = crate::layout::Metrics {
            font_size: 14.0,
            line_height: 20.0,
            cell_width: 8.0,
            padding: 8.0,
        };
        (session, metrics)
    }

    #[test]
    fn rectangles_stay_inside_the_editor_region() {
        let (session, metrics) = selected_session(None);
        let mut laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        // As if a side bar on the right left the editor only 60 pixels.
        laid.editor_area.width = 60.0;
        for quad in rectangles(&laid) {
            assert!(quad.rect.x + quad.rect.width <= 60.0, "{quad:?}");
        }
        assert_eq!(
            clip(rect(50.0, 0.0, 10.0, 10.0), rect(0.0, 0.0, 40.0, 40.0)),
            None
        );
    }

    #[test]
    fn text_stays_readable_on_an_opaque_selection() {
        // High contrast: white text, and an opaque white selection.
        let (session, metrics) = selected_session(Some(r#"{ "type": "hc-black", "colors": {} }"#));
        let laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        assert_eq!(laid.colors.selection_text, Some(laid.colors.background));
        assert_eq!(laid.lines[0].recolored, [(0..11, laid.colors.background)]);

        // A translucent selection keeps the theme's foreground.
        let (session, metrics) = selected_session(None);
        let laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        assert_eq!(laid.colors.selection_text, None);
        assert!(laid.lines[0].recolored.is_empty());
    }

    #[test]
    fn the_character_under_a_block_caret_takes_the_caret_text_colour() {
        let (mut session, metrics) = selected_session(None);
        session.view.selections = deco_core::SelectionSet::caret(deco_core::Position::new(0, 2));
        session.document.settings.cursor_style = CursorStyle::Block;
        let laid = crate::layout::layout(&session, 400.0, 100.0, metrics);
        assert_eq!(laid.lines[0].recolored, [(2..3, laid.colors.cursor_text)]);
    }

    #[test]
    fn a_quad_covers_its_pixels_in_clip_space() {
        let quad = Quad {
            rect: rect(0.0, 0.0, 50.0, 25.0),
            color: Rgba::new(255, 0, 0, 255),
        };
        let out = vertices(&[quad], 100.0, 50.0, false);
        assert_eq!(out.len(), 6 * FLOATS_PER_VERTEX);
        let corners: Vec<(f32, f32)> = out
            .chunks(FLOATS_PER_VERTEX)
            .map(|vertex| (vertex[0], vertex[1]))
            .collect();
        // The top-left quarter of the target.
        assert!(corners.contains(&(-1.0, 1.0)));
        assert!(corners.contains(&(0.0, 0.0)));
        assert_eq!(&out[2..6], &[1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn colours_are_linear_for_an_srgb_target() {
        let quad = Quad {
            rect: rect(0.0, 0.0, 1.0, 1.0),
            color: Rgba::new(128, 128, 128, 128),
        };
        let out = vertices(&[quad], 1.0, 1.0, true);
        assert!((out[2] - 0.2158).abs() < 0.001, "{}", out[2]);
        // Alpha is not a colour and is not converted.
        assert!((out[5] - 128.0 / 255.0).abs() < 0.001);
    }
}
