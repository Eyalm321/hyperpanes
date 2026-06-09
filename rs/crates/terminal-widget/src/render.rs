//! `PaneRenderer` trait + two implementations.
//!
//! This trait shape is the deliverable the real UI is expected to adopt: a renderer takes
//! a renderer-agnostic `GridSnapshot` plus the shared `Font` and returns a `slint::Image`
//! sized to the pane's *physical* pixel grid (cols*cell_w x rows*cell_h). Slint then
//! composites that Image with border-radius / shadow / z-order for free.
//!
//!  - `SoftwareRenderer`: swash coverage masks blended into a double-buffered
//!    `SharedPixelBuffer` (`Image::from_rgba8`). The RDP / software-GL fallback.
//!  - `GpuRenderer`: a swash→etagere R8 atlas + instanced quads rendered into a per-pane
//!    `wgpu::Texture` (Rgba8Unorm) on Slint's *own* device, imported via
//!    `slint::Image::try_from`.

use crate::font::{CachedGlyph, Font, GlyphKey};
use crate::grid::GridSnapshot;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

pub struct RenderOpts {
    pub cursor_on: bool,
}

pub trait PaneRenderer {
    fn name(&self) -> &'static str;
    /// Render `grid` into a `slint::Image` at physical resolution. Implementations cache
    /// internal buffers/atlases across calls.
    fn render(&mut self, grid: &GridSnapshot, font: &mut Font, opts: &RenderOpts) -> Image;
}

#[inline]
fn lerp_u8(b: u8, f: u8, cov: u8) -> u8 {
    let b = b as i32;
    let f = f as i32;
    (b + (f - b) * cov as i32 / 255) as u8
}

/// Blend a rasterized glyph's coverage mask into `px` in `color`, with the cell's pen at
/// (`x0`,`y0`) and the baseline at `y0 + ascent`. Shared by the normal glyph pass and the
/// cursor's true-invert redraw so the two never drift. Clips to the `w`×`h` buffer.
#[inline]
fn blit_glyph(
    px: &mut [Rgba8Pixel],
    w: u32,
    h: u32,
    stride: usize,
    g: &CachedGlyph,
    x0: u32,
    y0: u32,
    ascent: i32,
    color: [u8; 4],
) {
    if g.w == 0 {
        return;
    }
    let pen_x = x0 as i32 + g.left;
    let gy0 = y0 as i32 + ascent - g.top;
    for gy in 0..g.h as i32 {
        let dy = gy0 + gy;
        if dy < 0 || dy >= h as i32 {
            continue;
        }
        for gx in 0..g.w as i32 {
            let dx = pen_x + gx;
            if dx < 0 || dx >= w as i32 {
                continue;
            }
            let cov = g.mask[(gy * g.w as i32 + gx) as usize];
            if cov == 0 {
                continue;
            }
            let p = &mut px[dy as usize * stride + dx as usize];
            p.r = lerp_u8(p.r, color[0], cov);
            p.g = lerp_u8(p.g, color[1], cov);
            p.b = lerp_u8(p.b, color[2], cov);
            p.a = 255;
        }
    }
}

// =================================================================================
// Software renderer
// =================================================================================

pub struct SoftwareRenderer {
    bufs: Vec<SharedPixelBuffer<Rgba8Pixel>>,
    idx: usize,
    w: u32,
    h: u32,
}

impl SoftwareRenderer {
    pub fn new() -> Self {
        SoftwareRenderer {
            bufs: Vec::new(),
            idx: 0,
            w: 0,
            h: 0,
        }
    }
}

