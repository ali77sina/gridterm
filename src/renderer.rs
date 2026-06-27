use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use wgpu::util::DeviceExt;
use wgpu::{
    CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor, MultisampleState,
    PresentMode, SurfaceConfiguration, TextureFormat, TextureUsages,
};
use winit::window::Window;

use crate::grid::Rect;

/// A laid-out pane ready to render: its pixel rect and a shaped text buffer
/// holding the visible grid contents.
pub struct PaneDraw {
    pub rect: Rect,
    pub buffer: TextBuffer,
}

/// A solid-color rectangle in pixel space (cursor block, selection, borders).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct QuadInstance {
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
}

const QUAD_VERTS: [Vertex; 6] = [
    Vertex { pos: [0.0, 0.0] },
    Vertex { pos: [1.0, 0.0] },
    Vertex { pos: [0.0, 1.0] },
    Vertex { pos: [0.0, 1.0] },
    Vertex { pos: [1.0, 0.0] },
    Vertex { pos: [1.0, 1.0] },
];

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    instance: wgpu::Instance,

    pub font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,

    // Quad pipeline state.
    quad_pipeline: wgpu::RenderPipeline,
    quad_vbo: wgpu::Buffer,
    quad_instances: wgpu::Buffer,
    quad_capacity: u64,
    screen_uniform: wgpu::Buffer,
    quad_bind_group: wgpu::BindGroup,

    pub font_size: f32,
    pub line_height: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    /// Vertical offset + height for the cursor block within a cell, aligned to
    /// where the glyph ink actually renders.
    pub cursor_top: f32,
    pub cursor_height: f32,

    // Reused scratch buffers for set_pane_rows (avoid per-frame allocation).
    scratch_text: String,
    scratch_ranges: Vec<(usize, usize, Color)>,
    // Text buffers for the AI chat overlay.
    overlay_buffer: TextBuffer,
    input_buffer: TextBuffer,
    /// Pool of small buffers for per-pane cost badges (reused across frames).
    badge_buffers: Vec<TextBuffer>,
    /// Last shaped text per badge buffer, to skip redundant re-shaping.
    badge_cache: Vec<String>,
    /// Current frame's badge placements: (buffer index, x, y).
    badge_layout: Vec<(usize, f32, f32)>,

    window: Arc<Window>,
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();

        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("gridterm-device"),
                ..Default::default()
            })
            .await
            .unwrap();

        let swapchain_format = TextureFormat::Bgra8UnormSrgb;
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: swapchain_format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, swapchain_format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);

        // --- Quad pipeline ---
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
        });

        let screen_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screen-uniform"),
            size: 16, // vec2<f32> padded to 16 bytes
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("quad-bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let quad_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad-bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_uniform.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<QuadInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: swapchain_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let quad_vbo = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-vbo"),
            contents: bytemuck::cast_slice(&QUAD_VERTS),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let quad_capacity = 256;
        let quad_instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-instances"),
            size: quad_capacity * std::mem::size_of::<QuadInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let scale = window.scale_factor() as f32;
        let font_size = 14.0 * scale;
        // Tight leading so glyphs fill their cell; large leading makes the
        // visible glyph drift toward the bottom of the line box, which makes
        // the cursor block look like it sits a row above the text.
        let line_height = (font_size * 1.2).ceil();

        let (cell_w, cell_h, cursor_top, cursor_height) =
            measure_cell(&mut font_system, font_size, line_height);

        // Chat overlay text buffers (proportional UI font, not the grid metric).
        let ui_metrics = Metrics::new(font_size, line_height);
        let mut overlay_buffer = TextBuffer::new(&mut font_system, ui_metrics);
        overlay_buffer.set_wrap(&mut font_system, glyphon::cosmic_text::Wrap::Word);
        let mut input_buffer = TextBuffer::new(&mut font_system, ui_metrics);
        input_buffer.set_wrap(&mut font_system, glyphon::cosmic_text::Wrap::Word);

        Self {
            device,
            queue,
            surface,
            config,
            instance,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            quad_pipeline,
            quad_vbo,
            quad_instances,
            quad_capacity,
            screen_uniform,
            quad_bind_group,
            font_size,
            line_height,
            cell_w,
            cell_h,
            cursor_top,
            cursor_height,
            scratch_text: String::new(),
            scratch_ranges: Vec::new(),
            overlay_buffer,
            input_buffer,
            badge_buffers: Vec::new(),
            badge_cache: Vec::new(),
            badge_layout: Vec::new(),
            window,
        }
    }

    pub fn size(&self) -> (f32, f32) {
        (self.config.width as f32, self.config.height as f32)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Create a fresh text buffer for a pane sized to its rect.
    pub fn new_pane_buffer(&mut self, rect: Rect) -> TextBuffer {
        let mut buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(self.font_size, self.line_height),
        );
        // The terminal grid already decides line breaks; never let cosmic-text
        // re-wrap, or full-width rows would spill onto extra visual lines and
        // push everything below them out of sync with the cursor's grid row.
        buf.set_wrap(&mut self.font_system, glyphon::cosmic_text::Wrap::None);
        // Height is bounded by the pane, width is unbounded (no wrapping).
        buf.set_size(&mut self.font_system, None, Some(rect.h));
        // Pin glyph advance to our cell width for exact column alignment.
        buf.set_monospace_width(&mut self.font_system, Some(self.cell_w));
        buf
    }

    /// Build the pane's text buffer from styled rows (per-cell colors).
    /// Uses persistent scratch buffers to avoid per-frame allocation.
    pub fn set_pane_rows(&mut self, buf: &mut TextBuffer, rows: &[crate::pty::RowRuns]) {
        let base = Attrs::new().family(Family::Monospace).weight(Weight::NORMAL);

        // Reuse scratch storage across calls.
        self.scratch_text.clear();
        self.scratch_ranges.clear();

        for (ri, row) in rows.iter().enumerate() {
            if ri > 0 {
                self.scratch_text.push('\n');
            }
            for run in &row.runs {
                let start = self.scratch_text.len();
                self.scratch_text.push_str(&run.text);
                let end = self.scratch_text.len();
                self.scratch_ranges
                    .push((start, end, Color::rgb(run.fg.r, run.fg.g, run.fg.b)));
            }
        }

        // Build borrowed spans referencing the scratch string, filling gaps
        // (e.g. newlines) with default-colored runs.
        let text = &self.scratch_text;
        let mut span_vec: Vec<(&str, Attrs)> = Vec::with_capacity(self.scratch_ranges.len() * 2);
        let mut cur = 0usize;
        for &(s, e, color) in &self.scratch_ranges {
            if s > cur {
                span_vec.push((&text[cur..s], base.clone()));
            }
            span_vec.push((&text[s..e], base.clone().color(color)));
            cur = e;
        }
        if cur < text.len() {
            span_vec.push((&text[cur..], base.clone()));
        }

        buf.set_rich_text(
            &mut self.font_system,
            span_vec.into_iter(),
            &base,
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut self.font_system, false);
    }

    /// Set the chat conversation as styled segments (text, optional color).
    /// Returns the total laid-out pixel height for scrolling/clipping.
    pub fn set_overlay_segments(
        &mut self,
        segments: &[(String, Option<Color>)],
        width: f32,
    ) -> f32 {
        let base = Attrs::new().family(Family::SansSerif);
        self.overlay_buffer
            .set_size(&mut self.font_system, Some(width), None);
        let spans = segments.iter().map(|(t, c)| {
            let a = match c {
                Some(col) => base.clone().color(*col),
                None => base.clone(),
            };
            (t.as_str(), a)
        });
        self.overlay_buffer.set_rich_text(
            &mut self.font_system,
            spans,
            &base,
            Shaping::Advanced,
            None,
        );
        self.overlay_buffer
            .shape_until_scroll(&mut self.font_system, false);
        buffer_height(&self.overlay_buffer, self.line_height)
    }

    /// Set the chat input-box text. Returns its laid-out pixel height.
    pub fn set_input_text(&mut self, text: &str, width: f32) -> f32 {
        let attrs = Attrs::new().family(Family::SansSerif);
        self.input_buffer
            .set_size(&mut self.font_system, Some(width), None);
        self.input_buffer
            .set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
        self.input_buffer
            .shape_until_scroll(&mut self.font_system, false);
        buffer_height(&self.input_buffer, self.line_height)
    }

    /// Measure the caret pixel position (x offset on its line, line index) for
    /// a given prefix string laid out at `width`. Reuses a scratch buffer.
    /// Set per-pane cost badges. Each entry is (text, right_edge_x, top_y);
    /// the badge is right-aligned to right_edge_x. Returns the measured pill
    /// rects (x, y, w, h) so the caller can draw a background behind each.
    pub fn set_badges(&mut self, badges: &[(String, f32, f32)]) -> Vec<[f32; 4]> {
        self.badge_layout.clear();
        let mut rects = Vec::with_capacity(badges.len());
        let metrics = Metrics::new(self.font_size * 0.8, self.line_height * 0.8);
        let pad_x = 6.0;
        let line_h = self.line_height * 0.8;
        for (i, (text, right_x, top_y)) in badges.iter().enumerate() {
            if self.badge_buffers.len() <= i {
                self.badge_buffers
                    .push(TextBuffer::new(&mut self.font_system, metrics));
                self.badge_cache.push(String::new());
            }
            let buf = &mut self.badge_buffers[i];
            // Only re-shape when this badge's text changed (cheap when idle).
            if self.badge_cache[i] != *text {
                buf.set_size(&mut self.font_system, Some(400.0), Some(self.line_height));
                let attrs = Attrs::new().family(Family::SansSerif);
                buf.set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
                buf.shape_until_scroll(&mut self.font_system, false);
                self.badge_cache[i] = text.clone();
            }
            let w: f32 = buf
                .layout_runs()
                .next()
                .map(|r| r.glyphs.iter().map(|g| g.x + g.w).fold(0.0, f32::max))
                .unwrap_or(0.0);
            let text_x = right_x - w;
            self.badge_layout.push((i, text_x, *top_y + 2.0));
            // Pill background rect (a little padding around the text).
            rects.push([
                text_x - pad_x,
                *top_y - 1.0,
                w + pad_x * 2.0,
                line_h + 4.0,
            ]);
        }
        rects
    }

    pub fn measure_caret(&mut self, prefix: &str, width: f32) -> (f32, usize) {
        let attrs = Attrs::new().family(Family::SansSerif);
        self.input_buffer
            .set_size(&mut self.font_system, Some(width), None);
        self.input_buffer
            .set_text(&mut self.font_system, prefix, &attrs, Shaping::Advanced, None);
        self.input_buffer
            .shape_until_scroll(&mut self.font_system, false);
        let mut x = 0.0f32;
        let mut line = 0usize;
        for (li, run) in self.input_buffer.layout_runs().enumerate() {
            line = li;
            // Rightmost glyph edge on the last line = caret x.
            x = run
                .glyphs
                .iter()
                .map(|g| g.x + g.w)
                .fold(0.0, f32::max);
        }
        (x, line)
    }

    /// Hit-test a click at local (x, y) within input `text` laid out at `width`,
    /// returning the absolute byte offset into `text`.
    pub fn hit_input(&mut self, text: &str, width: f32, x: f32, y: f32) -> usize {
        let attrs = Attrs::new().family(Family::SansSerif);
        self.input_buffer
            .set_size(&mut self.font_system, Some(width), None);
        self.input_buffer
            .set_text(&mut self.font_system, text, &attrs, Shaping::Advanced, None);
        self.input_buffer
            .shape_until_scroll(&mut self.font_system, false);
        match self.input_buffer.hit(x, y) {
            Some(cursor) => {
                let mut offset = 0usize;
                for (i, line) in text.split_inclusive('\n').enumerate() {
                    if i == cursor.line {
                        return offset + cursor.index.min(line.len());
                    }
                    offset += line.len();
                }
                text.len()
            }
            None => text.len(),
        }
    }

    /// Render panes + quads + the chat overlay. `convo_rect` positions the
    /// conversation text (may be shifted up for scrolling), `convo_clip` bounds
    /// it to the visible area, `input_rect` positions the input text.
    pub fn render_with_overlay(
        &mut self,
        panes: &mut [PaneDraw],
        quads: &[QuadInstance],
        convo_rect: Rect,
        convo_clip: Rect,
        input_rect: Rect,
    ) {
        self.render_inner(panes, quads, Some((convo_rect, convo_clip, input_rect)));
    }

    /// Render all panes plus the provided quads (drawn behind the text).
    pub fn render(&mut self, panes: &mut [PaneDraw], quads: &[QuadInstance]) {
        self.render_inner(panes, quads, None);
    }

    fn render_inner(
        &mut self,
        panes: &mut [PaneDraw],
        quads: &[QuadInstance],
        overlay: Option<(Rect, Rect, Rect)>,
    ) {
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        // Upload screen size + quad instances.
        let screen = [self.config.width as f32, self.config.height as f32, 0.0, 0.0];
        self.queue
            .write_buffer(&self.screen_uniform, 0, bytemuck::cast_slice(&screen));

        let quad_count = quads.len() as u32;
        if quad_count as u64 > self.quad_capacity {
            self.quad_capacity = (quad_count as u64).next_power_of_two();
            self.quad_instances = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("quad-instances"),
                size: self.quad_capacity * std::mem::size_of::<QuadInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if quad_count > 0 {
            self.queue
                .write_buffer(&self.quad_instances, 0, bytemuck::cast_slice(quads));
        }

        let mut text_areas: Vec<TextArea> = panes
            .iter()
            .map(|p| {
                // Per-glyph colors come from the rich-text spans; this is just
                // the fallback for any unstyled text.
                let color = Color::rgb(
                    crate::color::DEFAULT_FG.r,
                    crate::color::DEFAULT_FG.g,
                    crate::color::DEFAULT_FG.b,
                );
                TextArea {
                    buffer: &p.buffer,
                    left: p.rect.x,
                    top: p.rect.y,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: p.rect.x as i32,
                        top: p.rect.y as i32,
                        right: (p.rect.x + p.rect.w) as i32,
                        bottom: (p.rect.y + p.rect.h) as i32,
                    },
                    default_color: color,
                    custom_glyphs: &[],
                }
            })
            .collect();

        // Per-pane cost badges (right-aligned in each pane's top-right corner).
        for (bi, bx, by) in &self.badge_layout {
            text_areas.push(TextArea {
                buffer: &self.badge_buffers[*bi],
                left: *bx,
                top: *by,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: Color::rgb(0xa6, 0xed, 0xe1),
                custom_glyphs: &[],
            });
        }

        // Append chat overlay text areas if present.
        if let Some((convo_rect, convo_clip, input_rect)) = overlay {
            text_areas.push(TextArea {
                buffer: &self.overlay_buffer,
                left: convo_rect.x,
                top: convo_rect.y,
                scale: 1.0,
                bounds: TextBounds {
                    left: convo_clip.x as i32,
                    top: convo_clip.y as i32,
                    right: (convo_clip.x + convo_clip.w) as i32,
                    bottom: (convo_clip.y + convo_clip.h) as i32,
                },
                default_color: Color::rgb(0xcd, 0xd6, 0xf4),
                custom_glyphs: &[],
            });
            text_areas.push(TextArea {
                buffer: &self.input_buffer,
                left: input_rect.x,
                top: input_rect.y,
                scale: 1.0,
                bounds: TextBounds {
                    left: input_rect.x as i32,
                    top: input_rect.y as i32,
                    right: (input_rect.x + input_rect.w) as i32,
                    bottom: (input_rect.y + input_rect.h + 40.0) as i32,
                },
                default_color: Color::rgb(0x9c, 0xc1, 0xff),
                custom_glyphs: &[],
            });
        }

        if self
            .text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .is_err()
        {
            return;
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("gridterm-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gridterm-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.027,
                            g: 0.027,
                            b: 0.043,
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

            // Quads first (cursor block, selection) so text draws on top.
            if quad_count > 0 {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_bind_group(0, &self.quad_bind_group, &[]);
                pass.set_vertex_buffer(0, self.quad_vbo.slice(..));
                pass.set_vertex_buffer(1, self.quad_instances.slice(..));
                pass.draw(0..6, 0..quad_count);
            }

            let _ = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass);
        }

        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        frame.present();
        self.atlas.trim();
    }
}

