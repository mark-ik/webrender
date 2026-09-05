// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Physical RG2c receipt for one renderer-owned combined-effect shape.

#![cfg(all(feature = "vello-all", not(target_arch = "wasm32")))]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::renderer::RasterExecution;
use crate::renderer::rg2c::{CombinedEffectDecomposition, decompose_combined_effect};
use crate::vello_backends::VelloBackend;

const DIM: u32 = 64;

struct Run {
    bytes: Vec<u8>,
    dump: String,
}

fn scene() -> crate::Scene {
    let mut scene = crate::Scene::new(DIM, DIM);
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
    scene.push_rect(16.0, 8.0, 48.0, 56.0, [1.0, 0.0, 0.0, 0.35]);
    scene.pop_layer();
    scene
}

fn target(device: &wgpu::Device, backend: VelloBackend) -> wgpu::Texture {
    let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC
        | wgpu::TextureUsages::COPY_DST;
    if backend == VelloBackend::Classic {
        usage |= wgpu::TextureUsages::STORAGE_BINDING;
    }
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rg2c raster fragment"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
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

fn upload_cpu(queue: &wgpu::Queue, texture: &wgpu::Texture, scene: &crate::Scene) {
    let context = crate::vello_backends::scene_to_vello_cpu(scene)
        .expect("RG2c decomposition must produce a CPU-admissible fragment");
    let mut pixmap = vello_cpu::Pixmap::new(DIM as u16, DIM as u16);
    let mut resources = vello_cpu::Resources::new();
    context.render(&mut pixmap, &mut resources);
    let bytes = pixmap
        .data()
        .iter()
        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a])
        .collect::<Vec<_>>();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(DIM * 4),
            rows_per_image: Some(DIM),
        },
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
    );
}

fn encode_hybrid(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    scene: &crate::Scene,
    target: &wgpu::Texture,
) {
    let scene = crate::vello_backends::scene_to_vello_hybrid(scene)
        .expect("RG2c decomposition must produce a Hybrid-admissible fragment");
    let size = vello_hybrid::RenderSize {
        width: DIM,
        height: DIM,
    };
    let mut renderer = vello_hybrid::Renderer::new(
        device,
        &vello_hybrid::RenderTargetConfig {
            format: wgpu::TextureFormat::Rgba8Unorm,
            width: DIM,
            height: DIM,
        },
    );
    let depth = vello_hybrid::Renderer::create_depth_texture_view(device, &size);
    renderer
        .0
        .render(
            &scene,
            &mut renderer.1,
            device,
            queue,
            encoder,
            &size,
            &target.create_view(&Default::default()),
            Some(&depth),
            &vello_hybrid::TextureBindings::new(),
        )
        .expect("Hybrid fragment recording");
}

fn render_run(
    renderer: &crate::Renderer,
    effect: &CombinedEffectDecomposition,
    execution: RasterExecution,
) -> Run {
    let device = renderer.wgpu_device.core.device.clone();
    let queue = renderer.wgpu_device.core.queue.clone();
    let prefix = target(&device, execution.backend);
    let content = target(&device, execution.backend);
    let compiled = renderer.build_combined_effect_plan(effect, execution);
    let dump = compiled.plan.dump();
    let bindings = HashMap::from([
        (compiled.prefix_input, prefix),
        (compiled.content_input, content),
    ]);

    let output = match execution.backend {
        VelloBackend::Classic => {
            let prefix_view = bindings[&compiled.prefix_input].create_view(&Default::default());
            let content_view = bindings[&compiled.content_input].create_view(&Default::default());
            renderer.render_vello(
                &effect.prefix,
                &prefix_view,
                crate::ColorLoad::Clear(wgpu::Color::TRANSPARENT),
            );
            renderer.render_vello(
                &effect.content,
                &content_view,
                crate::ColorLoad::Clear(wgpu::Color::TRANSPARENT),
            );
            let (mut outputs, _) = compiled
                .plan
                .execute(&device, &queue, bindings)
                .expect("Classic RG2c execution");
            outputs.remove(&compiled.output).expect("RG2c output")
        }
        VelloBackend::Hybrid => {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rg2c Hybrid raster and graph batch"),
            });
            encode_hybrid(
                &device,
                &queue,
                &mut encoder,
                &effect.prefix,
                &bindings[&compiled.prefix_input],
            );
            encode_hybrid(
                &device,
                &queue,
                &mut encoder,
                &effect.content,
                &bindings[&compiled.content_input],
            );
            let (mut outputs, _) = compiled
                .plan
                .encode_into(&device, bindings, &mut encoder)
                .expect("Hybrid RG2c encoder participation");
            queue.submit([encoder.finish()]);
            outputs.remove(&compiled.output).expect("RG2c output")
        }
        VelloBackend::Cpu => {
            upload_cpu(&queue, &bindings[&compiled.prefix_input], &effect.prefix);
            upload_cpu(&queue, &bindings[&compiled.content_input], &effect.content);
            let (mut outputs, _) = compiled
                .plan
                .execute(&device, &queue, bindings)
                .expect("CPU RG2c execution");
            outputs.remove(&compiled.output).expect("RG2c output")
        }
    };
    Run {
        bytes: renderer.wgpu_device.read_rgba8_texture(&output, DIM, DIM),
        dump,
    }
}