impl PaneRenderer for SoftwareRenderer {
    fn name(&self) -> &'static str {
        "software (swash → SharedPixelBuffer)"
    }

    fn render(&mut self, grid: &GridSnapshot, font: &mut Font, opts: &RenderOpts) -> Image {
        let cw = font.cell_w;
        let ch = font.cell_h;
        let w = (grid.cols as u32 * cw).max(1);
        let h = (grid.rows as u32 * ch).max(1);
        if w != self.w || h != self.h || self.bufs.is_empty() {
            self.w = w;
            self.h = h;
            self.bufs = vec![
                SharedPixelBuffer::new(w, h),
                SharedPixelBuffer::new(w, h),
            ];
        }
        // Double-buffer: write into the buffer Slint is not currently displaying.
        self.idx ^= 1;
        let buf = &mut self.bufs[self.idx];
        let px = buf.make_mut_slice();
        let stride = w as usize;
        let bg0 = grid.default_bg;
        for p in px.iter_mut() {
            *p = Rgba8Pixel {
                r: bg0[0],
                g: bg0[1],
                b: bg0[2],
                a: 255,
            };
        }

        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let cell = grid.cell(col, row);
                if cell.wide_spacer {
                    continue;
                }
                let x0 = col as u32 * cw;
                let y0 = row as u32 * ch;
                let cell_px_w = if cell.wide { cw * 2 } else { cw };

                // Background fill.
                if cell.bg[3] > 0 {
                    for yy in 0..ch {
                        let row_off = ((y0 + yy) as usize) * stride + x0 as usize;
                        for xx in 0..cell_px_w {
                            if (x0 + xx) >= w {
                                break;
                            }
                            let p = &mut px[row_off + xx as usize];
                            p.r = cell.bg[0];
                            p.g = cell.bg[1];
                            p.b = cell.bg[2];
                            p.a = 255;
                        }
                    }
                }

                // Glyph.
                if cell.ch != ' ' && cell.ch != '\0' {
                    let (font_id, gid) = font.resolve(cell.ch);
                    let ascent = font.ascent;
                    let g = font
                        .rasterize(GlyphKey {
                            font_id,
                            gid,
                            bold: cell.bold,
                            italic: cell.italic,
                        })
                        .clone();
                    blit_glyph(px, w, h, stride, &g, x0, y0, ascent, cell.fg);
                }

                // Underline. Compute in i32 (a negative ascent must not wrap to a huge u32)
                // and clamp WITHIN this cell's band — never to the bottom screen row.
                if cell.underline {
                    let uy = (y0 as i32 + font.ascent + 2)
                        .clamp(y0 as i32, (y0 + ch) as i32 - 1) as u32;
                    let row_off = uy as usize * stride + x0 as usize;
                    for xx in 0..cell_px_w {
                        if (x0 + xx) >= w {
                            break;
                        }
                        let p = &mut px[row_off + xx as usize];
                        p.r = cell.fg[0];
                        p.g = cell.fg[1];
                        p.b = cell.fg[2];
                    }
                }
            }
        }

        // Cursor (block) — TRUE invert: paint the block in the cell's fg, then redraw the
        // glyph on top in the colour it sits on (the cell bg, or the grid default bg when the
        // cell is transparent) so the character under the cursor stays visible instead of being
        // erased by a solid fg block.
        if grid.cursor_visible && opts.cursor_on {
            let (col, row) = grid.cursor;
            let ccol = col.min(grid.cols - 1);
            let crow = row.min(grid.rows - 1);
            let x0 = ccol as u32 * cw;
            let y0 = crow as u32 * ch;
            let cell = *grid.cell(ccol, crow);
            let block_w = if cell.wide { cw * 2 } else { cw };
            // The colour the inverted glyph is drawn in: the cell's own bg if opaque, else the
            // grid default bg — i.e. whatever the block colour "replaced", so it reads as invert.
            let under = if cell.bg[3] > 0 { cell.bg } else { grid.default_bg };

            // 1) Fill the cursor block with the cell's fg colour.
            for yy in 0..ch {
                if y0 + yy >= h {
                    break;
                }
                let row_off = (y0 + yy) as usize * stride + x0 as usize;
                for xx in 0..block_w {
                    if x0 + xx >= w {
                        break;
                    }
                    let p = &mut px[row_off + xx as usize];
                    p.r = cell.fg[0];
                    p.g = cell.fg[1];
                    p.b = cell.fg[2];
                    p.a = 255;
                }
            }

            // 2) Redraw the glyph over the block in `under` so it inverts cleanly.
            if cell.ch != ' ' && cell.ch != '\0' {
                let (font_id, gid) = font.resolve(cell.ch);
                let ascent = font.ascent;
                let g = font
                    .rasterize(GlyphKey {
                        font_id,
                        gid,
                        bold: cell.bold,
                        italic: cell.italic,
                    })
                    .clone();
                blit_glyph(px, w, h, stride, &g, x0, y0, ascent, under);
            }
        }

        Image::from_rgba8(buf.clone())
    }
}