/// Measure monospace cell metrics: advance width, line height, and the
/// vertical offset + height of the glyph ink within the line box, so the
/// cursor block sits exactly over the text instead of floating above it.
fn measure_cell(
    font_system: &mut FontSystem,
    font_size: f32,
    line_height: f32,
) -> (f32, f32, f32, f32) {
    let mut buf = TextBuffer::new(font_system, Metrics::new(font_size, line_height));
    buf.set_size(font_system, Some(1000.0), Some(line_height * 4.0));
    let attrs = Attrs::new().family(Family::Monospace);
    buf.set_text(font_system, "Mg", &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    let mut width = font_size * 0.6; // fallback
    for run in buf.layout_runs() {
        for glyph in run.glyphs.iter() {
            if glyph.w > 0.0 {
                width = glyph.w;
            }
        }
    }
    // Cursor block: nearly the full cell with a tiny inset so it reads as a
    // block over the glyph without spilling into neighbouring rows.
    let cursor_top = (line_height * 0.08).round();
    let cursor_height = line_height - cursor_top * 2.0;
    (width, line_height, cursor_top, cursor_height)
}

/// Total laid-out pixel height of a text buffer (number of visual lines x
/// line height), used to size the input box and clamp chat scrolling.
fn buffer_height(buf: &TextBuffer, line_height: f32) -> f32 {
    let lines = buf.layout_runs().count().max(1);
    lines as f32 * line_height
}
