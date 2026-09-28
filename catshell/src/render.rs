//! GPU rendering for the terminal grid.
//!
//! The grid deliberately does not go through egui's text layout. egui caches laid-out
//! text by content, and terminal cells change colour and content constantly, so that
//! cache would miss on every frame and re-tessellate the whole screen — exactly the kind
//! of per-frame CPU work that makes a terminal feel slow.
//!
//! Instead every visible thing is one textured quad: a cell background, a glyph, an
//! underline, the cursor. They share one pipeline and one atlas, so a full screen is a
//! single instanced draw call. Quads are emitted backgrounds-first so that alpha blending
//! layers them correctly without a depth buffer.

use std::collections::HashMap;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{RenderableContent, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use catshell_term::palette::Palette;
use egui_wgpu::wgpu;
use wgpu::util::DeviceExt as _;

use crate::font::{CellMetrics, FontStyle, GlyphAtlas};

/// One textured quad.
///
/// `bytemuck` derives let this go straight into a vertex buffer with no conversion, and
/// `repr(C)` keeps the layout in step with the `@location` bindings in the shader.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    /// `[x, y, width, height]` in physical pixels, relative to the grid's top-left.
    pub rect: [f32; 4],
    /// `[u0, v0, u1, v1]` into the glyph atlas.
    pub uv: [f32; 4],
    /// Colour multiplied with the sampled texel, straight (not premultiplied) alpha.
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    /// Size of *this pane's* viewport in physical pixels.
    ///
    /// Not the window's. egui-wgpu sets the render pass viewport to the pane's rect
    /// before calling `paint`, so clip space maps onto the pane rather than the whole
    /// target — measuring against the window instead squeezes each pane's contents by
    /// the ratio between them, which shows up as panes rendering at different text sizes.
    viewport_size: [f32; 2],
    /// Uniform buffers are laid out in 16-byte units.
    _padding: [f32; 2],
}

/// The pane's size in physical pixels, rounded exactly as egui rounds the viewport it
/// sets, so the grid lands on the same pixels egui clipped it to.
fn viewport_size_in_pixels(rect: egui::Rect, pixels_per_point: f32, screen: [u32; 2]) -> [f32; 2] {
    let (width, height) = (screen[0] as f32, screen[1] as f32);
    let left = (pixels_per_point * rect.min.x).round().clamp(0.0, width);
    let right = (pixels_per_point * rect.max.x).round().clamp(left, width);
    let top = (pixels_per_point * rect.min.y).round().clamp(0.0, height);
    let bottom = (pixels_per_point * rect.max.y).round().clamp(top, height);
    [right - left, bottom - top]
}

fn to_rgba(rgb: Rgb) -> [f32; 4] {
    [
        rgb.r as f32 / 255.0,
        rgb.g as f32 / 255.0,
        rgb.b as f32 / 255.0,
        1.0,
    ]
}

/// How the grid should be drawn, beyond what the terminal itself knows.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Whether this pane has keyboard focus. An unfocused pane draws a hollow cursor,
    /// which is how you tell at a glance which pane your typing goes to.
    pub focused: bool,
    /// Render bold text in the bright colour variant, as older terminals did.
    pub bold_is_bright: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            focused: true,
            bold_is_bright: false,
        }
    }
}