// =================================================================================
// GPU renderer
// =================================================================================

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BgInstance {
    rect: [f32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GlyphInstance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
}

const ATLAS: u32 = 2048;

struct AtlasEntry {
    uv: [f32; 4], // normalized x,y,w,h
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,

    atlas_tex: wgpu::Texture,
    atlas_alloc: etagere::AtlasAllocator,
    atlas_map: std::collections::HashMap<GlyphKey, AtlasEntry>,

    uniform_buf: wgpu::Buffer,
    bg_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    bg_bind: wgpu::BindGroup,
    glyph_bind: wgpu::BindGroup,

    bg_buf: wgpu::Buffer,
    bg_cap: u64,
    glyph_buf: wgpu::Buffer,
    glyph_cap: u64,

    target: Option<wgpu::Texture>,
    tw: u32,
    th: u32,
}

impl GpuRenderer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS,
                height: ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // bind layout 0: uniform only (bg)
        let bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bg-bgl"),
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
        // bind layout 1: uniform + atlas + sampler (glyph)
        let glyph_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });

        let bg_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg-bind"),
            layout: &bg_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        let glyph_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph-bind"),
            layout: &glyph_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("term-shaders"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        let color_target = wgpu::ColorTargetState {
            format: wgpu::TextureFormat::Rgba8Unorm,
            blend: Some(blend),
            write_mask: wgpu::ColorWrites::ALL,
        };

        let bg_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bg-pl"),
            bind_group_layouts: &[Some(&bg_layout)],
            immediate_size: 0,
        });
        let glyph_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph-pl"),
            bind_group_layouts: &[Some(&glyph_layout)],
            immediate_size: 0,
        });

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg-pipe"),
            layout: Some(&bg_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_bg"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<BgInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_bg"),
                targets: &[Some(color_target.clone())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph-pipe"),
            layout: Some(&glyph_pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_glyph"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GlyphInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_glyph"),
                targets: &[Some(color_target)],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let bg_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bg-instances"),
            size: 4096 * std::mem::size_of::<BgInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyph-instances"),
            size: 8192 * std::mem::size_of::<GlyphInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        GpuRenderer {
            device,
            queue,
            atlas_tex,
            atlas_alloc: etagere::AtlasAllocator::new(etagere::size2(ATLAS as i32, ATLAS as i32)),
            atlas_map: std::collections::HashMap::new(),
            uniform_buf,
            bg_pipeline,
            glyph_pipeline,
            bg_bind,
            glyph_bind,
            bg_buf,
            bg_cap: 4096,
            glyph_buf,
            glyph_cap: 8192,
            target: None,
            tw: 0,
            th: 0,
        }
    }

    fn ensure_glyph(&mut self, font: &mut Font, key: GlyphKey) -> bool {
        if self.atlas_map.contains_key(&key) {
            return true;
        }
        let g = font.rasterize(key).clone();
        if g.w == 0 || g.h == 0 {
            self.atlas_map.insert(
                key,
                AtlasEntry {
                    uv: [0.0; 4],
                    w: 0,
                    h: 0,
                    left: 0,
                    top: 0,
                },
            );
            return true;
        }
        let pad = 1;
        let alloc = match self
            .atlas_alloc
            .allocate(etagere::size2((g.w + pad) as i32, (g.h + pad) as i32))
        {
            Some(a) => a,
            None => return false, // atlas full — spike limitation
        };
        let x = alloc.rectangle.min.x as u32;
        let y = alloc.rectangle.min.y as u32;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &g.mask,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(g.w),
                rows_per_image: Some(g.h),
            },
            wgpu::Extent3d {
                width: g.w,
                height: g.h,
                depth_or_array_layers: 1,
            },
        );
        self.atlas_map.insert(
            key,
            AtlasEntry {
                uv: [
                    x as f32 / ATLAS as f32,
                    y as f32 / ATLAS as f32,
                    g.w as f32 / ATLAS as f32,
                    g.h as f32 / ATLAS as f32,
                ],
                w: g.w,
                h: g.h,
                left: g.left,
                top: g.top,
            },
        );
        true
    }
}

