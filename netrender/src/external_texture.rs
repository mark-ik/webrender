// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Same-device external texture composition.
//!
//! This is the zero-copy lane for producer textures that already live
//! on the renderer's `wgpu::Device` (WebGL canvases, video frames,
//! or embedder-owned render targets). Unlike vello's
//! `register_texture` path, this pass samples the producer texture
//! directly; the source texture does not need `COPY_SRC` usage.

/// How a producer texture's alpha is encoded, which selects the blend the
/// composite uses over the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceAlpha {
    /// Color is **straight** (non-premultiplied): the composite multiplies RGB
    /// by alpha at blend time (`ALPHA_BLENDING`). The default preserves legacy
    /// callers and is appropriate for straight-alpha targets. Default WebGL
    /// contexts are premultiplied and must pass [`Self::Premultiplied`].
    #[default]
    Straight,
    /// Color is **premultiplied** (RGB already carries alpha): the composite
    /// blends with `PREMULTIPLIED_ALPHA_BLENDING` and scales the whole tuple by
    /// `opacity`. Use for accelerated webview output (WebView2 / CEF OSR /
    /// WKWebView), whose composited surfaces are premultiplied.
    Premultiplied,
}

/// Reserve the upper-quarter image-key range for host-owned, same-device
/// producer images. Paint translators must derive an image key through this
/// helper instead of placing the producer's local key directly in a Scene.
/// Ordinary document image keys remain caller-owned and must not use this
/// range.
pub const EXTERNAL_IMAGE_KEY_BASE: u64 = 0xC000_0000_0000_0000;

/// Stable Scene image key for a host producer key.
///
/// Producer keys occupy the lower 62 bits. Rejecting an out-of-range key is
/// deliberate: masking would silently alias two live producers.
pub fn external_image_key(producer_key: u64) -> u64 {
    assert!(
        producer_key < (1_u64 << 62),
        "external producer key {producer_key:#x} exceeds the lower-62-bit namespace"
    );
    EXTERNAL_IMAGE_KEY_BASE | producer_key
}

/// GPU-stage a sampled producer into a Vello-importable texture.
///
/// Vello's `register_texture` requires `COPY_SRC` and straight alpha. This
/// pass samples the source, so it accepts producer views with no `COPY_SRC`
/// usage, unpremultiplies when required, and writes an `Rgba8Unorm`
/// `COPY_SRC` staging texture without CPU readback.
pub(crate) struct ExternalImageStagingPipeline {
    layout: wgpu::BindGroupLayout,
    straight: wgpu::RenderPipeline,
    premultiplied: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
}

impl ExternalImageStagingPipeline {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("netrender external image staging shader"),
            source: wgpu::ShaderSource::Wgsl(EXTERNAL_IMAGE_STAGING_WGSL.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("netrender external image staging layout"),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("netrender external image staging pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let make_pipeline = |entry| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("netrender external image staging pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Self {
            straight: make_pipeline("fs_straight"),
            premultiplied: make_pipeline("fs_premultiplied"),
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("netrender external image staging sampler"),
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            }),
            layout,
        }
    }
}

pub(crate) fn stage_external_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &ExternalImageStagingPipeline,
    source_view: &wgpu::TextureView,
    source_size: [u32; 2],
    alpha: SourceAlpha,
) -> wgpu::Texture {
    let width = source_size[0].max(1);
    let height = source_size[1].max(1);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("netrender external image staging"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("netrender external image staging bind group"),
        layout: &pipeline.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&pipeline.sampler),
            },
        ],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("netrender external image staging encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("netrender external image staging pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(match alpha {
            SourceAlpha::Straight => &pipeline.straight,
            SourceAlpha::Premultiplied => &pipeline.premultiplied,
        });
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit([encoder.finish()]);
    texture
}

const EXTERNAL_IMAGE_STAGING_WGSL: &str = r#"
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
struct Out { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> Out {
  var positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
  var uvs = array<vec2<f32>, 3>(vec2(0.0, 1.0), vec2(2.0, 1.0), vec2(0.0, -1.0));
  return Out(vec4(positions[i], 0.0, 1.0), uvs[i]);
}
@fragment fn fs_straight(in: Out) -> @location(0) vec4<f32> { return textureSample(source, source_sampler, in.uv); }
@fragment fn fs_premultiplied(in: Out) -> @location(0) vec4<f32> { let c = textureSample(source, source_sampler, in.uv); return vec4(select(vec3(0.0), c.rgb / c.a, c.a > 0.0), c.a); }
"#;

#[cfg(test)]
mod tests {
    use super::{EXTERNAL_IMAGE_KEY_BASE, external_image_key};

