// Copyright the WebRender authors (Mozilla): derived from webrender/src/renderer/init.rs under MPL-2.0.
// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Renderer construction. The embedder owns the wgpu device and
//! hands its handles in; we install a [`WgpuDevice`] over them.

use std::collections::HashMap;
use std::sync::Mutex;

use netrender_device::{WgpuDevice, WgpuHandles};

use crate::renderer::{Renderer, RendererError};
use crate::tile_cache::TileCache;
use crate::vello_tile_rasterizer::VelloTileRasterizer;

#[derive(Default)]
pub struct NetrenderOptions {
    /// Construct the renderer with an `N`-pixel-square tile cache.
    /// Required when `enable_vello = true`. `None` skips tile cache
    /// construction and produces a renderer that can still be used
    /// for direct render-graph access (e.g., running blur or clip
    /// mask tasks via `WgpuDevice` pipeline factories) but cannot
    /// drive `render_vello`.
    /// Report wgpu's bucketed limits instead of the adapter's real ones.
    ///
    /// A **host policy call**, and the reason it lives here rather than
    /// defaulting somewhere: a host that lets untrusted content reach this
    /// device wants it on, because adapter limits are a fingerprinting
    /// surface. A native app shell wants it off and its real hardware.
    /// Reach for [`NetrenderOptions::for_untrusted_content`] rather than
    /// setting the flag by hand, so the intent is greppable.
    ///
    /// It binds when the adapter is requested, so it cannot be changed
    /// without rebuilding the device and every texture on it.
    ///
    /// **It is only read on the boot paths that create the adapter.**
    /// `create_netrender_instance` receives `WgpuHandles` a host already
    /// made, so setting this alongside those handles does nothing: the
    /// adapter was requested before netrender saw it. Set it where the
    /// device is booted, or the host has to set it on its own request.
    pub apply_limit_buckets: bool,
    pub tile_cache_size: Option<u32>,
    /// Phase 7' — when `true`, eagerly construct a
    /// [`VelloTileRasterizer`] and route [`Renderer::render_vello`]
    /// through it. Requires `tile_cache_size = Some(_)`.
    pub enable_vello: bool,
    /// Roadmap A3 — when `true`, the renderer paints a translucent
    /// red wash on top of any tile that was reported dirty within
    /// the last `tile_dirty_overlay_window_frames` frames. Useful
    /// for visually debugging tile invalidation on dynamic scenes.
    /// No-op when `enable_vello = false` (overlay is rendered as
    /// part of the master vello scene composition).
    pub enable_tile_dirty_overlay: bool,
    /// Roadmap A3 — fade window for the tile-dirty overlay. Tiles
    /// dirtied within the last `N` frames stay visible; opacity
    /// decays linearly with age. `0` (default) is treated as a
    /// reasonable preset (~30 frames ≈ 0.5s at 60 Hz) when
    /// `enable_tile_dirty_overlay` is on; set to override.
    pub tile_dirty_overlay_window_frames: u32,
    /// Backend selection for the wgpu device boot. `Some(_)` forces a backend
    /// (e.g. `wgpu::Backends::DX12` so a host that imports system-WebView D3D12
    /// textures stays same-API); `None` honors `WGPU_BACKEND`, else all available.
    /// Only consulted by host boot paths that create the device from these options
    /// (e.g. `genet_winit_host::RenderCore::boot`), not embedder-supplied
    /// (`with_external`) devices.
    pub backends: Option<wgpu::Backends>,
}

impl NetrenderOptions {
    /// Options for a host that lets untrusted content reach this device.
    ///
    /// Turns limit bucketing on. Adapter limits are a fingerprinting surface,
    /// and wgpu's guidance is to decide this in trusted code rather than
    /// anywhere the content can influence, so a browsing host states it here
    /// and never takes it from a page.
    ///
    /// A native app shell should **not** use this: it gives away real
    /// hardware limits for a threat it does not have.
    #[must_use]
    pub fn for_untrusted_content() -> Self {
        Self {
            apply_limit_buckets: true,
            ..Self::default()
        }
    }
}

/// Construct a wgpu-only `Renderer`. The embedder owns the wgpu
/// device and hands the instance/adapter/device/queue handles in
/// here. The renderer fails with `WgpuFeaturesMissing(missing)` if
/// the embedder's adapter doesn't expose the features `WgpuDevice`
/// requires (Phase 0.5 demoted `REQUIRED_FEATURES` to empty, so this
/// no longer fails on a baseline adapter; the return shape is
/// preserved for later phases that re-introduce optional features).
pub fn create_netrender_instance(
    handles: WgpuHandles,
    options: NetrenderOptions,
) -> Result<Renderer, RendererError> {
    let wgpu_device =
        WgpuDevice::with_external(handles).map_err(RendererError::WgpuFeaturesMissing)?;

    let tile_cache = options
        .tile_cache_size
        .map(|size| Mutex::new(TileCache::new(size)));

    let vello_rasterizer = if options.enable_vello {
        if tile_cache.is_none() {
            return Err(RendererError::VelloRequiresTileCache);
        }
        let handles = wgpu_device.core.clone();
        let mut rast = VelloTileRasterizer::new(handles)
            .map_err(|e| RendererError::VelloInit(format!("{:?}", e)))?;
        if options.enable_tile_dirty_overlay {
            let window = if options.tile_dirty_overlay_window_frames == 0 {
                30 // default ~0.5s at 60 Hz
            } else {
                options.tile_dirty_overlay_window_frames
            };
            rast.set_dirty_overlay(true, window);
        }
        Some(Mutex::new(rast))
    } else {
        None
    };

    Ok(Renderer {
        wgpu_device,
        tile_cache,
        tile_cache_tile_size: options.tile_cache_size,
        surface_tiles: Mutex::new(Default::default()),
        vello_rasterizer,
        external_texture_pipelines: Mutex::new(HashMap::new()),
        external_images: Mutex::new(HashMap::new()),
        external_image_staging_pipeline: Mutex::new(None),
    })
}
