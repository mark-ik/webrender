// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Crate-local receipts for the private image execution plan.

mod p6_render_graph;
mod p9a_clip_rectangle;
mod p9b_box_shadow;
mod p9c_clip_fast_path;
mod rg2b;
mod rg2c;
mod rg3_imported_output;
mod rg3a_opaque_tenant;
mod rg_measurement;

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::filter::{blur_pass_callback, clip_rectangle_callback, make_bilinear_sampler};
use netrender_device::render_graph::{ImageLoad, ImageUse, RenderGraph, TransientImageDesc};
use crate::Renderer;

const MASK_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

static GPU_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(super) fn gpu_test_guard() -> MutexGuard<'static, ()> {
    GPU_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("render graph GPU test lock")
}

fn render_clip_mask(
    renderer: &Renderer,
    extent: u32,
    bounds: [f32; 4],
    radius: f32,
    has_rounded_corners: bool,
) -> wgpu::Texture {
    let device = renderer.wgpu_device.core.device.clone();
    let queue = renderer.wgpu_device.core.queue.clone();
    let pipe = renderer
        .wgpu_device
        .ensure_clip_rectangle(MASK_FORMAT, has_rounded_corners, false);
    let size = wgpu::Extent3d {
        width: extent,
        height: extent,
        depth_or_array_layers: 1,
    };
    let mut graph = RenderGraph::new();
    let mask = graph.transient_image(TransientImageDesc {
        size,
        format: MASK_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        label: Some("test clip mask".into()),
    });
    graph
        .add_plan_task(
            "test clip mask",
            Vec::new(),
            ImageUse::color_attachment(mask, ImageLoad::Clear),
            clip_rectangle_callback(pipe, bounds, radius),
        )
        .unwrap();
    let plan = graph.compile(&[mask]).unwrap();
    let (mut outputs, _) = plan.execute(&device, &queue, Default::default()).unwrap();
    outputs.remove(&mask).unwrap()
}

fn render_box_shadow_mask(
    renderer: &Renderer,
    extent: u32,
    bounds: [f32; 4],
    radius: f32,
) -> Arc<wgpu::Texture> {
    let device = renderer.wgpu_device.core.device.clone();
    let queue = renderer.wgpu_device.core.queue.clone();
    let clip_pipe = renderer
        .wgpu_device
        .ensure_clip_rectangle(MASK_FORMAT, true, false);
    let blur_pipe = renderer.wgpu_device.ensure_brush_blur(MASK_FORMAT);
    let sampler = make_bilinear_sampler(&device);
    let size = wgpu::Extent3d {
        width: extent,
        height: extent,
        depth_or_array_layers: 1,
    };
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC;
    let mut graph = RenderGraph::new();
    let mask = graph.transient_image(TransientImageDesc {
        size,
        format: MASK_FORMAT,
        usage,
        label: Some("test box shadow mask".into()),
    });
    let blur_h = graph.transient_image(TransientImageDesc {
        size,
        format: MASK_FORMAT,
        usage,
        label: Some("test box shadow horizontal".into()),
    });
    let blur_v = graph.transient_image(TransientImageDesc {
        size,
        format: MASK_FORMAT,
        usage,
        label: Some("test box shadow vertical".into()),
    });
    graph
        .add_plan_task(
            "test box shadow mask",
            Vec::new(),
            ImageUse::color_attachment(mask, ImageLoad::Clear),
            clip_rectangle_callback(clip_pipe, bounds, radius),
        )
        .unwrap();
    let step = 1.0 / extent as f32;
    graph
        .add_plan_task(
            "test box shadow horizontal",
            vec![ImageUse::sampled_read(mask)],
            ImageUse::color_attachment(blur_h, ImageLoad::Clear),
            blur_pass_callback(blur_pipe.clone(), Arc::clone(&sampler), step, 0.0),
        )
        .unwrap();
    graph
        .add_plan_task(
            "test box shadow vertical",
            vec![ImageUse::sampled_read(blur_h)],
            ImageUse::color_attachment(blur_v, ImageLoad::Clear),
            blur_pass_callback(blur_pipe, sampler, 0.0, step),
        )
        .unwrap();
    let plan = graph.compile(&[blur_v]).unwrap();
    let (mut outputs, _) = plan.execute(&device, &queue, Default::default()).unwrap();
    Arc::new(outputs.remove(&blur_v).unwrap())
}

fn make_renderer() -> Renderer {
    let handles = crate::boot().expect("wgpu boot");
    crate::create_netrender_instance(
        handles,
        crate::NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .expect("create_netrender_instance")
}

fn render_to_bytes(renderer: &Renderer, scene: &crate::Scene) -> Vec<u8> {
    let device = renderer.wgpu_device.core.device.clone();
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("render graph test target"),
        size: wgpu::Extent3d {
            width: scene.viewport_width,
            height: scene.viewport_height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: MASK_FORMAT,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor {
        format: Some(MASK_FORMAT),
        ..Default::default()
    });
    renderer.render_vello(scene, &view, crate::ColorLoad::Clear(wgpu::Color::BLACK));
    renderer
        .wgpu_device
        .read_rgba8_texture(&target, scene.viewport_width, scene.viewport_height)
}

fn pixel(bytes: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
}