    #[test]
    fn external_image_key_keeps_producer_keys_in_a_disjoint_namespace() {
        assert_eq!(external_image_key(0), EXTERNAL_IMAGE_KEY_BASE);
        assert_eq!(external_image_key((1_u64 << 62) - 1), u64::MAX,);
    }

    #[test]
    #[should_panic(expected = "lower-62-bit namespace")]
    fn external_image_key_rejects_the_first_aliasing_key() {
        let _ = external_image_key(1_u64 << 62);
    }
}

/// One external texture draw into a target view.
#[derive(Debug, Clone, Copy)]
pub struct ExternalTexturePlacement {
    /// Destination rectangle in target pixel coordinates.
    pub dest_rect: [f32; 4],
    /// Source UV rectangle in normalized texture coordinates.
    pub uv: [f32; 4],
    /// Additional opacity applied while blending over the target.
    pub opacity: f32,
    /// How the source's alpha is encoded (selects the blend). Defaults to
    /// [`SourceAlpha::Straight`] so existing call sites are unchanged.
    pub alpha: SourceAlpha,
}

impl ExternalTexturePlacement {
    pub fn new(dest_rect: [f32; 4]) -> Self {
        Self {
            dest_rect,
            uv: [0.0, 0.0, 1.0, 1.0],
            opacity: 1.0,
            alpha: SourceAlpha::Straight,
        }
    }

    pub fn with_uv(mut self, uv: [f32; 4]) -> Self {
        self.uv = uv;
        self
    }

    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// Set the source alpha convention (default [`SourceAlpha::Straight`]).
    pub fn with_alpha(mut self, alpha: SourceAlpha) -> Self {
        self.alpha = alpha;
        self
    }
}

/// One same-device external texture draw scheduled into a frame.
pub struct ExternalTextureComposite<'a> {
    pub source_view: &'a wgpu::TextureView,
    pub placement: ExternalTexturePlacement,
    /// Number of ordinary [`crate::scene::SceneOp`]s that should paint
    /// before this external texture. `usize::MAX` keeps the legacy
    /// "topmost overlay" behavior for call sites that do not care
    /// about interleaving.
    pub scene_op_boundary: usize,
}

impl<'a> ExternalTextureComposite<'a> {
    pub fn new(source_view: &'a wgpu::TextureView, placement: ExternalTexturePlacement) -> Self {
        Self {
            source_view,
            placement,
            scene_op_boundary: usize::MAX,
        }
    }

    pub fn with_scene_op_boundary(mut self, scene_op_boundary: usize) -> Self {
        self.scene_op_boundary = scene_op_boundary;
        self
    }
}

const EXTERNAL_TEXTURE_WGSL: &str = r#"
struct Params {
    dest: vec4<f32>,
    uv: vec4<f32>,
    viewport_opacity: vec4<f32>,
};

@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
@group(0) @binding(2) var<uniform> params: Params;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    let local = corners[vertex_index];
    let pixel = mix(params.dest.xy, params.dest.zw, local);
    let viewport = params.viewport_opacity.xy;

    var out: VsOut;
    out.position = vec4<f32>(
        (pixel.x / viewport.x) * 2.0 - 1.0,
        1.0 - (pixel.y / viewport.y) * 2.0,
        0.0,
        1.0,
    );
    out.uv = mix(params.uv.xy, params.uv.zw, local);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let color = textureSample(source_texture, source_sampler, in.uv);
    let opacity = clamp(params.viewport_opacity.z, 0.0, 1.0);
    // `.w` selects the source alpha convention: 0 = straight (the blend
    // multiplies RGB by src alpha), 1 = premultiplied (RGB already carries
    // alpha, so scale the whole tuple to apply the extra opacity).
    if (params.viewport_opacity.w > 0.5) {
        return vec4<f32>(color.rgb * opacity, color.a * opacity);
    }
    return vec4<f32>(color.rgb, color.a * opacity);
}
"#;

