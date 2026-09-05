// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! RG2b execution-shape receipt.
//!
//! The filter-bearing direct Scene remains a Classic semantic proof and a
//! typed Hybrid/CPU refusal in `rg2a_scene_corpus`. This receipt deliberately
//! uses a separate filter-free producer fixture to prove that the three
//! participation labels can feed one downstream image topology. It does not
//! claim that sparse backends silently execute Classic's filter preprocessor.

use netrender_device::render_graph::{
    ImageLoad, ImageUse, RenderGraph, TransientImageDesc, image_render_commands,
};
use crate::renderer::RasterExecution;

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
use std::collections::HashMap;

fn labeled_plan(execution: RasterExecution) -> String {
    let size = wgpu::Extent3d {
        width: 4,
        height: 4,
        depth_or_array_layers: 1,
    };
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC;
    let mut graph = RenderGraph::new();
    let input = graph.import_image("rg2b producer", size, wgpu::TextureFormat::Rgba8Unorm);
    let output = graph.transient_image(TransientImageDesc {
        size,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage,
        label: Some("rg2b downstream output".into()),
    });
    graph
        .add_plan_task(
            "rg2b downstream composite",
            vec![ImageUse::sampled_read(input)],
            ImageUse::color_attachment(output, ImageLoad::Clear),
            Box::new(|_device, _inputs| image_render_commands(|_pass| {})),
        )
        .expect("RG2b downstream fixture task");
    graph
        .compile(&[output])
        .expect("RG2b downstream fixture plan")
        .with_diagnostic_header(execution.dump())
        .dump()
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn producer_scene() -> crate::Scene {
    let mut scene = crate::Scene::new(32, 32);
    scene.push_rect(8.0, 8.0, 24.0, 24.0, [1.0, 0.25, 0.0, 1.0]);
    scene
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn producer_target(device: &wgpu::Device, usage: wgpu::TextureUsages) -> wgpu::Texture {
    producer_target_size(device, 32, 32, usage)
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn producer_target_size(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rg2b producer target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage,
        view_formats: &[],
    })
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn external_source(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
    let source = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rg2b external source"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[0, 0, 255, 255],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
    );
    source
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn downstream_plan(
    renderer: &crate::Renderer,
    producer: wgpu::Texture,
    execution: RasterExecution,
) -> (
    netrender_device::render_graph::ExecutionPlan,
    netrender_device::render_graph::ImageNode,
    netrender_device::render_graph::ImageNode,
    wgpu::Texture,
) {
    use crate::filter::{blur_pass_callback, make_bilinear_sampler};

    let device = renderer.wgpu_device.core.device.clone();
    let extent = wgpu::Extent3d {
        width: 32,
        height: 32,
        depth_or_array_layers: 1,
    };
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC;
    let pipe = renderer.wgpu_device.ensure_brush_blur(format);
    let sampler = make_bilinear_sampler(&device);
    let mut graph = RenderGraph::new();
    let input = graph.import_image("rg2b producer", extent, format);
    let output = graph.transient_image(TransientImageDesc {
        size: extent,
        format,
        usage,
        label: Some("rg2b downstream blur".into()),
    });
    graph
        .add_plan_task(
            "rg2b downstream blur",
            vec![ImageUse::sampled_read(input)],
            ImageUse::color_attachment(output, ImageLoad::Clear),
            blur_pass_callback(pipe, sampler, 1.0 / 32.0, 0.0),
        )
        .expect("RG2b downstream task");
    let plan = graph
        .compile(&[output])
        .expect("RG2b downstream plan")
        .with_diagnostic_header(execution.dump());
    (plan, input, output, producer)
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn read_center(renderer: &crate::Renderer, texture: &wgpu::Texture) -> [u8; 4] {
    let bytes = renderer.wgpu_device.read_rgba8_texture(texture, 32, 32);
    bytes[(16 * 32 * 4 + 16 * 4)..(16 * 32 * 4 + 17 * 4)]
        .try_into()
        .expect("center pixel")
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
#[test]
#[ignore = "physical RG2b three-backend execution receipt"]
fn rg2b_three_execution_shapes_share_downstream_readback() {
    let _gpu_guard = super::gpu_test_guard();
    let handles = crate::boot().expect("wgpu boot");
    let renderer = crate::create_netrender_instance(
        handles,
        crate::NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .expect("renderer");
    let scene = producer_scene();
    let device = renderer.wgpu_device.core.device.clone();
    let queue = renderer.wgpu_device.core.queue.clone();

    // Classic is deliberately an opaque submission: Vello owns its submit,
    // and the graph starts from the initialized producer texture afterward.
    let classic_producer = producer_target(
        &device,
        wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC,
    );
    let classic_view = classic_producer.create_view(&Default::default());
    renderer.render_vello(
        &scene,
        &classic_view,
        crate::ColorLoad::Clear(wgpu::Color::TRANSPARENT),
    );
    let (plan, input, classic_output_node, classic_producer) =
        downstream_plan(&renderer, classic_producer, RasterExecution::classic());
    let classic_dump = plan.dump();
    let classic_external = external_source(&device, &queue);
    let classic_external_view = classic_external.create_view(&Default::default());
    let mut classic_encoder = device.create_command_encoder(&Default::default());
    renderer.encode_external_texture(
        &classic_external_view,
        &classic_view,
        wgpu::TextureFormat::Rgba8Unorm,
        32,
        32,
        crate::ExternalTexturePlacement::new([0.0, 0.0, 32.0, 32.0]).with_opacity(0.5),
        &mut classic_encoder,
    );
    let (mut outputs, _) = plan
        .encode_into(
            &device,
            HashMap::from([(input, classic_producer)]),
            &mut classic_encoder,
        )
        .expect("Classic downstream graph");
    queue.submit([classic_encoder.finish()]);
    let classic_output = outputs.remove(&classic_output_node).unwrap();
    assert!(read_center(&renderer, &classic_output)[0] > 100);
    assert!(classic_dump.contains("rasterizer=Classic execution_boundary=opaque_submission"));
    assert!(read_center(&renderer, &classic_output)[2] > 80);
    drop(classic_output);

    // Hybrid receives the graph-owned encoder and records its raster work
    // before the same downstream plan encodes its blur pass.
    let hybrid_producer = producer_target(
        &device,
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
    );
    let hybrid_view = hybrid_producer.create_view(&Default::default());
    let hybrid_scene = crate::vello_backends::scene_to_vello_hybrid(&scene)
        .expect("Hybrid filter-free fixture admission");
    let mut hybrid_renderer = vello_hybrid::Renderer::new(
        &device,
        &vello_hybrid::RenderTargetConfig {
            format: wgpu::TextureFormat::Rgba8Unorm,
            width: 32,
            height: 32,
        },
    );
    let render_size = vello_hybrid::RenderSize {
        width: 32,
        height: 32,
    };
    let depth = vello_hybrid::Renderer::create_depth_texture_view(&device, &render_size);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("rg2b Hybrid encoder batch"),
    });
    hybrid_renderer
        .0
        .render(
            &hybrid_scene,
            &mut hybrid_renderer.1,
            &device,
            &queue,
            &mut encoder,
            &render_size,
            &hybrid_view,
            Some(&depth),
            &vello_hybrid::TextureBindings::new(),
        )
        .expect("Hybrid encoder participation");
    let hybrid_external = external_source(&device, &queue);
    let hybrid_external_view = hybrid_external.create_view(&Default::default());
    renderer.encode_external_texture(
        &hybrid_external_view,
        &hybrid_view,
        wgpu::TextureFormat::Rgba8Unorm,
        32,
        32,
        crate::ExternalTexturePlacement::new([0.0, 0.0, 32.0, 32.0]).with_opacity(0.5),
        &mut encoder,
    );
    let (plan, input, hybrid_output_node, hybrid_producer) =
        downstream_plan(&renderer, hybrid_producer, RasterExecution::hybrid());
    let hybrid_dump = plan.dump();
    let (mut outputs, _) = plan
        .encode_into(
            &device,
            HashMap::from([(input, hybrid_producer)]),
            &mut encoder,
        )
        .expect("Hybrid downstream graph");
    queue.submit([encoder.finish()]);
    let hybrid_output = outputs.remove(&hybrid_output_node).unwrap();
    assert!(read_center(&renderer, &hybrid_output)[0] > 100);
    assert!(read_center(&renderer, &hybrid_output)[2] > 80);
    assert!(hybrid_dump.contains("rasterizer=Hybrid execution_boundary=encoder_batch"));
    drop(hybrid_output);

    // CPU is outside the graph. Its named ready upload/import is a queue
    // upload into the same-device producer texture before graph execution.
    let cpu_producer = producer_target(
        &device,
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
    );
    let cpu_context = crate::vello_backends::scene_to_vello_cpu(&scene)
        .expect("CPU filter-free fixture admission");
    let mut pixmap = vello_cpu::Pixmap::new(32, 32);
    let mut resources = vello_cpu::Resources::new();
    cpu_context.render(&mut pixmap, &mut resources);
    let bytes = pixmap
        .data()
        .iter()
        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
        .collect::<Vec<_>>();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &cpu_producer,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(32 * 4),
            rows_per_image: Some(32),
        },
        wgpu::Extent3d {
            width: 32,
            height: 32,
            depth_or_array_layers: 1,
        },
    );
    let (plan, input, cpu_output_node, cpu_producer) =
        downstream_plan(&renderer, cpu_producer, RasterExecution::cpu());
    let cpu_dump = plan.dump();
    let cpu_external = external_source(&device, &queue);
    let cpu_external_view = cpu_external.create_view(&Default::default());
    let cpu_view = cpu_producer.create_view(&Default::default());
    let mut cpu_encoder = device.create_command_encoder(&Default::default());
    renderer.encode_external_texture(
        &cpu_external_view,
        &cpu_view,
        wgpu::TextureFormat::Rgba8Unorm,
        32,
        32,
        crate::ExternalTexturePlacement::new([0.0, 0.0, 32.0, 32.0]).with_opacity(0.5),
        &mut cpu_encoder,
    );
    let (mut outputs, _) = plan
        .encode_into(
            &device,
            HashMap::from([(input, cpu_producer)]),
            &mut cpu_encoder,
        )
        .expect("CPU downstream graph");
    queue.submit([cpu_encoder.finish()]);
    let cpu_output = outputs.remove(&cpu_output_node).unwrap();
    assert!(read_center(&renderer, &cpu_output)[0] > 100);
    assert!(read_center(&renderer, &cpu_output)[2] > 80);
    assert!(cpu_dump.contains("rasterizer=Cpu execution_boundary=ready_upload_import"));
}

#[test]
fn rg2b_dump_names_three_execution_boundaries_over_one_topology() {
    let classic = labeled_plan(RasterExecution::classic());
    let hybrid = labeled_plan(RasterExecution::hybrid());
    let cpu = labeled_plan(RasterExecution::cpu());

    for dump in [&classic, &hybrid, &cpu] {
        assert!(dump.contains("rg2b downstream composite"));
        assert!(dump.contains("import->0"));
        assert!(dump.contains("graph_encoder_batches: 1"));
        assert!(dump.contains("graph_submission_boundaries: 1 (submit)"));
    }
    assert!(classic.contains("rasterizer=Classic execution_boundary=opaque_submission"));
    assert!(hybrid.contains("rasterizer=Hybrid execution_boundary=encoder_batch"));
    assert!(cpu.contains("rasterizer=Cpu execution_boundary=ready_upload_import"));
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
fn combined_filter_scene() -> crate::Scene {
    let mut scene = crate::Scene::new(64, 64);
    // A hard color boundary makes backdrop blur observable beneath the
    // semi-transparent filtered layer.
    scene.push_rect(0.0, 0.0, 32.0, 64.0, [0.0, 0.0, 1.0, 1.0]);
    scene.push_rect(32.0, 0.0, 64.0, 64.0, [1.0, 1.0, 0.0, 1.0]);
    let mut layer = crate::SceneLayer::clip(crate::SceneClip::Rect {
        rect: [16.0, 8.0, 48.0, 56.0],
        radii: [0.0; 4],
    });
    layer.alpha = 0.6;
    layer.backdrop_filter = Some(crate::SceneFilter::Blur(8.0));
    layer.filters.push(crate::SceneFilter::Invert(1.0));
    scene.push_layer(layer);
    // Partial alpha leaves the backdrop visible beneath the element filter,
    // making both halves of the combined operation causally observable.
    scene.push_rect(16.0, 8.0, 48.0, 56.0, [1.0, 0.0, 0.0, 0.35]);
    scene.pop_layer();
    scene
}

#[cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]
#[test]
fn rg2b_combined_filter_scene_is_classic_only_and_visible() {
    let _gpu_guard = super::gpu_test_guard();
    let scene = combined_filter_scene();
    let classic = crate::vello_backends::validate_scene_for_backend(
        crate::vello_backends::VelloBackend::Classic,
        &scene,
    );
    assert!(classic.is_ok(), "Classic admits the combined filter Scene");
    let hybrid = crate::vello_backends::scene_to_vello_hybrid(&scene).unwrap_err();
    assert!(matches!(
        hybrid,
        crate::vello_backends::BackendAdmissionError::UnsupportedOperation {
            backend: crate::vello_backends::VelloBackend::Hybrid,
            op_index: 2,
            operation: "PushLayer",
            reason: "Netrender filter graphs have not been mapped to sparse filters",
        }
    ));
    let cpu = crate::vello_backends::scene_to_vello_cpu(&scene).unwrap_err();
    assert!(matches!(
        cpu,
        crate::vello_backends::BackendAdmissionError::UnsupportedOperation {
            backend: crate::vello_backends::VelloBackend::Cpu,
            op_index: 2,
            operation: "PushLayer",
            reason: "Netrender filter graphs have not been mapped to sparse filters",
        }
    ));

    let handles = crate::boot().expect("wgpu boot");
    let renderer = crate::create_netrender_instance(
        handles.clone(),
        crate::NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .expect("renderer");
    let target = producer_target_size(
        &handles.device,
        64,
        64,
        wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
    );
    let view = target.create_view(&Default::default());
    renderer.render_vello(
        &scene,
        &view,
        crate::ColorLoad::Clear(wgpu::Color::TRANSPARENT),
    );
    let bytes = renderer.wgpu_device.read_rgba8_texture(&target, 64, 64);
    let pixel = |x: u32, y: u32| {
        let offset = ((y * 64 + x) * 4) as usize;
        [
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]
    };
    let center = pixel(32, 32);
    let boundary_left = pixel(24, 32);
    let boundary_right = pixel(40, 32);
    assert!(
        center[3] > 200,
        "combined filtered layer should be visible: {center:?}"
    );
    assert!(
        boundary_left != boundary_right,
        "backdrop blur layer should preserve a visible boundary contribution: left={boundary_left:?} right={boundary_right:?}"
    );

    let render_variant = |variant: &crate::Scene| {
        let variant_renderer = crate::create_netrender_instance(
            handles.clone(),
            crate::NetrenderOptions {
                tile_cache_size: Some(64),
                enable_vello: true,
                ..Default::default()
            },
        )
        .expect("variant renderer");
        let target = producer_target_size(
            &handles.device,
            64,
            64,
            wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
        );
        let view = target.create_view(&Default::default());
        variant_renderer.render_vello(
            variant,
            &view,
            crate::ColorLoad::Clear(wgpu::Color::TRANSPARENT),
        );
        variant_renderer
            .wgpu_device
            .read_rgba8_texture(&target, 64, 64)
    };
    let mut without_backdrop = scene.clone();
    let mut without_element = scene.clone();
    for op in &mut without_backdrop.ops {
        if let crate::SceneOp::PushLayer(layer) = op {
            layer.backdrop_filter = None;
        }
    }
    for op in &mut without_element.ops {
        if let crate::SceneOp::PushLayer(layer) = op {
            layer.filters.clear();
        }
    }
    let without_backdrop = render_variant(&without_backdrop);
    let without_element = render_variant(&without_element);
    assert_ne!(
        &bytes[((32 * 64 + 32) * 4) as usize..((32 * 64 + 33) * 4) as usize],
        &without_element[((32 * 64 + 32) * 4) as usize..((32 * 64 + 33) * 4) as usize],
        "element filter must change the combined-scene center"
    );
    let sample = |image: &[u8], x: u32, y: u32| {
        let offset = ((y * 64 + x) * 4) as usize;
        [
            image[offset],
            image[offset + 1],
            image[offset + 2],
            image[offset + 3],
        ]
    };
    let combined_boundary = sample(&bytes, 31, 32);
    let backdrop_boundary = sample(&without_backdrop, 31, 32);
    eprintln!(
        "rg2b combined anchors center={center:?} no_element={:?} boundary={combined_boundary:?} no_backdrop={backdrop_boundary:?}",
        sample(&without_element, 32, 32)
    );
    assert_ne!(
        combined_boundary, backdrop_boundary,
        "backdrop blur must change the partially transparent combined-scene boundary"
    );
}
