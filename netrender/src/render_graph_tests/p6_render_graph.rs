// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Phase 6 receipts — typed image-plan graph + separable Gaussian blur.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::filter::{blur_pass_callback, make_bilinear_sampler};
use netrender_device::render_graph::{ImageLoad, ImageUse, RenderGraph, TransientImageDesc};
use crate::{ColorLoad, ImageKey, NO_CLIP, NetrenderOptions, Scene, boot, create_netrender_instance};

const DIM: u32 = 64;
const BLUR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn oracle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("oracle")
        .join("p6")
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("create oracle/p6 dir");
    let file = std::fs::File::create(path)
        .unwrap_or_else(|e| panic!("creating {}: {}", path.display(), e));
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png pixels");
}

fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let file =
        std::fs::File::open(path).unwrap_or_else(|e| panic!("opening {}: {}", path.display(), e));
    let dec = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = dec.read_info().expect("png read_info");
    let info = reader.info();
    assert_eq!(info.color_type, png::ColorType::Rgba);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    let (w, h) = (info.width, info.height);
    let mut buf = vec![0u8; reader.output_buffer_size()];
    reader.next_frame(&mut buf).expect("png decode");
    (w, h, buf)
}

fn should_regen() -> bool {
    std::env::var("NETRENDER_REGEN").is_ok_and(|v| v == "1")
}