#[derive(Clone)]
pub(crate) struct ExternalTexturePipeline {
    /// `ALPHA_BLENDING` (straight-alpha source).
    straight: wgpu::RenderPipeline,
    /// `PREMULTIPLIED_ALPHA_BLENDING` (premultiplied source, e.g. webview OSR).
    premultiplied: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

pub(crate) fn build_external_texture_pipeline(
    device: &wgpu::Device,
    target_format: wgpu::TextureFormat,
) -> ExternalTexturePipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("netrender external texture composite"),
        source: wgpu::ShaderSource::Wgsl(EXTERNAL_TEXTURE_WGSL.into()),
    });

    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("netrender external texture layout"),
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
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("netrender external texture pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });

    // Two pipelines that differ only in blend state; the shader branches on the
    // packed mode value to apply opacity correctly for each. Selected per draw
    // by the placement's `SourceAlpha`.
    let make_pipeline = |blend: wgpu::BlendState, label: &str| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(blend),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    let straight = make_pipeline(
        wgpu::BlendState::ALPHA_BLENDING,
        "netrender external texture pipeline (straight)",
    );
    let premultiplied = make_pipeline(
        wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
        "netrender external texture pipeline (premultiplied)",
    );

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("netrender external texture sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });

    ExternalTexturePipeline {
        straight,
        premultiplied,
        layout,
        sampler,
    }
}

fn params_bytes(
    viewport_width: u32,
    viewport_height: u32,
    placement: ExternalTexturePlacement,
) -> [u8; 48] {
    let mode = match placement.alpha {
        SourceAlpha::Straight => 0.0,
        SourceAlpha::Premultiplied => 1.0,
    };
    let values = [
        placement.dest_rect[0],
        placement.dest_rect[1],
        placement.dest_rect[2],
        placement.dest_rect[3],
        placement.uv[0],
        placement.uv[1],
        placement.uv[2],
        placement.uv[3],
        viewport_width as f32,
        viewport_height as f32,
        placement.opacity,
        mode,
    ];
    let mut bytes = [0u8; 48];
    for (index, value) in values.iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Encode one external-texture composite into a caller-owned command encoder.
///
/// The returned flag is false when the placement is empty or fully
/// transparent. Keeping the encoder ownership with the caller lets this
/// operation participate in a graph/executor batch; the legacy convenience
/// wrapper below still submits one private encoder for existing callers.
pub(crate) fn encode_external_texture(
    device: &wgpu::Device,
    pipe: &ExternalTexturePipeline,
    source_view: &wgpu::TextureView,
    target_view: &wgpu::TextureView,
    viewport_width: u32,
    viewport_height: u32,
    placement: ExternalTexturePlacement,
    encoder: &mut wgpu::CommandEncoder,
) -> bool {
    let Some(commands) = external_texture_commands(
        device,
        pipe,
        source_view,
        viewport_width,
        viewport_height,
        placement,
    ) else {
        return false;
    };
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("netrender external texture pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    commands.encode(&mut pass);
    true
}

/// Prepare the commands for one executor-owned external-texture pass.
pub(crate) fn external_texture_commands(
    device: &wgpu::Device,
    pipe: &ExternalTexturePipeline,
    source_view: &wgpu::TextureView,
    viewport_width: u32,
    viewport_height: u32,
    placement: ExternalTexturePlacement,
) -> Option<Box<dyn netrender_device::render_graph::ImageRenderCommands>> {
    if viewport_width == 0
        || viewport_height == 0
        || placement.opacity <= 0.0
        || placement.dest_rect[0] == placement.dest_rect[2]
        || placement.dest_rect[1] == placement.dest_rect[3]
    {
        return None;
    }

    let bytes = params_bytes(viewport_width, viewport_height, placement);
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("netrender external texture params"),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: true,
    });
    {
        let mut view = params.slice(..).get_mapped_range_mut().expect("map range");
        view.copy_from_slice(&bytes);
    }
    params.unmap();

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("netrender external texture bind group"),
        layout: &pipe.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&pipe.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params.as_entire_binding(),
            },
        ],
    });

    let pipeline = match placement.alpha {
        SourceAlpha::Straight => pipe.straight.clone(),
        SourceAlpha::Premultiplied => pipe.premultiplied.clone(),
    };
    Some(netrender_device::render_graph::image_render_commands(
        move |pass| {
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        },
    ))
}

pub(crate) fn compose_external_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipe: &ExternalTexturePipeline,
    source_view: &wgpu::TextureView,
    target_view: &wgpu::TextureView,
    viewport_width: u32,
    viewport_height: u32,
    placement: ExternalTexturePlacement,
) {
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("netrender external texture encoder"),
    });
    encode_external_texture(
        device,
        pipe,
        source_view,
        target_view,
        viewport_width,
        viewport_height,
        placement,
        &mut encoder,
    );
    queue.submit([encoder.finish()]);
}
