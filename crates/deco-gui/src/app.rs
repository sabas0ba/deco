//! The window, the GPU device, and the event loop.
//!
//! This is the only module that needs a display. What it draws is the frame
//! the terminal frontend's renderer lays out, placed in pixels by
//! [`mod@crate::grid`], which is testable without one. Changes to how the
//! editor looks are normally made in the renderer, not here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context as _, Result};
use deco_editor::Session;
use deco_theme::Rgba;
use deco_tui::{Driver, Flow, Options, RemoteSession};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Color, Family, FontSystem, Metrics as TextMetrics,
    Resolution, Shaping, Style, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::grid::{Grid, Metrics};
use crate::keys::chord_from_event;

fn to_glyphon(color: Rgba) -> Color {
    Color::rgba(color.r, color.g, color.b, color.a)
}

/// The advance width of one character in `family` at the size in `metrics`.
///
/// Measured on `M`, whose advance equals every other glyph's in a monospace
/// face. Returns zero when nothing was shaped, which
/// [`Metrics::with_cell_width`] ignores.
fn measure_advance(font_system: &mut FontSystem, family: &str, metrics: Metrics) -> f32 {
    let mut buffer = TextBuffer::new(
        font_system,
        TextMetrics::new(metrics.font_size, metrics.line_height),
    );
    let mut buffer = buffer.borrow_with(font_system);
    let attrs = Attrs::new().family(Family::Name(family));
    buffer.set_text("M", &attrs, Shaping::Advanced, None);
    buffer
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first())
        .map_or(0.0, |glyph| glyph.w)
}

/// Everything that only exists once a window and device are available.
struct Gpu {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    font_system: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    /// Draws the filled rectangles under the text.
    quads: crate::quads::QuadRenderer,
    /// The last measured cell width, with the family and size it was
    /// measured for. Measuring shapes text, so it is repeated only when
    /// either changes.
    advance: Option<(String, f32, f32)>,
}

impl Gpu {
    fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .context("could not create a drawing surface for the window")?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| anyhow!("no suitable GPU adapter was found: {e}"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("deco"),
            ..Default::default()
        }))
        .context("could not open the GPU device")?;

        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(capabilities.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Srgb,
        };
        surface.configure(&device, &config);

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let quads = crate::quads::QuadRenderer::new(&device, format);

        Ok(Self {
            window,
            device,
            queue,
            surface,
            config,
            font_system: FontSystem::new(),
            swash: SwashCache::new(),
            viewport,
            atlas,
            renderer,
            quads,
            advance: None,
        })
    }

    /// The metrics for `session`, with the cell width measured from the font.
    fn metrics(&mut self, session: &Session) -> Metrics {
        let estimate = Metrics::from_session(session, self.window.scale_factor() as f32);
        let family = &session.document.settings.font_family;
        let measured = match &self.advance {
            Some((known, size, width)) if known == family && *size == estimate.font_size => *width,
            _ => {
                let width = measure_advance(&mut self.font_system, family, estimate);
                self.advance = Some((family.clone(), estimate.font_size, width));
                width
            }
        };
        estimate.with_cell_width(measured)
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Draws one frame: `grid`, which [`crate::grid::grid`] placed.
    fn draw(&mut self, grid: &Grid, family: &str, metrics: Metrics) -> Result<()> {
        let width = self.config.width as f32;
        let height = self.config.height as f32;

        // One glyphon buffer per run. Runs are bounded by the window's cells,
        // so this stays small however large the file is.
        let text_metrics = TextMetrics::new(metrics.font_size, metrics.line_height);
        let attrs = Attrs::new().family(Family::Name(family));
        let mut buffers: Vec<(TextBuffer, f32, f32, Color)> = Vec::with_capacity(grid.runs.len());
        for run in &grid.runs {
            let mut buffer = TextBuffer::new(&mut self.font_system, text_metrics);
            let attrs = if run.italic {
                attrs.clone().style(Style::Italic)
            } else {
                attrs.clone()
            };
            buffer.borrow_with(&mut self.font_system).set_text(
                &run.text,
                &attrs,
                Shaping::Advanced,
                None,
            );
            buffers.push((buffer, run.x, run.y, to_glyphon(run.fg)));
        }

        let bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        };
        let areas: Vec<TextArea<'_>> = buffers
            .iter()
            .map(|(buffer, left, top, color)| TextArea {
                buffer,
                left: *left,
                top: *top,
                scale: 1.0,
                bounds,
                default_color: *color,
                custom_glyphs: &[],
            })
            .collect();

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        self.renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash,
            )
            .context("could not prepare text for drawing")?;
        let rectangles =
            crate::quads::vertices(&grid.quads, width, height, self.config.format.is_srgb());
        self.quads.prepare(&self.device, &self.queue, &rectangles);

        use wgpu::CurrentSurfaceTexture;
        let frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            }
            // A surface becomes lost or outdated when the window is resized or
            // the display changes. Reconfigure and skip one frame; this is not
            // an error.
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            // Occluded means nothing would be visible. A timeout means the
            // compositor is behind. Both resolve on a later frame.
            CurrentSurfaceTexture::Occluded | CurrentSurfaceTexture::Timeout => return Ok(()),
            other => return Err(anyhow!("could not acquire a frame: {other:?}")),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let background = grid.background;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("deco text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: srgb_to_linear(background.r),
                            g: srgb_to_linear(background.g),
                            b: srgb_to_linear(background.b),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Under the text, so the text stays readable on a selection.
            self.quads.render(&mut pass);
            self.renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;
        }

        self.queue.submit(Some(encoder.finish()));
        // wgpu 30 presents through the queue rather than the texture.
        self.queue.present(frame);
        self.atlas.trim();
        Ok(())
    }
}

