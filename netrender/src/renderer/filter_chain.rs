// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Element CSS `filter` pass: per-layer content render, the blur/color-matrix
//! filter chain, and the single-pass color-matrix helper. See [`super`].

use std::sync::Arc;

use crate::scene::{ImageKey, Scene};
use crate::tile_cache::TileCache;

use super::filters::{
    blur_kernel_plan_with_downscale, build_layer_content_scene, matching_pop,
    scene_filter_to_matrix, ELEMENT_FILTER_KEY_BASE,
};
use super::Renderer;

impl Renderer {
    /// Roadmap D1 — blur an arbitrary input texture using the
    /// existing render-graph cascade machinery (and R5's downscale
    /// path for large radii). Returns a fresh `Rgba8Unorm` texture
    /// of size `dim × dim`.
    pub(super) fn build_blurred_image(
        &self,
        input: wgpu::Texture,
        dim: u32,
        blur_radius_px: f32,
    ) -> wgpu::Texture {
        use crate::filter::{blur_pass_callback, make_bilinear_sampler};
        use netrender_device::render_graph::{
            ImageAccess, ImageLoad, ImageUse, RenderGraph, TransientImageDesc,
        };
        use std::collections::HashMap;

        let device = self.wgpu_device.core.device.clone();
        let queue = self.wgpu_device.core.queue.clone();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let blur_pipe = self.wgpu_device.ensure_brush_blur(format);
        let sampler = make_bilinear_sampler(&device);

        let (level, passes, step_px) = blur_kernel_plan_with_downscale(blur_radius_px);
        let scaled_dim = (dim / level).max(1);
        let scaled_extent = wgpu::Extent3d {
            width: scaled_dim,
            height: scaled_dim,
            depth_or_array_layers: 1,
        };
        let full_extent = wgpu::Extent3d {
            width: dim,
            height: dim,
            depth_or_array_layers: 1,
        };
        let step_uv = step_px / scaled_dim as f32;

        let mut graph = RenderGraph::new();
        let input_node = graph.import_image("blur input", full_extent, format);
        let mut prev = input_node;

        if level > 1 {
            let down = graph.transient_image(TransientImageDesc {
                size: scaled_extent,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                label: Some("blur downsample".into()),
            });
            graph
                .add_plan_task(
                    "blur downsample",
                    vec![ImageUse {
                        image: prev,
                        access: ImageAccess::SampledRead,
                    }],
                    ImageUse::color_attachment(down, ImageLoad::Clear),
                    blur_pass_callback(blur_pipe.clone(), Arc::clone(&sampler), 0.0, 0.0),
                )
                .expect("valid blur downsample task");
            prev = down;
        }

        for pass_index in 0..passes {
            let horizontal = graph.transient_image(TransientImageDesc {
                size: scaled_extent,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                label: Some(format!("blur horizontal {pass_index}")),
            });
            graph
                .add_plan_task(
                    format!("blur horizontal {pass_index}"),
                    vec![ImageUse {
                        image: prev,
                        access: ImageAccess::SampledRead,
                    }],
                    ImageUse::color_attachment(horizontal, ImageLoad::Clear),
                    blur_pass_callback(blur_pipe.clone(), Arc::clone(&sampler), step_uv, 0.0),
                )
                .expect("valid blur horizontal task");
            let vertical = graph.transient_image(TransientImageDesc {
                size: scaled_extent,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                label: Some(format!("blur vertical {pass_index}")),
            });
            graph
                .add_plan_task(
                    format!("blur vertical {pass_index}"),
                    vec![ImageUse {
                        image: horizontal,
                        access: ImageAccess::SampledRead,
                    }],
                    ImageUse::color_attachment(vertical, ImageLoad::Clear),
                    blur_pass_callback(blur_pipe.clone(), Arc::clone(&sampler), 0.0, step_uv),
                )
                .expect("valid blur vertical task");
            prev = vertical;
        }

        if level > 1 {
            let up = graph.transient_image(TransientImageDesc {
                size: full_extent,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                label: Some("blur upsample".into()),
            });
            graph
                .add_plan_task(
                    "blur upsample",
                    vec![ImageUse {
                        image: prev,
                        access: ImageAccess::SampledRead,
                    }],
                    ImageUse::color_attachment(up, ImageLoad::Clear),
                    blur_pass_callback(blur_pipe, Arc::clone(&sampler), 0.0, 0.0),
                )
                .expect("valid blur upsample task");
            prev = up;
        }

        let mut imported = HashMap::new();
        imported.insert(input_node, input);
        let plan = graph.compile(&[prev]).expect("filter blur plan is valid");
        let (mut outputs, _report) = plan
            .execute(&device, &queue, imported)
            .expect("filter blur graph is valid");
        outputs.remove(&prev).expect("D1 final blur output")
    }