/// Turn a frame of terminal content into quads.
///
/// Pure and GPU-free, so the whole visual result can be asserted in tests.
pub fn build_instances(
    content: RenderableContent<'_>,
    palette: &Palette,
    atlas: &mut GlyphAtlas,
    options: RenderOptions,
) -> Vec<Instance> {
    let metrics = atlas.metrics();
    let solid_uv = atlas.solid_uv();
    let colors = content.colors;
    let display_offset = content.display_offset;
    let default_bg = palette.resolve(colors, NamedColor::Background as usize);

    let cursor_visible = content.mode.contains(TermMode::SHOW_CURSOR)
        && content.cursor.shape != CursorShape::Hidden
        // A cursor scrolled out of view must not be drawn on whatever row now occupies
        // its coordinates.
        && content.cursor.point.line.0 >= 0;
    let cursor_point = content.cursor.point;
    let cursor_color = palette.resolve(colors, NamedColor::Cursor as usize);

    // Backgrounds must all precede foregrounds, or a tall glyph from the row above is
    // painted over by the next row's background.
    let mut backgrounds = Vec::new();
    let mut foregrounds = Vec::new();

    for indexed in content.display_iter {
        let cell = indexed.cell;
        let point = indexed.point;

        // The second half of a double-width character has no content of its own.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let row = point.line.0 + display_offset as i32;
        if row < 0 {
            continue;
        }
        let x = point.column.0 as f32 * metrics.width;
        let y = row as f32 * metrics.height;

        let selected = content.selection.is_some_and(|range| range.contains(point));
        let is_cursor_cell = cursor_visible
            && cursor_point.line == point.line
            && cursor_point.column == point.column;
        let block_cursor =
            is_cursor_cell && options.focused && matches!(content.cursor.shape, CursorShape::Block);

        let (mut fg, mut bg) = cell_colors(cell.fg, cell.bg, cell.flags, colors, palette, options);

        // Selection and a block cursor both work by swapping the cell's colours. Applying
        // both would swap twice and leave the cell looking untouched, so a cursor over a
        // selected cell simply wins.
        if block_cursor {
            fg = default_bg;
            bg = cursor_color;
        } else if selected {
            std::mem::swap(&mut fg, &mut bg);
        }

        let cell_width = if cell.flags.contains(Flags::WIDE_CHAR) {
            metrics.width * 2.0
        } else {
            metrics.width
        };

        // Skip backgrounds that match the window's own, which is most of a typical
        // screen — roughly halving the quad count for ordinary text.
        if bg != default_bg {
            backgrounds.push(Instance {
                rect: [x, y, cell_width, metrics.height],
                uv: solid_uv,
                color: to_rgba(bg),
            });
        }

        if !cell.flags.contains(Flags::HIDDEN) {
            let style = FontStyle {
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
            };
            push_glyph(&mut foregrounds, atlas, cell.c, style, x, y, fg, &metrics);

            // Combining marks share the cell with the character they modify.
            for &mark in cell.zerowidth().unwrap_or(&[]) {
                push_glyph(&mut foregrounds, atlas, mark, style, x, y, fg, &metrics);
            }
        }

        // Underlines and strikeouts are solid rectangles, drawn in the cell's foreground
        // unless the program picked a separate underline colour.
        let underline_color = cell
            .underline_color()
            .map(|color| resolve_color(color, colors, palette, Flags::empty(), options))
            .unwrap_or(fg);
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            foregrounds.push(Instance {
                rect: [
                    x,
                    y + metrics.underline_y,
                    cell_width,
                    metrics.line_thickness,
                ],
                uv: solid_uv,
                color: to_rgba(underline_color),
            });
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            foregrounds.push(Instance {
                rect: [
                    x,
                    y + metrics.strikeout_y,
                    cell_width,
                    metrics.line_thickness,
                ],
                uv: solid_uv,
                color: to_rgba(fg),
            });
        }

        // Non-block cursor shapes are drawn on top rather than by swapping colours.
        if is_cursor_cell && !block_cursor {
            push_cursor(
                &mut foregrounds,
                content.cursor.shape,
                options.focused,
                x,
                y,
                cell_width,
                cursor_color,
                solid_uv,
                &metrics,
            );
        }
    }

    backgrounds.append(&mut foregrounds);
    backgrounds
}

#[allow(clippy::too_many_arguments)]
fn push_glyph(
    out: &mut Vec<Instance>,
    atlas: &mut GlyphAtlas,
    c: char,
    style: FontStyle,
    x: f32,
    y: f32,
    fg: Rgb,
    metrics: &CellMetrics,
) {
    let Some(glyph) = atlas.glyph(c, style) else {
        return;
    };
    out.push(Instance {
        rect: [
            x + glyph.offset[0],
            y + metrics.ascent + glyph.offset[1],
            glyph.size[0],
            glyph.size[1],
        ],
        uv: glyph.uv,
        // A colour glyph carries its own colours, so it is multiplied by white to leave
        // them alone; a mask is white with coverage in alpha, and takes the cell's colour.
        color: if glyph.colored {
            [1.0, 1.0, 1.0, 1.0]
        } else {
            to_rgba(fg)
        },
    });
}