/// Converts an 8-bit sRGB channel to the linear value wgpu's clear colour wants.
fn srgb_to_linear(value: u8) -> f64 {
    let v = value as f64 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

struct App<'a> {
    session: &'a mut Session,
    /// What the driver starts with; taken when the window opens.
    options: Option<Options>,
    /// The frontend-independent event core the terminal frontend also uses:
    /// what a key does, files, language servers, Git, terminals.
    driver: Option<Driver>,
    gpu: Option<Gpu>,
    modifiers: ModifiersState,
    started: Instant,
    error: Option<anyhow::Error>,
}

impl App<'_> {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// The grid size for the window as it is now, telling the driver when
    /// it changed.
    fn fit(&mut self) {
        let (Some(gpu), Some(driver)) = (self.gpu.as_mut(), self.driver.as_mut()) else {
            return;
        };
        let metrics = gpu.metrics(self.session);
        let (cols, rows) = metrics.cells(gpu.config.width as f32, gpu.config.height as f32);
        driver.resize(self.session, cols, rows);
    }
}

impl ApplicationHandler for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("{} — deco", self.session.document.title()))
            .with_inner_size(winit::dpi::LogicalSize::new(1024.0, 700.0));

        match event_loop
            .create_window(attributes)
            .map_err(anyhow::Error::from)
            .and_then(|window| Gpu::new(Arc::new(window)))
        {
            Ok(mut gpu) => {
                let metrics = gpu.metrics(self.session);
                let size = metrics.cells(gpu.config.width as f32, gpu.config.height as f32);
                let options = Options {
                    size,
                    ..self.options.take().unwrap_or_default()
                };
                self.driver = Some(Driver::start(self.session, options));
                gpu.window.request_redraw();
                self.gpu = Some(gpu);
            }
            Err(error) => {
                // Return the error instead of panicking, so the user sees
                // "no GPU adapter was found" instead of a backtrace.
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if self.gpu.is_none() || self.driver.is_none() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                self.fit();
                self.request_redraw();
            }
            // A different monitor or a changed system scale: the font's size in
            // pixels changes, so the grid does.
            WindowEvent::ScaleFactorChanged { .. } => {
                self.fit();
                self.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(chord) = chord_from_event(&event, self.modifiers) else {
                    return;
                };
                let now_ms = self.now_ms();
                let Some(driver) = self.driver.as_mut() else {
                    return;
                };
                match driver.key(self.session, chord, now_ms) {
                    Ok(Flow::Quit) => event_loop.exit(),
                    Ok(Flow::Continue) => {}
                    Err(error) => {
                        // A failed local save: reported, as the terminal
                        // frontend reports it when it exits.
                        self.error = Some(error);
                        event_loop.exit();
                    }
                }
                // The font may have changed with a setting the key changed.
                self.fit();
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    /// Between events: what the language servers, Git and the terminals sent,
    /// and the auto-save check, then a wake-up after the driver's interval.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now_ms = self.now_ms();
        let Some(driver) = self.driver.as_mut() else {
            return;
        };
        if let Err(error) = driver.idle(self.session, now_ms) {
            self.error = Some(error);
            event_loop.exit();
            return;
        }
        // The terminal frontend writes copies to the terminal's clipboard with
        // OSC 52. A window has no terminal to write to, so copies stay inside
        // deco; the sequence is taken so it does not build up.
        let _ = driver.terminal_output(self.session);
        let interval = driver.poll_interval();
        if driver.needs_redraw() {
            self.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + interval));
    }
}

impl App<'_> {
    fn request_redraw(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.window.request_redraw();
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(gpu), Some(driver)) = (self.gpu.as_mut(), self.driver.as_mut()) else {
            return;
        };
        let frame = driver.frame(self.session);
        let metrics = gpu.metrics(self.session);
        let background = self
            .session
            .theme
            .color("editor.background")
            .unwrap_or(Rgba::BLACK);
        let caret = crate::grid::caret_for(self.session);
        let grid = crate::grid::grid(&frame, metrics, background, Some(caret));
        let family = self.session.document.settings.font_family.clone();
        gpu.window
            .set_title(&format!("{} — deco", self.session.document.title()));
        if let Err(error) = gpu.draw(&grid, &family, metrics) {
            self.error = Some(error);
            event_loop.exit();
        }
    }
}

/// Runs the GPU frontend: a window that shows the editor the terminal
/// frontend shows, driven by the same [`Driver`].
///
/// `started_with` and `remote` are what [`deco_tui::run_with`] takes: the
/// file deco was started with, and the connection when every file is on
/// another machine.
pub fn run(
    session: &mut Session,
    started_with: Option<PathBuf>,
    remote: Option<RemoteSession>,
) -> Result<()> {
    let event_loop = EventLoop::new().context("could not start the window event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        session,
        options: Some(Options {
            started_with,
            remote,
            ..Options::default()
        }),
        driver: None,
        gpu: None,
        modifiers: ModifiersState::empty(),
        started: Instant::now(),
        error: None,
    };
    let ran = event_loop
        .run_app(&mut app)
        .context("the window event loop failed");
    if let Some(driver) = app.driver.as_mut() {
        driver.shutdown();
    }
    ran?;
    match app.error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_converted_for_glyphon() {
        let color = to_glyphon(Rgba::new(1, 2, 3, 4));
        assert_eq!((color.r(), color.g(), color.b(), color.a()), (1, 2, 3, 4));
    }

    #[test]
    fn srgb_conversion_keeps_the_endpoints() {
        assert_eq!(srgb_to_linear(0), 0.0);
        assert!((srgb_to_linear(255) - 1.0).abs() < 1e-9);
        // And is monotonic in between.
        assert!(srgb_to_linear(128) > srgb_to_linear(64));
    }
}