    /// Apply one CSS color-matrix filter to `input` (a premultiplied
    /// `Rgba8Unorm` texture), returning a fresh `dim x dim` texture via a single
    /// `cs_color_matrix` pass. `dim` is the square work size (matches
    /// `build_blurred_image`; a non-square viewport is a shared limitation).
    fn build_color_matrix_image(
        &self,
        input: wgpu::Texture,
        dim: u32,
        matrix: [f32; 20],
    ) -> wgpu::Texture {
        use crate::filter::{color_matrix_callback, make_bilinear_sampler};
        use netrender_device::render_graph::{ImageLoad, ImageUse, RenderGraph, TransientImageDesc};
        use std::collections::HashMap;

        let device = self.wgpu_device.core.device.clone();
        let queue = self.wgpu_device.core.queue.clone();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let pipe = self.wgpu_device.ensure_color_matrix(format);
        let sampler = make_bilinear_sampler(&device);
        let extent = wgpu::Extent3d {
            width: dim,
            height: dim,
            depth_or_array_layers: 1,
        };

        let mut graph = RenderGraph::new();
        let input_node = graph.import_image("color matrix input", extent, format);
        let output_node = graph.transient_image(TransientImageDesc {
            size: extent,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            label: Some("color matrix output".into()),
        });
        graph
            .add_plan_task(
                "color matrix",
                vec![ImageUse::sampled_read(input_node)],
                ImageUse::color_attachment(output_node, ImageLoad::Clear),
                color_matrix_callback(pipe, sampler, matrix),
            )
            .expect("valid color-matrix task");
        let mut externals = HashMap::new();
        externals.insert(input_node, input);
        let plan = graph
            .compile(&[output_node])
            .expect("color-matrix plan is valid");
        let (mut outputs, _report) = plan
            .execute(&device, &queue, externals)
            .expect("color-matrix graph is valid");
        outputs.remove(&output_node).expect("color_matrix output")
    }

    /// Apply a CSS `filter` chain to a rendered layer-content texture, in order:
    /// `Blur` via the separable blur cascade, the color functions via
    /// `cs_color_matrix`. Each pass feeds the next; returns the final texture.
    fn apply_filter_chain(
        &self,
        input: wgpu::Texture,
        dim: u32,
        filters: &[crate::scene::SceneFilter],
    ) -> wgpu::Texture {
        use crate::scene::SceneFilter;
        let mut tex = input;
        for f in filters {
            tex = match f {
                SceneFilter::Blur(r) => self.build_blurred_image(tex, dim, *r),
                other => self.build_color_matrix_image(tex, dim, scene_filter_to_matrix(*other)),
            };
        }
        tex
    }

