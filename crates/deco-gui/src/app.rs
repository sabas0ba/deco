//! The window, the GPU device, and the event loop.
//!
//! This is the only module that needs a display. Everything it draws comes from
//! [`mod@crate::layout`], which is testable without one, so a change to how the
//! editor looks is normally a change over there rather than in here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context as _, Result};
use deco_editor::{Outcome, Session};
use deco_theme::Rgba;
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Color, Family, FontSystem, Metrics as TextMetrics,
    Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::keys::chord_from_event;
use crate::layout::{self, Metrics};

fn to_glyphon(color: Rgba) -> Color {
    Color::rgba(color.r, color.g, color.b, color.a)
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
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Draws one frame.
    fn draw(&mut self, session: &Session) -> Result<()> {
        let scale = self.window.scale_factor() as f32;
        let metrics = Metrics::from_session(session, scale);
        let width = self.config.width as f32;
        let height = self.config.height as f32;
        let laid_out = layout::layout(session, width, height, metrics);

        // One glyphon buffer per visible line. Line count is bounded by the
        // window height, so this stays small however large the file is.
        let mut buffers: Vec<(TextBuffer, f32, f32, Color)> = Vec::new();
        let text_metrics = TextMetrics::new(metrics.font_size, metrics.line_height);
        let family = session.document.settings.font_family.clone();
        let attrs = Attrs::new().family(Family::Name(&family));

        for line in &laid_out.lines {
            if !line.gutter.is_empty() {
                let mut buffer = TextBuffer::new(&mut self.font_system, text_metrics);
                buffer.borrow_with(&mut self.font_system).set_text(
                    &line.gutter,
                    &attrs,
                    Shaping::Advanced,
                    None,
                );
                let color = if line.is_cursor_line {
                    laid_out.colors.gutter_active
                } else {
                    laid_out.colors.gutter
                };
                buffers.push((buffer, metrics.padding, line.y, to_glyphon(color)));
            }
            let mut buffer = TextBuffer::new(&mut self.font_system, text_metrics);
            buffer.borrow_with(&mut self.font_system).set_text(
                &line.text,
                &attrs,
                Shaping::Advanced,
                None,
            );
            buffers.push((
                buffer,
                laid_out.text_left,
                line.y,
                to_glyphon(laid_out.colors.foreground),
            ));
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

        use wgpu::CurrentSurfaceTexture;
        let frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            }
            // A lost or outdated surface is routine — the window was resized,
            // or the display changed. Reconfiguring and skipping one frame is
            // the correct response, not an error.
            CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            // Occluded means nothing would be visible; a timeout means the
            // compositor is behind. Both resolve themselves on the next frame.
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
            let background = laid_out.colors.background;
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
    path: Option<PathBuf>,
    gpu: Option<Gpu>,
    modifiers: ModifiersState,
    started: Instant,
    error: Option<anyhow::Error>,
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
            Ok(gpu) => self.gpu = Some(gpu),
            Err(error) => {
                // Carrying the error out rather than panicking means the user
                // sees "no GPU adapter was found" instead of a backtrace.
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(gpu) = self.gpu.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Resized(size) => {
                gpu.resize(size.width, size.height);
                let metrics = Metrics::from_session(self.session, gpu.window.scale_factor() as f32);
                self.session.resize(
                    (size.width as f32 / metrics.cell_width) as usize,
                    (size.height as f32 / metrics.line_height) as usize,
                );
                gpu.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let Some(chord) = chord_from_event(&event, self.modifiers) else {
                    return;
                };
                let now_ms = self.started.elapsed().as_millis() as u64;
                match self.session.handle_chord(chord, now_ms) {
                    Outcome::Quit => event_loop.exit(),
                    Outcome::Save => {
                        if let Err(error) = save(self.session, self.path.as_ref()) {
                            self.session.status = Some(error.to_string());
                        }
                    }
                    _ => {}
                }
                refuse_overlays(self.session);
                gpu.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = gpu.draw(self.session) {
                    self.error = Some(error);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
}

/// Closes any bar or prompt a command opened.
///
/// This frontend has nowhere to draw them — no status bar, no chrome of any kind
/// yet — and a widget that is invisible while holding the keyboard would look
/// exactly like an editor that had stopped responding. `ctrl+f`, `ctrl+g` and
/// `ctrl+shift+p` therefore do nothing here rather than something the user cannot
/// see, and say so.
fn refuse_overlays(session: &mut Session) {
    if session.find.visible() {
        session.find.close();
        session.status = Some("the find bar is only in the terminal frontend so far".to_owned());
    }
    if session.prompt.take().is_some() {
        session.status =
            Some("the command palette is only in the terminal frontend so far".to_owned());
    }
}

/// Writes the open document to disk.
fn save(session: &mut Session, path: Option<&PathBuf>) -> Result<()> {
    let target = session.document.path.clone().or_else(|| path.cloned());
    match target {
        Some(path) => {
            std::fs::write(&path, session.save_contents())
                .with_context(|| format!("could not write {}", path.display()))?;
            session.mark_saved();
            session.status = Some(format!("Saved {}", path.display()));
        }
        None => session.status = Some("This document has no filename yet".to_owned()),
    }
    Ok(())
}

/// Opens a window and runs the editor in it.
pub fn run(session: &mut Session, path: Option<PathBuf>) -> Result<()> {
    let event_loop = EventLoop::new().context("could not start the window event loop")?;
    // Wait for input rather than spinning: an editor at rest should use no CPU.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        session,
        path,
        gpu: None,
        modifiers: ModifiersState::empty(),
        started: Instant::now(),
        error: None,
    };
    event_loop
        .run_app(&mut app)
        .context("the window event loop failed")?;

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

    #[test]
    fn saving_an_untitled_document_says_so_instead_of_failing_silently() {
        let mut session = Session::with_defaults();
        save(&mut session, None).unwrap();
        assert!(session.status.as_deref().unwrap().contains("no filename"));
    }

    #[test]
    fn the_find_bar_is_refused_rather_than_left_invisible() {
        let mut session = Session::with_defaults();
        session.run("actions.find", None, 0);
        assert!(session.find.visible());
        refuse_overlays(&mut session);
        assert!(!session.find.visible());
        assert!(session.status.as_deref().unwrap().contains("terminal"));
    }

    #[test]
    fn the_command_palette_is_refused_too() {
        let mut session = Session::with_defaults();
        session.run("workbench.action.showCommands", None, 0);
        assert!(session.prompt.is_some());
        refuse_overlays(&mut session);
        assert!(session.prompt.is_none());
        assert!(session.status.as_deref().unwrap().contains("palette"));
    }

    #[test]
    fn refusing_is_a_no_op_when_nothing_was_opened() {
        let mut session = Session::with_defaults();
        refuse_overlays(&mut session);
        assert!(session.status.is_none(), "nothing to report");
    }
}
