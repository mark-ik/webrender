// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Phase 11c' — render-graph encode callbacks for the WGSL filter
//! pipelines (`cs_clip_rectangle` mask + separable `brush_blur`)
//! that produce intermediate textures consumed by vello scenes via
//! `Renderer::insert_image_vello`.
//!
//! These are crate-internal helpers used by the renderer's image execution
//! plans. The high-level `Renderer::build_box_shadow_mask` orchestration that
//! consumes them lives in `renderer/mod.rs`.

use std::sync::Arc;

use netrender_device::{BrushBlurPipeline, ClipRectanglePipeline, ColorMatrixPipeline};

use netrender_device::render_graph::{PrepareCallback, image_render_commands};

/// Bilinear-clamp sampler. `brush_blur` and other filter passes use
/// this to sample their input textures.
pub(crate) fn make_bilinear_sampler(device: &wgpu::Device) -> Arc<wgpu::Sampler> {
    Arc::new(device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("netrender bilinear clamp"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    }))
}

/// Build a render-graph encode callback that runs `cs_clip_rectangle`
/// to produce a rounded-rect coverage mask. `bounds` are in
/// target-pixel space; `corner_radius` is uniform on all four
/// corners (the underlying WGSL accepts per-corner radii but the
/// `vec4` slot is filled with `radius` in this helper for parity
/// with the original test usage).
pub(crate) fn clip_rectangle_callback(
    pipe: ClipRectanglePipeline,
    bounds: [f32; 4],
    corner_radius: f32,
) -> PrepareCallback {
    // ClipParams: bounds (vec4) + radii (vec4) = 32 bytes.
    let mut bytes = [0u8; 32];
    for (i, f) in bounds.iter().enumerate() {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&f.to_ne_bytes());
    }
    for i in 4..8 {
        bytes[i * 4..(i + 1) * 4].copy_from_slice(&corner_radius.to_ne_bytes());
    }

    Box::new(move |device, _inputs| {
        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clip_rectangle params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        {
            let mut view = params_buf
                .slice(..)
                .get_mapped_range_mut()
                .expect("map range");
            view.copy_from_slice(&bytes);
        }
        params_buf.unmap();

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("clip_rectangle bind group"),
            layout: &pipe.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buf.as_entire_binding(),
            }],
        });

        image_render_commands(move |pass| {
            pass.set_pipeline(&pipe.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        })
    })
}

/// Build a render-graph encode callback for one `brush_blur` pass.
/// `step_x` / `step_y` are the texel-space sample offsets:
/// `(1/W, 0)` for horizontal, `(0, 1/H)` for vertical. Run two
/// callbacks back-to-back (H then V) for a separable 2-D blur.
pub(crate) fn blur_pass_callback(
    pipe: BrushBlurPipeline,
    sampler: Arc<wgpu::Sampler>,
    step_x: f32,
    step_y: f32,
) -> PrepareCallback {
    let mut step_bytes = [0u8; 16];
    step_bytes[0..4].copy_from_slice(&step_x.to_ne_bytes());
    step_bytes[4..8].copy_from_slice(&step_y.to_ne_bytes());

    Box::new(move |device, inputs| {
        assert!(!inputs.is_empty(), "blur task: expected one input view");
        let input_view = &inputs[0];

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blur params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        {
            let mut view = params_buf
                .slice(..)
                .get_mapped_range_mut()
                .expect("map range");
            view.copy_from_slice(&step_bytes);
        }
        params_buf.unmap();

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blur bind group"),
            layout: &pipe.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });

        image_render_commands(move |pass| {
            pass.set_pipeline(&pipe.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        })
    })
}

/// Build a render-graph encode callback that applies a 4x5 color matrix to its
/// single input texture (CSS `filter` color functions). `matrix` is row-major
/// `[f32; 20]`: indices `row*5 + 0..4` are the `[r,g,b,a]` coefficients for that
/// output row, index `row*5 + 4` the bias. Packed to match the WGSL
/// `ColorMatrixParams { row0..row3: vec4, bias: vec4 }` (80 bytes). Mirrors
/// [`blur_pass_callback`].
pub(crate) fn color_matrix_callback(
    pipe: ColorMatrixPipeline,
    sampler: Arc<wgpu::Sampler>,
    matrix: [f32; 20],
) -> PrepareCallback {
    // 4 rows of vec4 (the r,g,b,a coefficients) + a bias vec4 (column 4).
    let mut bytes = [0u8; 80];
    for row in 0..4 {
        for col in 0..4 {
            let off = row * 16 + col * 4;
            bytes[off..off + 4].copy_from_slice(&matrix[row * 5 + col].to_ne_bytes());
        }
        let bias_off = 64 + row * 4;
        bytes[bias_off..bias_off + 4].copy_from_slice(&matrix[row * 5 + 4].to_ne_bytes());
    }

    Box::new(move |device, inputs| {
        assert!(
            !inputs.is_empty(),
            "color_matrix task: expected one input view"
        );
        let input_view = &inputs[0];

        let params_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("color_matrix params"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: true,
        });
        {
            let mut view = params_buf
                .slice(..)
                .get_mapped_range_mut()
                .expect("map range");
            view.copy_from_slice(&bytes);
        }
        params_buf.unmap();

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("color_matrix bind group"),
            layout: &pipe.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(input_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buf.as_entire_binding(),
                },
            ],
        });

        image_render_commands(move |pass| {
            pass.set_pipeline(&pipe.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..4, 0..1);
        })
    })
}