    /// Element CSS `filter`: for every outermost layer carrying a non-empty
    /// `filters` chain, render that layer's own content to a texture, apply the
    /// chain, and replace the content ops with one image so the surviving
    /// `PushLayer`/`PopLayer` composite the filtered result with the layer's
    /// alpha/blend/clip. Mirrors [`Self::preprocess_backdrop_filters`]. Nested
    /// element filters are deferred (outermost-only), and the image-key region is
    /// disjoint from the backdrop pass's.
    pub(super) fn preprocess_element_filters(
        &self,
        scene: &Scene,
        rast: &mut crate::vello_tile_rasterizer::VelloTileRasterizer,
        tc: &mut TileCache,
    ) -> Scene {
        use crate::scene::{SceneClip, SceneFilter, SceneImage, SceneOp, NO_CLIP, SHARP_CLIP};

        let mut processed = scene.clone();

        // Collect (push_idx, pop_idx, filters, bounds) for outermost filtered
        // layers in painter order; skip any layer nested inside an
        // already-collected one (nested element filters are a follow-up).
        let mut covered_until = 0usize;
        let mut elements: Vec<(usize, usize, Vec<SceneFilter>, [f32; 4])> = Vec::new();
        for (i, op) in scene.ops.iter().enumerate() {
            if let SceneOp::PushLayer(l) = op {
                if l.filters.is_empty() || i < covered_until {
                    continue;
                }
                let Some(pop_idx) = matching_pop(&scene.ops, i) else {
                    continue;
                };
                let bounds = match &l.clip {
                    SceneClip::None => [
                        0.0,
                        0.0,
                        scene.viewport_width as f32,
                        scene.viewport_height as f32,
                    ],
                    SceneClip::Rect { rect, .. } => *rect,
                    SceneClip::Path(path) => path.local_aabb().unwrap_or([
                        0.0,
                        0.0,
                        scene.viewport_width as f32,
                        scene.viewport_height as f32,
                    ]),
                };
                elements.push((i, pop_idx, l.filters.clone(), bounds));
                covered_until = pop_idx;
            }
        }

        // Image-key region disjoint from the backdrop pass; both count down from
        // their bases (see ELEMENT_FILTER_KEY_BASE).
        let mut next_key: ImageKey = ELEMENT_FILTER_KEY_BASE;

        // Replacing an interior of `inner_len` ops with one image shifts later
        // indices by `1 - inner_len`. Track the running (signed) offset.
        let mut offset: isize = 0;

        for (push_idx, pop_idx, filters, bounds) in elements {
            // Render the layer's own content (flat) to a viewport texture, then
            // run the filter chain over it.
            let content = build_layer_content_scene(scene, push_idx, pop_idx);
            let content_tex = self.render_scene_to_texture(rast, tc, &content);
            let filtered = self.apply_filter_chain(content_tex, scene.viewport_width, &filters);
            rast.register_texture(next_key, filtered);

            let vw = scene.viewport_width as f32;
            let vh = scene.viewport_height as f32;
            let uv = [
                bounds[0] / vw,
                bounds[1] / vh,
                bounds[2] / vw,
                bounds[3] / vh,
            ];

            let cur_push = (push_idx as isize + offset) as usize;
            let cur_pop = (pop_idx as isize + offset) as usize;
            // Keep a backdrop-filter image (the layer's first content op, marked
            // by a sentinel key above the element region) behind the filtered
            // result, so `backdrop-filter` + `filter` on one layer composites the
            // unfiltered backdrop under the element's filtered content.
            let content_start = match processed.ops.get(cur_push + 1) {
                Some(SceneOp::Image(im)) if im.key > ELEMENT_FILTER_KEY_BASE => cur_push + 2,
                _ => cur_push + 1,
            };
            let inner_len = cur_pop - content_start;

            let image = SceneOp::Image(SceneImage {
                x0: bounds[0],
                y0: bounds[1],
                x1: bounds[2],
                y1: bounds[3],
                uv,
                color: [1.0, 1.0, 1.0, 1.0],
                key: next_key,
                transform_id: 0,
                clip_rect: NO_CLIP,
                clip_corner_radii: SHARP_CLIP,
                // `false` like the backdrop path: the filtered result is a
                // GPU-texture override (no CPU `ImageData`), so the `clamp_to_uv`
                // CPU crop path can't run on it.
                clamp_to_uv: false,
                nearest: false,
            });
            // Replace the (non-backdrop) interior with the single filtered image.
            processed
                .ops
                .splice(content_start..cur_pop, std::iter::once(image));
            // Clear the layer's filters so it re-enters the no-filter fast path,
            // applying alpha/blend/clip over the filtered image.
            if let SceneOp::PushLayer(l) = &mut processed.ops[cur_push] {
                l.filters.clear();
            }

            offset += 1 - inner_len as isize;
            next_key = next_key.wrapping_sub(1);
        }

        processed
    }
}