#[allow(clippy::too_many_arguments)]
fn push_cursor(
    out: &mut Vec<Instance>,
    shape: CursorShape,
    focused: bool,
    x: f32,
    y: f32,
    width: f32,
    color: Rgb,
    solid_uv: [f32; 4],
    metrics: &CellMetrics,
) {
    let color = to_rgba(color);
    let thickness = metrics.line_thickness.max(1.0);
    let mut rect = |rect: [f32; 4]| {
        out.push(Instance {
            rect,
            uv: solid_uv,
            color,
        })
    };

    match shape {
        CursorShape::Beam => rect([x, y, thickness, metrics.height]),
        CursorShape::Underline => {
            rect([x, y + metrics.height - thickness, width, thickness]);
        }
        // An unfocused pane shows its cursor as an outline, so only one pane ever looks
        // like it is taking input.
        CursorShape::Block | CursorShape::HollowBlock => {
            rect([x, y, width, thickness]);
            rect([x, y + metrics.height - thickness, width, thickness]);
            rect([x, y, thickness, metrics.height]);
            rect([x + width - thickness, y, thickness, metrics.height]);
        }
        CursorShape::Hidden => {}
    }
    let _ = focused;
}

/// Resolve a cell's foreground and background to concrete colours.
fn cell_colors(
    fg: Color,
    bg: Color,
    flags: Flags,
    colors: &Colors,
    palette: &Palette,
    options: RenderOptions,
) -> (Rgb, Rgb) {
    let mut foreground = resolve_color(fg, colors, palette, flags, options);
    let mut background = resolve_color(bg, colors, palette, Flags::empty(), options);

    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    (foreground, background)
}

/// Map one [`Color`] to RGB, applying the dim and bold attributes.
///
/// Dim and bold shift *named* colours to a different slot rather than scaling the RGB
/// value, which is what the palette's dim entries are for; indexed and direct colours
/// are used exactly as the program specified.
fn resolve_color(
    color: Color,
    colors: &Colors,
    palette: &Palette,
    flags: Flags,
    options: RenderOptions,
) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(index) => {
            let index = usize::from(index);
            // The dim and bright attributes only shift the eight base colours.
            let index = if index < 8 {
                if flags.contains(Flags::DIM) {
                    index + NamedColor::DimBlack as usize
                } else if options.bold_is_bright && flags.contains(Flags::BOLD) {
                    index + 8
                } else {
                    index
                }
            } else {
                index
            };
            palette.resolve(colors, index)
        }
        Color::Named(named) => {
            let named = match named {
                _ if flags.contains(Flags::DIM) => named.to_dim(),
                named if options.bold_is_bright && flags.contains(Flags::BOLD) => named.to_bright(),
                named => named,
            };
            palette.resolve(colors, named as usize)
        }
    }
}