impl GpuRenderer {
    /// Do the GPU work (build instances, upload, draw into the per-pane target, submit).
    /// Separated from the Slint import so the benchmark can time pure render throughput.
    pub fn render_to_texture(&mut self, grid: &GridSnapshot, font: &mut Font, opts: &RenderOpts) {
        let cw = font.cell_w;
        let ch = font.cell_h;
        let w = (grid.cols as u32 * cw).max(1);
        let h = (grid.rows as u32 * ch).max(1);

        if self.target.is_none() || w != self.tw || h != self.th {
            self.tw = w;
            self.th = h;
            self.target = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pane-target"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            }));
        }

        // Build instance arrays.
        let mut bgs: Vec<BgInstance> = Vec::with_capacity(grid.cols * grid.rows / 4);
        let mut glyphs: Vec<GlyphInstance> = Vec::with_capacity(grid.cols * grid.rows);
        let to_f = |c: [u8; 4]| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
                c[3] as f32 / 255.0,
            ]
        };

        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let cell = grid.cell(col, row);
                if cell.wide_spacer {
                    continue;
                }
                let x0 = (col as u32 * cw) as f32;
                let y0 = (row as u32 * ch) as f32;
                let cell_w = if cell.wide { (cw * 2) as f32 } else { cw as f32 };

                if cell.bg[3] > 0 {
                    bgs.push(BgInstance {
                        rect: [x0, y0, cell_w, ch as f32],
                        color: to_f(cell.bg),
                    });
                }

                if cell.ch != ' ' && cell.ch != '\0' {
                    let (font_id, gid) = font.resolve(cell.ch);
                    let key = GlyphKey {
                        font_id,
                        gid,
                        bold: cell.bold,
                        italic: cell.italic,
                    };
                    self.ensure_glyph(font, key);
                    if let Some(e) = self.atlas_map.get(&key) {
                        if e.w > 0 {
                            let gx = x0 + e.left as f32;
                            let gy = y0 + font.ascent as f32 - e.top as f32;
                            glyphs.push(GlyphInstance {
                                rect: [gx, gy, e.w as f32, e.h as f32],
                                uv: e.uv,
                                color: to_f(cell.fg),
                            });
                        }
                    }
                }

                if cell.underline {
                    // Clamp the underline within this cell's band (ascent is i32 → f32, so no
                    // u32 wrap), keeping it off the next row / bottom of the target.
                    let uy = (y0 + font.ascent as f32 + 2.0).clamp(y0, y0 + ch as f32 - 1.0);
                    bgs.push(BgInstance {
                        rect: [x0, uy, cell_w, 1.0],
                        color: to_f(cell.fg),
                    });
                }
            }
        }

        // Cursor as an inverted block quad + re-draw nothing (good enough for spike).
        if grid.cursor_visible && opts.cursor_on {
            let (col, row) = grid.cursor;
            let cell = grid.cell(col.min(grid.cols - 1), row.min(grid.rows - 1));
            bgs.push(BgInstance {
                rect: [
                    (col as u32 * cw) as f32,
                    (row as u32 * ch) as f32,
                    cw as f32,
                    ch as f32,
                ],
                color: to_f(cell.fg),
            });
        }

        // Upload uniforms + instances (grow buffers if needed).
        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen: [w as f32, h as f32],
                _pad: [0.0; 2],
            }),
        );
        if bgs.len() as u64 > self.bg_cap {
            self.bg_cap = (bgs.len() as u64).next_power_of_two();
            self.bg_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("bg-instances"),
                size: self.bg_cap * std::mem::size_of::<BgInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if glyphs.len() as u64 > self.glyph_cap {
            self.glyph_cap = (glyphs.len() as u64).next_power_of_two();
            self.glyph_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glyph-instances"),
                size: self.glyph_cap * std::mem::size_of::<GlyphInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !bgs.is_empty() {
            self.queue
                .write_buffer(&self.bg_buf, 0, bytemuck::cast_slice(&bgs));
        }
        if !glyphs.is_empty() {
            self.queue
                .write_buffer(&self.glyph_buf, 0, bytemuck::cast_slice(&glyphs));
        }

        let target = self.target.as_ref().unwrap();
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let bg0 = grid.default_bg;
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("pane-encoder"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pane-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg0[0] as f64 / 255.0,
                            g: bg0[1] as f64 / 255.0,
                            b: bg0[2] as f64 / 255.0,
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
            if !bgs.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_bind_group(0, &self.bg_bind, &[]);
                pass.set_vertex_buffer(0, self.bg_buf.slice(..));
                pass.draw(0..6, 0..bgs.len() as u32);
            }
            if !glyphs.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.glyph_bind, &[]);
                pass.set_vertex_buffer(0, self.glyph_buf.slice(..));
                pass.draw(0..6, 0..glyphs.len() as u32);
            }
        }
        self.queue.submit(Some(enc.finish()));
    }

    /// Block until the most recent submission completes (benchmark timing only).
    pub fn wait_idle(&self) {
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }
}

