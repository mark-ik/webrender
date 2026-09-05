// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! RG2c's first renderer-owned combined-effect decomposition.
//!
//! This is intentionally narrower than the existing Classic preprocessor. It
//! admits one outermost, trailing, normal source-over layer with an identity
//! transform and a sharp rectangular clip. That bounded shape is enough to
//! prove a real backend-neutral fork/join without implying that arbitrary CSS
//! effect DAGs have been lowered.

use std::sync::Arc;

use crate::filter::{blur_pass_callback, color_matrix_callback, make_bilinear_sampler};
use netrender_device::render_graph::{
    ExecutionPlan, ImageLoad, ImageNode, ImageUse, PrepareCallback, RenderGraph,
    TransientImageDesc, image_render_commands,
};
use crate::scene::{Scene, SceneBlendMode, SceneClip, SceneCompose, SceneFilter, SceneOp};

use super::filters::{
    blur_kernel_plan_with_downscale, build_layer_content_scene, build_prefix_scene, matching_pop,
    scene_filter_to_matrix,
};
use super::{RasterExecution, Renderer};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CombinedEffectDecompositionError {
    NoCombinedEffectLayer,
    MultipleCombinedEffectLayers,
    NestedCombinedEffectLayer,
    UnbalancedLayer,
    LayerMustBeTrailing,
    NonSquareViewport { width: u32, height: u32 },
    UnsupportedRootComposition,
    UnsupportedLayerComposition,
    UnsupportedLayerTransform { transform_id: u32 },
    UnsupportedClip,
    UnsupportedBackdropFilter,
}

#[derive(Clone, Debug)]
pub(crate) struct CombinedEffectDecomposition {
    pub(crate) prefix: Scene,
    pub(crate) content: Scene,
    pub(crate) bounds: [f32; 4],
    pub(crate) alpha: f32,
    pub(crate) backdrop_radius: f32,
    pub(crate) element_filters: Vec<SceneFilter>,
}

pub(crate) struct CombinedEffectPlan {
    pub(crate) plan: ExecutionPlan,
    pub(crate) prefix_input: ImageNode,
    pub(crate) content_input: ImageNode,
    pub(crate) output: ImageNode,
}