fn upload_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    bytes: &[u8],
) -> wgpu::Texture {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("p6 source"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: BLUR_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    tex
}

fn blur_plan(
    renderer: &crate::Renderer,
    src: wgpu::Texture,
    extent: wgpu::Extent3d,
    step: f32,
) -> wgpu::Texture {
    let device = renderer.wgpu_device.core.device.clone();
    let queue = renderer.wgpu_device.core.queue.clone();
    let pipe = renderer.wgpu_device.ensure_brush_blur(BLUR_FORMAT);
    let sampler = make_bilinear_sampler(&device);
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
        | wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_SRC;

    let mut graph = RenderGraph::new();
    let input = graph.import_image("p6 source", extent, BLUR_FORMAT);
    let blur_h = graph.transient_image(TransientImageDesc {
        size: extent,
        format: BLUR_FORMAT,
        usage,
        label: Some("p6 blur horizontal".into()),
    });
    let blur_v = graph.transient_image(TransientImageDesc {
        size: extent,
        format: BLUR_FORMAT,
        usage,
        label: Some("p6 blur vertical".into()),
    });
    graph
        .add_plan_task(
            "p6 blur horizontal",
            vec![ImageUse::sampled_read(input)],
            ImageUse::color_attachment(blur_h, ImageLoad::Clear),
            blur_pass_callback(pipe.clone(), Arc::clone(&sampler), step, 0.0),
        )
        .unwrap();
    graph
        .add_plan_task(
            "p6 blur vertical",
            vec![ImageUse::sampled_read(blur_h)],
            ImageUse::color_attachment(blur_v, ImageLoad::Clear),
            blur_pass_callback(pipe, sampler, 0.0, step),
        )
        .unwrap();

    let mut imported = HashMap::new();
    imported.insert(input, src);
    let plan = graph.compile(&[blur_v]).unwrap();
    let (mut outputs, _) = plan.execute(&device, &queue, imported).unwrap();
    outputs.remove(&blur_v).unwrap()
}

#[test]
fn p6_01_blur_uniform_source() {
    let _gpu_guard = super::gpu_test_guard();
    let handles = boot().expect("wgpu boot");
    let device = handles.device.clone();
    let queue = handles.queue.clone();
    let renderer = create_netrender_instance(handles, NetrenderOptions::default()).unwrap();
    let src_bytes: Vec<u8> = (0..DIM * DIM).flat_map(|_| [255u8, 0, 0, 255]).collect();
    let src_tex = upload_rgba8(&device, &queue, DIM, DIM, &src_bytes);
    let blur_v = blur_plan(
        &renderer,
        src_tex,
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        1.0 / DIM as f32,
    );
    let actual = renderer.wgpu_device.read_rgba8_texture(&blur_v, DIM, DIM);
    assert_eq!(actual.len(), (DIM * DIM * 4) as usize);
    let mut max_diff: u8 = 0;
    for chunk in actual.chunks_exact(4) {
        max_diff = max_diff.max((chunk[0] as i16 - 255).unsigned_abs() as u8);
        max_diff = max_diff.max(chunk[1]);
        max_diff = max_diff.max(chunk[2]);
        max_diff = max_diff.max((chunk[3] as i16 - 255).unsigned_abs() as u8);
    }
    assert!(
        max_diff <= 2,
        "uniform source not invariant under blur: {max_diff}"
    );
}

#[test]
fn p6_02_drop_shadow() {
    let _gpu_guard = super::gpu_test_guard();
    let handles = boot().expect("wgpu boot");
    let device = handles.device.clone();
    let queue = handles.queue.clone();
    let renderer = create_netrender_instance(
        handles,
        NetrenderOptions {
            tile_cache_size: Some(64),
            enable_vello: true,
            ..Default::default()
        },
    )
    .unwrap();
    let src_bytes: Vec<u8> = (0..DIM * DIM)
        .flat_map(|i| {
            let x = (i % DIM) as i32;
            let y = (i / DIM) as i32;
            if (16..48).contains(&x) && (16..48).contains(&y) {
                [255u8, 255, 255, 255]
            } else {
                [0u8, 0, 0, 0]
            }
        })
        .collect();
    let blur_v = blur_plan(
        &renderer,
        upload_rgba8(&device, &queue, DIM, DIM, &src_bytes),
        wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        1.0 / DIM as f32,
    );
    let shadow_key: ImageKey = 0xDEAD_6666;
    renderer.insert_image_vello(shadow_key, Arc::new(blur_v));
    let mut scene = Scene::new(DIM, DIM);
    scene.push_rect(16.0, 16.0, 48.0, 48.0, [1.0, 1.0, 1.0, 1.0]);
    scene.push_image_full(
        18.0,
        18.0,
        50.0,
        50.0,
        [0.0, 0.0, 1.0, 1.0],
        [0.1, 0.1, 0.1, 0.5],
        shadow_key,
        0,
        NO_CLIP,
    );
    let target_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("p6_02 target"),
        size: wgpu::Extent3d {
            width: DIM,
            height: DIM,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: BLUR_FORMAT,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
    });
    let target_view = target_tex.create_view(&wgpu::TextureViewDescriptor {
        label: Some("p6_02 target view"),
        format: Some(BLUR_FORMAT),
        ..Default::default()
    });
    renderer.render_vello(&scene, &target_view, ColorLoad::Clear(wgpu::Color::BLACK));
    let actual = renderer
        .wgpu_device
        .read_rgba8_texture(&target_tex, DIM, DIM);
    let oracle_path = oracle_dir().join("p6_02_drop_shadow.png");
    if should_regen() {
        write_png(&oracle_path, DIM, DIM, &actual);
        return;
    }
    let (ow, oh, expected) = read_png(&oracle_path);
    assert_eq!((ow, oh), (DIM, DIM), "oracle dimensions mismatch");
    assert_eq!(actual.len(), expected.len(), "pixel buffer length mismatch");
    let mut max_diff: u8 = 0;
    let mut diff_count = 0usize;
    for (a, e) in actual.chunks_exact(4).zip(expected.chunks_exact(4)) {
        for (&av, &ev) in a.iter().zip(e.iter()) {
            let d = (av as i16 - ev as i16).unsigned_abs() as u8;
            if d > 2 {
                diff_count += 1;
            }
            max_diff = max_diff.max(d);
        }
    }
    assert_eq!(
        diff_count, 0,
        "p6_02 drop-shadow: {diff_count} channel values differ by >2 (max diff = {max_diff}); re-run with NETRENDER_REGEN=1 to update oracle"
    );
}