impl PaneRenderer for GpuRenderer {
    fn name(&self) -> &'static str {
        "gpu (swash atlas → wgpu texture → slint Image)"
    }

    fn render(&mut self, grid: &GridSnapshot, font: &mut Font, opts: &RenderOpts) -> Image {
        self.render_to_texture(grid, font, opts);
        // Import the freshly-rendered texture as a Slint Image (shared device → zero copy).
        let target = self.target.as_ref().unwrap();
        Image::try_from(target.clone()).expect("wgpu texture import into slint failed")
    }
}

const SHADER: &str = r#"
struct U { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: U;

fn quad_corner(vi: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return c[vi];
}
fn to_ndc(px: vec2<f32>) -> vec4<f32> {
    let ndc = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
    return vec4<f32>(ndc, 0.0, 1.0);
}

struct BgOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs_bg(@builtin(vertex_index) vi: u32,
                 @location(0) rect: vec4<f32>,
                 @location(1) color: vec4<f32>) -> BgOut {
    let c = quad_corner(vi);
    let px = rect.xy + c * rect.zw;
    var o: BgOut;
    o.pos = to_ndc(px);
    o.color = color;
    return o;
}
@fragment fn fs_bg(i: BgOut) -> @location(0) vec4<f32> { return i.color; }

struct GlyphOut { @builtin(position) pos: vec4<f32>,
                  @location(0) uv: vec2<f32>,
                  @location(1) color: vec4<f32> };
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@vertex fn vs_glyph(@builtin(vertex_index) vi: u32,
                    @location(0) rect: vec4<f32>,
                    @location(1) uvr: vec4<f32>,
                    @location(2) color: vec4<f32>) -> GlyphOut {
    let c = quad_corner(vi);
    let px = rect.xy + c * rect.zw;
    var o: GlyphOut;
    o.pos = to_ndc(px);
    o.uv = uvr.xy + c * uvr.zw;
    o.color = color;
    return o;
}
@fragment fn fs_glyph(i: GlyphOut) -> @location(0) vec4<f32> {
    let cov = textureSample(atlas, samp, i.uv).r;
    return vec4<f32>(i.color.rgb, i.color.a * cov);
}
"#;
