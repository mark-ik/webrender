// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! RG3 receipt: graph sampling into an initialized imported color target.

use std::collections::HashMap;

use crate::external_texture::{
    ExternalTexturePlacement, build_external_texture_pipeline, external_texture_commands,
};
use netrender_device::render_graph::{ImageLoad, ImageUse, RenderGraph};
use crate::{NetrenderOptions, boot, create_netrender_instance};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const WIDTH: u32 = 4;
const HEIGHT: u32 = 4;

fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &'static str,
    usage: wgpu::TextureUsages,
    bytes: &[u8],
) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(WIDTH * 4),
            rows_per_image: Some(HEIGHT),
        },
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
    );
    texture
}

fn pixel(bytes: &[u8], x: u32, y: u32) -> [u8; 4] {
    let index = ((y * WIDTH + x) * 4) as usize;
    bytes[index..index + 4].try_into().expect("rgba pixel")
}

#[test]
fn rg3_imported_output_load_preserves_untouched_pixels() {
    let _gpu_guard = super::gpu_test_guard();
    let handles = boot().expect("wgpu boot");
    let device = handles.device.clone();
    let queue = handles.queue.clone();
    let renderer = create_netrender_instance(handles, NetrenderOptions::default())
        .expect("create netrender instance");

    let source_bytes: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|_| [0u8, 0, 255, 255])
        .collect();
    let target_bytes: Vec<u8> = (0..WIDTH * HEIGHT)
        .flat_map(|_| [16u8, 32, 48, 255])
        .collect();
    let source = upload_texture(
        &device,
        &queue,
        "rg3 source",
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        &source_bytes,
    );
    let target = upload_texture(
        &device,
        &queue,
        "rg3 initialized imported target",
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC,
        &target_bytes,
    );

    let pipe = build_external_texture_pipeline(&device, FORMAT);
    let placement = ExternalTexturePlacement::new([0.0, 0.0, 2.0, HEIGHT as f32]);
    let mut graph = RenderGraph::new();
    let source_node = graph.import_image(
        "rg3 source",
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        FORMAT,
    );
    let target_node = graph.import_image(
        "rg3 initialized imported target",
        wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        FORMAT,
    );
    graph
        .add_plan_task(
            "rg3 sampled import composite",
            vec![ImageUse::sampled_read(source_node)],
            ImageUse::color_attachment(target_node, ImageLoad::Load),
            Box::new(move |device, inputs| {
                assert_eq!(inputs.len(), 1);
                external_texture_commands(device, &pipe, &inputs[0], WIDTH, HEIGHT, placement)
                    .expect("nonempty RG3 placement")
            }),
        )
        .unwrap();
    let plan = graph.compile(&[target_node]).unwrap();
    let mut imported = HashMap::new();
    imported.insert(source_node, source);
    imported.insert(target_node, target);
    let (outputs, _) = plan.execute(&device, &queue, imported).unwrap();
    let output = outputs
        .get(&target_node)
        .expect("requested imported output");
    let actual = renderer
        .wgpu_device
        .read_rgba8_texture(output, WIDTH, HEIGHT);

    for y in 0..HEIGHT {
        assert_eq!(pixel(&actual, 0, y), [0, 0, 255, 255]);
        assert_eq!(pixel(&actual, 1, y), [0, 0, 255, 255]);
        assert_eq!(pixel(&actual, 2, y), [16, 32, 48, 255]);
        assert_eq!(pixel(&actual, 3, y), [16, 32, 48, 255]);
    }
}