pub(crate) fn decompose_combined_effect(
    scene: &Scene,
) -> Result<CombinedEffectDecomposition, CombinedEffectDecompositionError> {
    if scene.viewport_width != scene.viewport_height {
        return Err(CombinedEffectDecompositionError::NonSquareViewport {
            width: scene.viewport_width,
            height: scene.viewport_height,
        });
    }
    if scene.root_alpha != 1.0 || scene.root_blend_mode != SceneBlendMode::Normal {
        return Err(CombinedEffectDecompositionError::UnsupportedRootComposition);
    }

    let mut depth = 0usize;
    let mut candidate = None;
    for (index, op) in scene.ops.iter().enumerate() {
        match op {
            SceneOp::PushLayer(layer) => {
                if layer.backdrop_filter.is_some() && !layer.filters.is_empty() {
                    if candidate.is_some() {
                        return Err(CombinedEffectDecompositionError::MultipleCombinedEffectLayers);
                    }
                    if depth != 0 {
                        return Err(CombinedEffectDecompositionError::NestedCombinedEffectLayer);
                    }
                    candidate = Some(index);
                }
                depth += 1;
            }
            SceneOp::PopLayer => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let push_index = candidate.ok_or(CombinedEffectDecompositionError::NoCombinedEffectLayer)?;
    let pop_index = matching_pop(&scene.ops, push_index)
        .ok_or(CombinedEffectDecompositionError::UnbalancedLayer)?;
    if pop_index + 1 != scene.ops.len() {
        return Err(CombinedEffectDecompositionError::LayerMustBeTrailing);
    }
    let SceneOp::PushLayer(layer) = &scene.ops[push_index] else {
        unreachable!("candidate was selected from PushLayer")
    };
    if layer.blend_mode != SceneBlendMode::Normal || layer.compose != SceneCompose::SrcOver {
        return Err(CombinedEffectDecompositionError::UnsupportedLayerComposition);
    }
    if layer.transform_id != 0 {
        return Err(
            CombinedEffectDecompositionError::UnsupportedLayerTransform {
                transform_id: layer.transform_id,
            },
        );
    }
    let bounds = match &layer.clip {
        SceneClip::None => [
            0.0,
            0.0,
            scene.viewport_width as f32,
            scene.viewport_height as f32,
        ],
        SceneClip::Rect { rect, radii } if radii.iter().all(|radius| *radius == 0.0) => *rect,
        SceneClip::Rect { .. } | SceneClip::Path(_) => {
            return Err(CombinedEffectDecompositionError::UnsupportedClip);
        }
    };
    let Some(SceneFilter::Blur(backdrop_radius)) = layer.backdrop_filter else {
        return Err(CombinedEffectDecompositionError::UnsupportedBackdropFilter);
    };

    Ok(CombinedEffectDecomposition {
        prefix: build_prefix_scene(scene, push_index),
        content: build_layer_content_scene(scene, push_index, pop_index),
        bounds,
        alpha: layer.alpha.clamp(0.0, 1.0),
        backdrop_radius,
        element_filters: layer.filters.clone(),
    })
}

#[derive(Clone)]
struct TwoImagePipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

const TWO_IMAGE_WGSL: &str = r#"
struct Params {
    dest: vec4<f32>,
    uv: vec4<f32>,
    viewport_opacity: vec4<f32>,
};

@group(0) @binding(0) var bottom_texture: texture_2d<f32>;
@group(0) @binding(1) var top_texture: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;
@group(0) @binding(3) var<uniform> params: Params;

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
    let bottom = textureSample(bottom_texture, image_sampler, in.uv);
    let top = textureSample(top_texture, image_sampler, in.uv);
    let opacity = clamp(params.viewport_opacity.z, 0.0, 1.0);
    let scaled_top = top * opacity;
    return scaled_top + bottom * (1.0 - top.a * opacity);
}
"#;

fn build_two_image_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
) -> TwoImagePipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("netrender RG2c two-image join"),
        source: wgpu::ShaderSource::Wgsl(TWO_IMAGE_WGSL.into()),
    });
    let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("netrender RG2c two-image layout"),
        entries: &[
            texture_entry(0),
            texture_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
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
        label: Some("netrender RG2c two-image pipeline layout"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("netrender RG2c two-image pipeline"),
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
                format,
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
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("netrender RG2c two-image sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });
    TwoImagePipeline {
        pipeline,
        layout,
        sampler,
    }
}

fn two_image_callback(
    pipe: TwoImagePipeline,
    viewport: [u32; 2],
    dest: [f32; 4],
    uv: [f32; 4],
    opacity: f32,
) -> PrepareCallback {
    let values = [
        dest[0],
        dest[1],
        dest[2],
        dest[3],
        uv[0],
        uv[1],
        uv[2],
        uv[3],
        viewport[0] as f32,
        viewport[1] as f32,
        opacity,
        0.0,
    ];
    let mut bytes = [0u8; 48];
    for (index, value) in values.iter().enumerate() {
        bytes[index * 4..(index + 1) * 4].copy_from_slice(&value.to_ne_bytes());
    }
    Box::new(move |device, inputs| {
        assert_eq!(inputs.len(), 2, "RG2c join requires two image inputs");
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("netrender RG2c two-image params"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        params
            .slice(..)
            .get_mapped_range_mut()
            .expect("RG2c params mapping")
            .copy_from_slice(&bytes);
        params.unmap();
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("netrender RG2c two-image bind group"),
            layout: &pipe.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&inputs[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&inputs[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&pipe.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        image_render_commands(move |pass| {
            pass.set_pipeline(&pipe.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        })
    })
}

impl Renderer {
    fn rg2c_transient(
        graph: &mut RenderGraph,
        size: wgpu::Extent3d,
        label: impl Into<String>,
    ) -> ImageNode {
        graph.transient_image(TransientImageDesc {
            size,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            label: Some(label.into()),
        })
    }

    fn append_rg2c_blur(
        &self,
        graph: &mut RenderGraph,
        input: ImageNode,
        dim: u32,
        radius: f32,
        branch: &str,
    ) -> ImageNode {
        if radius <= 0.0 {
            return input;
        }
        let device = &self.wgpu_device.core.device;
        let pipe = self
            .wgpu_device
            .ensure_brush_blur(wgpu::TextureFormat::Rgba8Unorm);
        let sampler = make_bilinear_sampler(device);
        let (level, passes, step_px) = blur_kernel_plan_with_downscale(radius);
        let full = wgpu::Extent3d {
            width: dim,
            height: dim,
            depth_or_array_layers: 1,
        };
        let scaled_dim = (dim / level).max(1);
        let scaled = wgpu::Extent3d {
            width: scaled_dim,
            height: scaled_dim,
            depth_or_array_layers: 1,
        };
        let mut previous = input;
        if level > 1 {
            let down = Self::rg2c_transient(graph, scaled, format!("{branch} downsample"));
            graph
                .add_plan_task(
                    format!("{branch} downsample"),
                    vec![ImageUse::sampled_read(previous)],
                    ImageUse::color_attachment(down, ImageLoad::Clear),
                    blur_pass_callback(pipe.clone(), Arc::clone(&sampler), 0.0, 0.0),
                )
                .expect("RG2c blur downsample admission");
            previous = down;
        }
        let step = step_px / scaled_dim as f32;
        for pass in 0..passes {
            let horizontal =
                Self::rg2c_transient(graph, scaled, format!("{branch} horizontal {pass}"));
            graph
                .add_plan_task(
                    format!("{branch} horizontal {pass}"),
                    vec![ImageUse::sampled_read(previous)],
                    ImageUse::color_attachment(horizontal, ImageLoad::Clear),
                    blur_pass_callback(pipe.clone(), Arc::clone(&sampler), step, 0.0),
                )
                .expect("RG2c horizontal blur admission");
            let vertical = Self::rg2c_transient(graph, scaled, format!("{branch} vertical {pass}"));
            graph
                .add_plan_task(
                    format!("{branch} vertical {pass}"),
                    vec![ImageUse::sampled_read(horizontal)],
                    ImageUse::color_attachment(vertical, ImageLoad::Clear),
                    blur_pass_callback(pipe.clone(), Arc::clone(&sampler), 0.0, step),
                )
                .expect("RG2c vertical blur admission");
            previous = vertical;
        }
        if level > 1 {
            let up = Self::rg2c_transient(graph, full, format!("{branch} upsample"));
            graph
                .add_plan_task(
                    format!("{branch} upsample"),
                    vec![ImageUse::sampled_read(previous)],
                    ImageUse::color_attachment(up, ImageLoad::Clear),
                    blur_pass_callback(pipe, sampler, 0.0, 0.0),
                )
                .expect("RG2c blur upsample admission");
            previous = up;
        }
        previous
    }

    fn append_rg2c_filters(
        &self,
        graph: &mut RenderGraph,
        input: ImageNode,
        dim: u32,
        filters: &[SceneFilter],
    ) -> ImageNode {
        let size = wgpu::Extent3d {
            width: dim,
            height: dim,
            depth_or_array_layers: 1,
        };
        let mut previous = input;
        for (index, filter) in filters.iter().copied().enumerate() {
            match filter {
                SceneFilter::Blur(radius) => {
                    previous = self.append_rg2c_blur(
                        graph,
                        previous,
                        dim,
                        radius,
                        &format!("rg2c element blur {index}"),
                    );
                }
                filter => {
                    let output =
                        Self::rg2c_transient(graph, size, format!("rg2c element matrix {index}"));
                    let pipe = self
                        .wgpu_device
                        .ensure_color_matrix(wgpu::TextureFormat::Rgba8Unorm);
                    let sampler = make_bilinear_sampler(&self.wgpu_device.core.device);
                    graph
                        .add_plan_task(
                            format!("rg2c element matrix {index}"),
                            vec![ImageUse::sampled_read(previous)],
                            ImageUse::color_attachment(output, ImageLoad::Clear),
                            color_matrix_callback(pipe, sampler, scene_filter_to_matrix(filter)),
                        )
                        .expect("RG2c color matrix admission");
                    previous = output;
                }
            }
        }
        previous
    }

    pub(crate) fn build_combined_effect_plan(
        &self,
        effect: &CombinedEffectDecomposition,
        execution: RasterExecution,
    ) -> CombinedEffectPlan {
        let dim = effect.prefix.viewport_width;
        let size = wgpu::Extent3d {
            width: dim,
            height: dim,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let prefix_input = graph.import_image(
            format!("rg2c {:?} prefix raster", execution.backend),
            size,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let content_input = graph.import_image(
            format!("rg2c {:?} content raster", execution.backend),
            size,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let backdrop = self.append_rg2c_blur(
            &mut graph,
            prefix_input,
            dim,
            effect.backdrop_radius,
            "rg2c backdrop blur",
        );
        let content =
            self.append_rg2c_filters(&mut graph, content_input, dim, &effect.element_filters);
        let group = Self::rg2c_transient(&mut graph, size, "rg2c joined layer");
        let output = Self::rg2c_transient(&mut graph, size, "rg2c final output");
        let pipe = build_two_image_pipeline(
            &self.wgpu_device.core.device,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let uv = [
            effect.bounds[0] / dim as f32,
            effect.bounds[1] / dim as f32,
            effect.bounds[2] / dim as f32,
            effect.bounds[3] / dim as f32,
        ];
        graph
            .add_plan_task(
                "rg2c two-input layer join",
                vec![
                    ImageUse::sampled_read(backdrop),
                    ImageUse::sampled_read(content),
                ],
                ImageUse::color_attachment(group, ImageLoad::Clear),
                two_image_callback(pipe.clone(), [dim, dim], effect.bounds, uv, 1.0),
            )
            .expect("RG2c layer join admission");
        graph
            .add_plan_task(
                "rg2c prefix and layer alpha composite",
                vec![
                    ImageUse::sampled_read(prefix_input),
                    ImageUse::sampled_read(group),
                ],
                ImageUse::color_attachment(output, ImageLoad::Clear),
                two_image_callback(
                    pipe,
                    [dim, dim],
                    [0.0, 0.0, dim as f32, dim as f32],
                    [0.0, 0.0, 1.0, 1.0],
                    effect.alpha,
                ),
            )
            .expect("RG2c final composite admission");
        let plan = graph
            .compile(&[output])
            .expect("RG2c combined-effect graph compilation")
            .with_diagnostic_header(execution.dump());
        CombinedEffectPlan {
            plan,
            prefix_input,
            content_input,
            output,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> Scene {
        let mut scene = Scene::new(64, 64);
        scene.push_rect(0.0, 0.0, 32.0, 64.0, [0.0, 0.0, 1.0, 1.0]);
        scene.push_rect(32.0, 0.0, 64.0, 64.0, [1.0, 1.0, 0.0, 1.0]);
        let mut layer = crate::SceneLayer::clip(SceneClip::Rect {
            rect: [16.0, 8.0, 48.0, 56.0],
            radii: [0.0; 4],
        });
        layer.alpha = 0.6;
        layer.backdrop_filter = Some(SceneFilter::Blur(8.0));
        layer.filters.push(SceneFilter::Invert(1.0));
        scene.push_layer(layer);
        scene.push_rect(16.0, 8.0, 48.0, 56.0, [1.0, 0.0, 0.0, 0.35]);
        scene.pop_layer();
        scene
    }

    #[test]
    fn decomposes_one_bounded_trailing_combined_effect() {
        let effect = decompose_combined_effect(&scene()).unwrap();
        assert_eq!(effect.bounds, [16.0, 8.0, 48.0, 56.0]);
        assert_eq!(effect.alpha, 0.6);
        assert_eq!(effect.backdrop_radius, 8.0);
        assert_eq!(effect.element_filters, [SceneFilter::Invert(1.0)]);
        assert_eq!(effect.prefix.ops.len(), 2);
        assert_eq!(effect.content.ops.len(), 1);
        assert!(!effect
            .prefix
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::PushLayer(layer) if layer.backdrop_filter.is_some())));
        assert!(!effect
            .content
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::PushLayer(layer) if !layer.filters.is_empty())));
    }

    #[test]
    fn refuses_a_tail_instead_of_losing_painter_order() {
        let mut scene = scene();
        scene.push_rect(0.0, 0.0, 4.0, 4.0, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(
            decompose_combined_effect(&scene).unwrap_err(),
            CombinedEffectDecompositionError::LayerMustBeTrailing
        );
    }
}