fn pixel(bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
    let offset = ((y * DIM + x) * 4) as usize;
    bytes[offset..offset + 4].try_into().expect("pixel")
}

fn delta(a: [u8; 4], b: [u8; 4]) -> u32 {
    a.into_iter()
        .zip(b)
        .map(|(left, right)| left.abs_diff(right) as u32)
        .sum()
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[test]
#[ignore = "physical RG2c backend-neutral combined-effect receipt"]
fn rg2c_three_backends_execute_real_effect_fork_join() {
    let _gpu_guard = super::gpu_test_guard();
    let source = scene();
    assert!(
        crate::vello_backends::validate_scene_for_backend(VelloBackend::Classic, &source).is_ok()
    );
    assert!(crate::vello_backends::scene_to_vello_hybrid(&source).is_err());
    assert!(crate::vello_backends::scene_to_vello_cpu(&source).is_err());

    let effect = decompose_combined_effect(&source).expect("bounded RG2c decomposition");
    let handles = crate::boot().expect("wgpu boot");
    let adapter = format!("{:?}", handles.adapter.get_info());
    let renderer = crate::create_netrender_instance(
        handles,
        crate::NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .expect("renderer");
    let executions = [
        RasterExecution::classic(),
        RasterExecution::hybrid(),
        RasterExecution::cpu(),
    ];
    let mut rows = Vec::new();

    for execution in executions {
        let full = render_run(&renderer, &effect, execution);
        let mut without_backdrop = effect.clone();
        without_backdrop.backdrop_radius = 0.0;
        let no_backdrop = render_run(&renderer, &without_backdrop, execution);
        let mut without_element = effect.clone();
        without_element.element_filters.clear();
        let no_element = render_run(&renderer, &without_element, execution);

        let outside = pixel(&full.bytes, 4, 4);
        let backdrop_anchor = pixel(&full.bytes, 31, 32);
        let element_anchor = pixel(&full.bytes, 32, 32);
        let backdrop_delta = delta(backdrop_anchor, pixel(&no_backdrop.bytes, 31, 32));
        let element_delta = delta(element_anchor, pixel(&no_element.bytes, 32, 32));
        assert_eq!(
            outside,
            [0, 0, 255, 255],
            "prefix must survive outside layer"
        );
        assert!(backdrop_delta > 0, "backdrop branch must affect its anchor");
        assert!(element_delta > 0, "element branch must affect its anchor");
        for needle in [
            "prefix raster",
            "content raster",
            "rg2c backdrop blur horizontal",
            "rg2c backdrop blur vertical",
            "rg2c element matrix 0",
            "rg2c two-input layer join",
            "rg2c prefix and layer alpha composite",
            &execution.dump(),
        ] {
            assert!(
                full.dump.contains(needle),
                "missing {needle:?} in plan:\n{}",
                full.dump
            );
        }
        rows.push((
            execution,
            outside,
            backdrop_anchor,
            backdrop_delta,
            element_anchor,
            element_delta,
            full.dump,
        ));
    }

    let receipt_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repository root")
        .parent()
        .expect("repos root")
        .parent()
        .expect("workspace root")
        .join("testing/netrender");
    std::fs::create_dir_all(&receipt_dir).expect("receipt directory");
    let mut receipt = format!(
        "{{\n  \"adapter\": \"{}\",\n  \"runs\": [\n",
        json_escape(&adapter)
    );
    for (index, (execution, outside, backdrop, backdrop_delta, element, element_delta, dump)) in
        rows.iter().enumerate()
    {
        let comma = if index + 1 == rows.len() { "" } else { "," };
        writeln!(
            receipt,
            "    {{\"backend\": \"{:?}\", \"boundary\": \"{}\", \"outside\": {:?}, \"backdrop_anchor\": {:?}, \"backdrop_delta\": {}, \"element_anchor\": {:?}, \"element_delta\": {}, \"plan\": \"{}\"}}{}",
            execution.backend,
            execution.boundary_name(),
            outside,
            backdrop,
            backdrop_delta,
            element,
            element_delta,
            json_escape(dump),
            comma,
        ).expect("format receipt");
    }
    receipt.push_str("  ]\n}\n");
    let receipt_path = receipt_dir.join("rg2c_backend_neutral_effect.json");
    std::fs::write(&receipt_path, receipt).expect("write RG2c receipt");
    println!("RG2c receipt: {}", receipt_path.display());
}