const SHADER: &str = r#"
struct Uniforms {
    viewport_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var atlas: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_index: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOutput {
    // Four vertices of a triangle strip: (0,0), (1,0), (0,1), (1,1).
    let corner = vec2<f32>(f32(vertex_index & 1u), f32(vertex_index >> 1u));

    // Relative to the pane, which is what the render pass viewport covers.
    let pixel = rect.xy + corner * rect.zw;
    // Pixels run right and down from the top-left; clip space runs right and up from the
    // centre.
    let clip = vec2<f32>(
        pixel.x / uniforms.viewport_size.x * 2.0 - 1.0,
        1.0 - pixel.y / uniforms.viewport_size.y * 2.0,
    );

    var out: VertexOutput;
    out.position = vec4<f32>(clip, 0.0, 1.0);
    out.uv = mix(uv.xy, uv.zw, corner);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(atlas, atlas_sampler, in.uv);
    let color = texel * in.color;
    // egui's render pass blends premultiplied alpha, so premultiply on the way out.
    return vec4<f32>(color.rgb * color.a, color.a);
}
"#;

/// GPU resources shared by every terminal pane, stored in egui's callback resources.
pub struct TerminalRenderer {
    pipeline: wgpu::RenderPipeline,
    uniform_layout: wgpu::BindGroupLayout,
    atlas_bind_group: wgpu::BindGroup,
    atlas_texture: wgpu::Texture,
    atlas_size: u32,
    panes: HashMap<u64, PaneBuffers>,
}

/// Per-pane buffers. Panes cannot share one buffer: `prepare` runs for every pane before
/// any `paint`, so a shared buffer would hold only the last pane's quads by the time the
/// first one drew.
struct PaneBuffers {
    instances: wgpu::Buffer,
    capacity: u64,
    count: u32,
    uniforms: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
}

impl TerminalRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat, atlas_size: u32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("catshell terminal shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("catshell terminal uniforms"),
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

        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("catshell glyph atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let atlas_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("catshell glyph atlas"),
            size: wgpu::Extent3d {
                width: atlas_size,
                height: atlas_size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Nearest filtering: glyphs are rasterised at exactly the size they are drawn, so
        // there is nothing to interpolate and linear filtering would only blur them.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("catshell glyph sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("catshell glyph atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("catshell terminal"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("catshell terminal"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, // rect
                        1 => Float32x4, // uv
                        2 => Float32x4, // color
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            uniform_layout,
            atlas_bind_group,
            atlas_texture,
            atlas_size,
            panes: HashMap::new(),
        }
    }

    /// Copy newly rasterised glyph rows into the atlas texture.
    fn upload_atlas(&self, queue: &wgpu::Queue, first_row: u32, rows: u32, data: &[u8]) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: 0,
                    y: first_row,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.atlas_size * 4),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d {
                width: self.atlas_size,
                height: rows,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Upload one pane's quads, growing its buffer if the frame needs more room.
    fn upload_pane(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pane: u64,
        instances: &[Instance],
        uniforms: Uniforms,
    ) {
        let needed = std::mem::size_of_val(instances) as u64;

        let entry = self.panes.entry(pane).or_insert_with(|| {
            let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("catshell pane uniforms"),
                contents: bytemuck::bytes_of(&uniforms),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
            let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("catshell pane uniforms"),
                layout: &self.uniform_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buffer.as_entire_binding(),
                }],
            });
            PaneBuffers {
                instances: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("catshell pane quads"),
                    size: needed.max(1),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                capacity: needed.max(1),
                count: 0,
                uniforms: uniform_buffer,
                uniform_bind_group,
            }
        });

        if entry.capacity < needed {
            // Grow geometrically so that a steadily busier screen does not reallocate
            // every frame.
            let capacity = needed.next_power_of_two();
            entry.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("catshell pane quads"),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            entry.capacity = capacity;
        }

        queue.write_buffer(&entry.uniforms, 0, bytemuck::bytes_of(&uniforms));
        if !instances.is_empty() {
            queue.write_buffer(&entry.instances, 0, bytemuck::cast_slice(instances));
        }
        entry.count = instances.len() as u32;
    }

    /// Release the buffers of panes that no longer exist.
    pub fn retain_panes(&mut self, live: &dyn Fn(u64) -> bool) {
        self.panes.retain(|pane, _| live(*pane));
    }
}

/// A single pane's draw, handed to egui to run inside its own render pass.
pub struct TerminalCallback {
    pub pane: u64,
    /// The pane's rect in points, used to size its coordinate system.
    pub rect: egui::Rect,
    pub instances: Vec<Instance>,
    /// Atlas rows rasterised this frame, if any. Only the pane that triggered the
    /// rasterisation carries them.
    pub atlas_upload: Option<(u32, u32, Vec<u8>)>,
}

impl egui_wgpu::CallbackTrait for TerminalCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<TerminalRenderer>() else {
            return Vec::new();
        };

        if let Some((first_row, rows, data)) = &self.atlas_upload {
            renderer.upload_atlas(queue, *first_row, *rows, data);
        }

        let viewport_size = viewport_size_in_pixels(
            self.rect,
            screen_descriptor.pixels_per_point,
            screen_descriptor.size_in_pixels,
        );
        let uniforms = Uniforms {
            viewport_size,
            _padding: [0.0; 2],
        };
        renderer.upload_pane(device, queue, self.pane, &self.instances, uniforms);

        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<TerminalRenderer>() else {
            return;
        };
        let Some(pane) = renderer.panes.get(&self.pane) else {
            return;
        };
        if pane.count == 0 {
            return;
        }

        render_pass.set_pipeline(&renderer.pipeline);
        render_pass.set_bind_group(0, &pane.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, &renderer.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, pane.instances.slice(..));
        render_pass.draw(0..4, 0..pane.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;
    use catshell_term::session::GridSize;

    /// Build a terminal, feed it `input`, and turn the result into quads.
    fn render(input: &str, columns: usize, lines: usize) -> (Vec<Instance>, GlyphAtlas, Palette) {
        let size = GridSize::new(columns, lines);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut parser = Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(&mut term, input.as_bytes());

        let palette = Palette::default();
        let mut atlas = GlyphAtlas::new("monospace", 14.0, 1.0);
        let instances = build_instances(
            term.renderable_content(),
            &palette,
            &mut atlas,
            RenderOptions::default(),
        );
        (instances, atlas, palette)
    }

    fn has_color(instances: &[Instance], rgb: Rgb) -> bool {
        let want = to_rgba(rgb);
        instances.iter().any(|i| i.color == want)
    }

    #[test]
    fn a_pane_is_measured_against_itself_not_the_window() {
        // The bug this guards: using the window size made every pane's contents scale by
        // the ratio between the pane and the window, so a half-width split rendered at
        // half the text size.
        let screen = [1000, 800];
        let full = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0));
        assert_eq!(viewport_size_in_pixels(full, 1.0, screen), [1000.0, 800.0]);

        let half = egui::Rect::from_min_size(egui::pos2(500.0, 0.0), egui::vec2(500.0, 800.0));
        assert_eq!(viewport_size_in_pixels(half, 1.0, screen), [500.0, 800.0]);

        // Scale is applied, and the result is whole pixels.
        let scaled = viewport_size_in_pixels(half, 1.5, [1500, 1200]);
        assert_eq!(scaled, [750.0, 1200.0]);
    }

    #[test]
    fn a_pane_outside_the_screen_is_clamped_not_negative() {
        let screen = [100, 100];
        let outside = egui::Rect::from_min_size(egui::pos2(200.0, 200.0), egui::vec2(50.0, 50.0));
        let size = viewport_size_in_pixels(outside, 1.0, screen);
        assert!(
            size[0] >= 0.0 && size[1] >= 0.0,
            "negative viewport {size:?}"
        );
    }

    #[test]
    fn empty_screen_draws_only_the_cursor() {
        // Default backgrounds are skipped and blank cells have no glyph, so an idle
        // screen costs almost nothing to draw. This is what keeps an idle pane cheap.
        let (instances, _, _) = render("", 80, 24);
        assert!(
            instances.len() <= 2,
            "empty screen emitted {} quads",
            instances.len()
        );
    }

    #[test]
    fn text_produces_one_glyph_per_character() {
        let (plain, _, _) = render("", 80, 24);
        let (with_text, _, _) = render("hello", 80, 24);
        // Five letters, none of which is a space.
        assert_eq!(with_text.len(), plain.len() + 5);
    }

    #[test]
    fn spaces_emit_nothing() {
        let (spaces, _, _) = render("     ", 80, 24);
        let (plain, _, _) = render("", 80, 24);
        assert_eq!(spaces.len(), plain.len(), "blank cells emitted quads");
    }

    #[test]
    fn cells_are_positioned_on_the_grid() {
        let (instances, atlas, _) = render("ab", 80, 24);
        let metrics = atlas.metrics();
        let xs: Vec<f32> = instances
            .iter()
            .filter(|i| i.rect[2] < metrics.width * 2.0)
            .map(|i| i.rect[0])
            .collect();
        // The second glyph sits one cell to the right of the first.
        assert!(
            xs.iter()
                .any(|x| (*x - metrics.width).abs() < metrics.width),
            "no glyph near column 1: {xs:?}"
        );
    }

    #[test]
    fn sgr_colors_reach_the_quads() {
        let palette = Palette::default();
        // Red foreground.
        let (instances, _, _) = render("\x1b[31mred", 80, 24);
        assert!(
            has_color(&instances, palette.ansi[1]),
            "red foreground missing"
        );

        // Blue background emits a background quad, since it differs from the default.
        let (instances, _, _) = render("\x1b[44mblue", 80, 24);
        assert!(
            has_color(&instances, palette.ansi[4]),
            "blue background missing"
        );
    }

    #[test]
    fn truecolor_is_used_verbatim() {
        let (instances, _, _) = render("\x1b[38;2;10;20;30mx", 80, 24);
        assert!(
            has_color(
                &instances,
                Rgb {
                    r: 10,
                    g: 20,
                    b: 30
                }
            ),
            "24-bit colour lost"
        );
    }

    #[test]
    fn default_background_cells_emit_no_background_quad() {
        // One glyph, and no background quad for it.
        let (plain, _, _) = render("", 80, 24);
        let (text, _, _) = render("x", 80, 24);
        assert_eq!(text.len(), plain.len() + 1, "default background was drawn");
    }

    #[test]
    fn inverse_swaps_foreground_and_background() {
        let palette = Palette::default();
        let (instances, _, _) = render("\x1b[7mx", 80, 24);
        // The cell background becomes the default foreground colour.
        assert!(
            has_color(&instances, palette.foreground),
            "inverse did not swap colours"
        );
    }

    #[test]
    fn underline_and_strikeout_add_rules() {
        let (plain, _, _) = render("x", 80, 24);
        let (underlined, _, _) = render("\x1b[4mx", 80, 24);
        let (struck, _, _) = render("\x1b[9mx", 80, 24);
        assert_eq!(underlined.len(), plain.len() + 1, "underline missing");
        assert_eq!(struck.len(), plain.len() + 1, "strikeout missing");
    }

    #[test]
    fn hidden_text_draws_no_glyph() {
        let (plain, _, _) = render("", 80, 24);
        let (hidden, _, _) = render("\x1b[8mhidden", 80, 24);
        assert_eq!(hidden.len(), plain.len(), "hidden text was drawn");
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let (instances, atlas, _) = render("\x1b[41m日", 80, 24);
        let metrics = atlas.metrics();
        // The background quad spans two cells, and the spacer cell adds nothing of its own.
        let wide = instances
            .iter()
            .find(|i| (i.rect[2] - metrics.width * 2.0).abs() < 0.5)
            .expect("no double-width background quad");
        assert!((wide.rect[2] - metrics.width * 2.0).abs() < 0.5);
    }

    #[test]
    fn cursor_is_drawn_at_the_cursor_position() {
        let (instances, atlas, palette) = render("ab", 80, 24);
        let metrics = atlas.metrics();
        // A focused block cursor paints the cell it occupies in the cursor colour.
        let cursor = instances
            .iter()
            .find(|i| i.color == to_rgba(palette.cursor))
            .expect("no cursor quad");
        // It sits in column 2, after the two typed characters.
        assert!(
            (cursor.rect[0] - metrics.width * 2.0).abs() < 0.5,
            "cursor at x={} not column 2",
            cursor.rect[0]
        );
    }

    #[test]
    fn hidden_cursor_is_not_drawn() {
        let palette = Palette::default();
        // DECTCEM off.
        let (instances, _, _) = render("\x1b[?25l", 80, 24);
        assert!(
            !has_color(&instances, palette.cursor),
            "cursor drawn while hidden"
        );
    }

    #[test]
    fn backgrounds_are_emitted_before_foregrounds() {
        // Ordering is what makes alpha blending correct without a depth buffer: a tall
        // glyph must not be painted over by the next row's background.
        let (instances, atlas, _) = render("\x1b[41mab\r\n\x1b[41mcd", 80, 24);
        let metrics = atlas.metrics();
        let last_background = instances
            .iter()
            .rposition(|i| (i.rect[3] - metrics.height).abs() < 0.5)
            .expect("no background quads");
        let first_glyph = instances
            .iter()
            .position(|i| i.rect[3] < metrics.height && i.rect[2] < metrics.width)
            .expect("no glyph quads");
        assert!(
            last_background < first_glyph,
            "background at {last_background} came after glyph at {first_glyph}"
        );
    }

    #[test]
    fn scrollback_rows_land_on_screen_rows() {
        // Fill past the bottom so the grid scrolls, then check every quad is inside the
        // viewport rather than at a negative row.
        let mut input = String::new();
        for i in 0..40 {
            input.push_str(&format!("line{i}\r\n"));
        }
        let (instances, atlas, _) = render(&input, 80, 24);
        let metrics = atlas.metrics();
        let height = metrics.height * 24.0;
        for instance in &instances {
            assert!(
                instance.rect[1] >= -metrics.height,
                "quad above viewport: {instance:?}"
            );
            assert!(
                instance.rect[1] < height + metrics.height,
                "quad below viewport: {instance:?}"
            );
        }
    }

    #[test]
    fn a_full_screen_of_text_is_one_draw_call_worth_of_quads() {
        // The performance claim in concrete terms: a dense screen stays proportional to
        // the cell count, with no per-cell allocation or draw call.
        let columns = 200;
        let lines = 50;
        let input = "x".repeat(columns * lines);
        let (instances, _, _) = render(&input, columns, lines);
        // One glyph per cell, plus the cursor; no background quads at the default colour.
        assert!(
            instances.len() <= columns * lines + 8,
            "{} quads for {} cells",
            instances.len(),
            columns * lines
        );
    }
}
