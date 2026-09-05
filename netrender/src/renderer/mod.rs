// Copyright the WebRender authors (Mozilla): derived from webrender/src/renderer.rs under MPL-2.0.
// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `Renderer` shell — vello-backed.
//!
//! Public entry point: [`Renderer::render_vello`]. The renderer
//! owns a [`crate::vello_tile_rasterizer::VelloTileRasterizer`]
//! (constructed at init when `NetrenderOptions::enable_vello` is
//! true) and a [`TileCache`] (constructed when
//! `NetrenderOptions::tile_cache_size` is `Some(_)`). Both must be
//! present for `render_vello` to succeed.
//!
//! The Renderer used to host a parallel batched-WGSL rasterizer
//! (`prepare()` / `render()` returning `PreparedFrame`); that path
//! was retired in favor of a single vello pipeline. The brush
//! pipeline factories on `WgpuDevice` (brush_blur, clip_rectangle)
//! are still used by render-graph tasks, but the rasterizer-side
//! brush_solid / brush_rect_solid / brush_image / brush_gradient
//! factories are now unreachable from netrender; they're slated for
//! removal from `netrender_device` in a follow-up.
//!
//! Phase mapping after the cleanup:
//!
//! - **Phase 6** (render-graph for filters / blur / clip-mask
//!   tasks) lives on, intersecting with the rasterizer via
//!   [`Renderer::insert_image_vello`] — render-graph outputs
//!   become image sources for vello scenes.
//! - **Phase 7** picture caching is now the [`TileCache`]
//!   algorithm + the per-tile `vello::Scene` cache inside
//!   `VelloTileRasterizer`.
//! - **Phase 8** gradients (linear / radial / conic / N-stop) are
//!   `peniko::Gradient` mapped from `SceneGradient`; see
//!   `vello_rasterizer::scene_to_vello`.
//! - **Phase 9** clips are vello `push_layer` shapes (axis-aligned
//!   today; arbitrary path on Phase 9' completion).

pub(crate) mod init;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use netrender_device::compositor::{Compositor, PresentedFrame};
use netrender_device::WgpuDevice;

use crate::external_texture::{
    ExternalTextureComposite, ExternalTexturePipeline, ExternalTexturePlacement,
};
use crate::render_graph::{ImageLoad, ImageUse, RenderGraph};
use crate::scene::{ImageKey, Scene};
use crate::tile_cache::TileCache;

mod filter_chain;
mod filter_passes;
mod filters;
mod rg2b;
#[allow(dead_code)]
pub(crate) mod rg2c;

pub(crate) use rg2b::RasterExecution;

use filters::{make_external_tail_target, scene_tail_fragment};

pub struct Renderer {
    pub wgpu_device: WgpuDevice,
    /// Phase 7: tile-cache invalidation algorithm. Configured via
    /// `NetrenderOptions::tile_cache_size`. Required for
    /// `render_vello` (the vello rasterizer holds its per-tile
    /// `vello::Scene` cache against this tile cache's coords).
    pub(crate) tile_cache: Option<Mutex<TileCache>>,
    /// Tile size `tile_cache` was built with; per-surface caches
    /// ([`Self::render_vello_scaled_for`]) mint theirs to match.
    pub(crate) tile_cache_tile_size: Option<u32>,
    /// Per-SURFACE tile state for hosts that rasterize several retained
    /// surfaces through one renderer (P4, shell paint plan 2026-07-03). Tile
    /// invalidation is a diff against the PREVIOUS scene, so interleaving
    /// different surfaces through the one shared `tile_cache` dirtied every
    /// tile on every call (measured: 234/234 tiles rebuilt per settled frame,
    /// and a small card render spending 90-150ms diffing against foreign tile
    /// state). Keyed by a host-chosen surface id; each entry pairs a
    /// `TileCache` with its per-tile scene store so a surface only ever diffs
    /// against itself. LRU-capped.
    pub(crate) surface_tiles: Mutex<SurfaceTiles>,
    /// Phase 7' — vello-backed tile rasterizer. Constructed at init
    /// when `NetrenderOptions::enable_vello` is true.
    pub(crate) vello_rasterizer: Option<Mutex<crate::vello_tile_rasterizer::VelloTileRasterizer>>,
    /// Per-target-format pipeline cache for zero-copy external
    /// texture overlays.
    pub(crate) external_texture_pipelines:
        Mutex<HashMap<wgpu::TextureFormat, ExternalTexturePipeline>>,
}

/// See [`Renderer::surface_tiles`]: the per-surface tile states plus a
/// monotonic use counter for LRU eviction.
#[derive(Default)]
pub(crate) struct SurfaceTiles {
    pub(crate) clock: u64,
    pub(crate) map: HashMap<u64, SurfaceTileState>,
}

impl SurfaceTiles {
    fn invalidate(&mut self, surface: u64) -> bool {
        self.map.remove(&surface).is_some()
    }
}

/// One retained surface's tile state: its own invalidation cache and the
/// per-tile `vello::Scene` store that pairs with it.
pub(crate) struct SurfaceTileState {
    pub(crate) tile_cache: TileCache,
    pub(crate) tile_scenes: HashMap<crate::tile_cache::TileCoord, vello::Scene>,
    pub(crate) last_used: u64,
}

/// Cap on distinct surfaces holding tile state. Above it the least-recently
/// used entry is evicted (its next render simply rebuilds cold). Sized for a
/// shell's surfaces plus a healthy set of card tiles.
const MAX_SURFACE_TILE_STATES: usize = 64;

/// Per-frame load policy on the color attachment for `render_vello`.
/// `Clear(c)` maps to vello's `RenderParams::base_color`. `Load` is
/// not supported on the vello path (vello always overwrites the
/// entire target); it's accepted for API compatibility and treated
/// as `Clear(transparent)`.
pub enum ColorLoad {
    Clear(wgpu::Color),
    Load,
}

/// Host-supplied metadata for one opaque tenant producer in a frame.
///
/// Netrender records these values in the returned receipt and deterministic
/// logical plan dump. It does not interpret the producer's internal resource
/// topology or submission policy.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct OpaqueTenantMetadata {
    pub tenant_name: String,
    pub producer_path: String,
    pub fallback_count: u64,
    pub scene_op_boundary: usize,
    pub placement: ExternalTexturePlacement,
    /// `None` means that the caller did not report the tenant's physical
    /// submissions. This is caller-declared metadata, not a Netrender count.
    pub caller_reported_physical_submission_count: Option<u64>,
}

impl OpaqueTenantMetadata {
    pub fn new(
        tenant_name: impl Into<String>,
        producer_path: impl Into<String>,
        fallback_count: u64,
        scene_op_boundary: usize,
        placement: ExternalTexturePlacement,
    ) -> Self {
        Self {
            tenant_name: tenant_name.into(),
            producer_path: producer_path.into(),
            fallback_count,
            scene_op_boundary,
            placement,
            caller_reported_physical_submission_count: None,
        }
    }

    pub fn with_reported_physical_submission_count(mut self, count: u64) -> Self {
        self.caller_reported_physical_submission_count = Some(count);
        self
    }
}

/// One tenant-owned, same-device physical color texture plus its frame
/// metadata. The texture handle is cloned into Netrender's private graph;
/// ownership remains with the caller's handle lifetime.
#[non_exhaustive]
pub struct OpaqueTenantInput<'a> {
    pub texture: &'a wgpu::Texture,
    pub metadata: OpaqueTenantMetadata,
}

impl<'a> OpaqueTenantInput<'a> {
    pub fn new(texture: &'a wgpu::Texture, metadata: OpaqueTenantMetadata) -> Self {
        Self { texture, metadata }
    }
}

/// Receipt for one opaque tenant graph participation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpaqueTenantReceipt {
    pub tenant_name: String,
    pub producer_path: String,
    pub fallback_count: u64,
    pub scene_op_boundary: usize,
    pub caller_reported_physical_submission_count: Option<u64>,
    pub logical_opaque_producer_boundaries: usize,
    /// Graph-only encoder count. It does not count Vello or tenant submits.
    pub graph_encoder_batches: usize,
    /// Graph-only submission count. It does not imply a single physical frame
    /// submission because opaque producers may submit independently.
    pub graph_submission_boundaries: usize,
    pub logical_plan_dump: String,
}

impl Default for ColorLoad {
    fn default() -> Self {
        Self::Clear(wgpu::Color::TRANSPARENT)
    }
}

impl Renderer {
    /// Borrow the tile cache mutex (used by tests for invalidation
    /// inspection). Returns `None` if `tile_cache_size` was `None`.
    pub fn tile_cache(&self) -> Option<&Mutex<TileCache>> {
        self.tile_cache.as_ref()
    }

    /// Retire the retained tile state for one keyed surface.
    ///
    /// Hosts must call this when a surface's document or other content source
    /// is replaced in place. Its next keyed render then rebuilds from the full
    /// scene instead of reusing tile scenes from the previous content source.
    pub fn invalidate_surface_tiles(&self, surface: u64) -> bool {
        self.surface_tiles
            .lock()
            .expect("surface_tiles lock")
            .invalidate(surface)
    }
    /// Register a GPU-resident wgpu texture as an image source for
    /// subsequent `render_vello` calls under the given `ImageKey`.
    /// Render-graph outputs (blur results, mask coverage textures,
    /// etc.) become addressable from within a vello scene's
    /// `SceneImage` primitives via this entry point.
    ///
    /// The texture is cloned (cheap — `wgpu::Texture` is internally
    /// Arc-shared) and handed to `vello::Renderer::register_texture`
    /// (Path B from rasterizer plan §3.5). Entries persist across
    /// `render_vello` calls until `unregister_image_vello` is
    /// called or the renderer is dropped. Overrides win over
    /// `scene.image_sources` entries with the same `ImageKey`.
    ///
    /// # Panics
    ///
    /// If `enable_vello` was false at construction.
    pub fn insert_image_vello(&self, key: ImageKey, texture: Arc<wgpu::Texture>) {
        let rast_mutex = self
            .vello_rasterizer
            .as_ref()
            .expect("Renderer::insert_image_vello requires enable_vello = true");
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        rast.register_texture(key, (*texture).clone());
    }

    /// Drop a previously-registered `insert_image_vello` entry.
    /// No-op if `key` was never registered or `enable_vello` is
    /// false.
    pub fn unregister_image_vello(&self, key: ImageKey) {
        let Some(rast_mutex) = self.vello_rasterizer.as_ref() else {
            return;
        };
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        rast.unregister_texture(key);
    }

    /// Number of tiles whose `vello::Scene`s were rebuilt during
    /// the most recent `render_vello` call. `0` after a no-op
    /// frame (unchanged scene). Returns `None` if `enable_vello`
    /// was false.
    pub fn vello_last_dirty_count(&self) -> Option<usize> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.last_dirty_count())
    }

    /// Number of tile-Scenes currently held in the vello
    /// rasterizer's cache. Returns `None` if `enable_vello` was
    /// false.
    pub fn vello_cached_tile_count(&self) -> Option<usize> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.cached_tile_count())
    }

    /// Roadmap E4 — register a retained fragment. The returned id is
    /// stable until `remove_fragment`; place it per frame with
    /// [`crate::scene::Scene::place_fragment`]. The fragment's
    /// lowering is cached across frames, so placement-only changes
    /// (pan / scroll / drag) cost an append, not a re-lower.
    /// Returns `None` if `enable_vello` was false.
    pub fn register_fragment(&self, fragment: crate::scene::SceneFragment) -> Option<u64> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.retained.register(fragment))
    }

    /// Roadmap E4 — replace a retained fragment's content. Bumps its
    /// generation, so the next frame that places it re-lowers.
    /// Returns `Some(false)` if the id is unknown, `None` if
    /// `enable_vello` was false.
    pub fn update_fragment(&self, id: u64, fragment: crate::scene::SceneFragment) -> Option<bool> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.retained.update(id, fragment))
    }

    /// Roadmap E4 — drop a retained fragment. Placing a removed id
    /// warns and paints nothing. Returns `Some(false)` if the id is
    /// unknown, `None` if `enable_vello` was false.
    pub fn remove_fragment(&self, id: u64) -> Option<bool> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.retained.remove(id))
    }

    /// Roadmap E4 receipt — how many times any fragment has been
    /// lowered since construction. Flat across placement-only frames;
    /// grows by one per content generation actually placed.
    pub fn fragment_lower_count(&self) -> Option<u64> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.retained.lower_count())
    }

    /// Roadmap E4 receipt — how many frames reused the cached master
    /// wholesale (nothing changed at all, not even placement).
    pub fn fragment_master_hits(&self) -> Option<u64> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.retained.master_hits())
    }
    /// `target_view`'s texture dimensions are the render size — pass a full
    /// mip-0 view of a viewport-sized texture.
    ///
    /// # Panics
    ///
    /// - If `enable_vello` was false at construction.
    /// - If `tile_cache_size` was `None` at construction.
    /// - If a vello render error occurs (mirrors the existing
    ///   `render()` shape, which doesn't return a Result).
    pub fn render_vello(&self, scene: &Scene, target_view: &wgpu::TextureView, clear: ColorLoad) {
        self.render_vello_scaled(scene, target_view, clear, 1.0);
    }

    /// Like [`render_vello`](Self::render_vello) but rasterizes a logical-coord scene
    /// into a `scale`×-larger target via a root scale affine — the device-pixel-ratio
    /// path for crisp content on HiDPI displays. The `target_view` must be a full mip-0
    /// view of a texture `scale`× the scene's viewport; **its dimensions are the render
    /// size**, so a host laying out at a truncated `physical / scale` still fills every
    /// row. (Auto-DPI D2.)
    pub fn render_vello_scaled(
        &self,
        scene: &Scene,
        target_view: &wgpu::TextureView,
        clear: ColorLoad,
        scale: f32,
    ) {
        let rast_mutex = self
            .vello_rasterizer
            .as_ref()
            .expect("Renderer::render_vello requires NetrenderOptions::enable_vello = true");
        let tc_mutex = self
            .tile_cache
            .as_ref()
            .expect("Renderer::render_vello requires NetrenderOptions::tile_cache_size = Some(_)");

        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        let mut tc = tc_mutex.lock().expect("tile_cache lock");
        // Legacy shared path: the rasterizer's own tile-scene store rides with
        // the shared tile cache (one-surface hosts; multi-surface hosts should
        // use `render_vello_scaled_for` so surfaces stop cross-invalidating).
        let mut tile_scenes = std::mem::take(rast.tile_scenes_mut());
        self.render_vello_inner(
            &mut rast,
            &mut tc,
            &mut tile_scenes,
            scene,
            target_view,
            clear,
            scale,
        );
        *rast.tile_scenes_mut() = tile_scenes;
    }

    /// Like [`render_vello_scaled`](Self::render_vello_scaled) but with
    /// per-SURFACE tile state: `surface` names one retained surface (a shell
    /// partition texture, a canvas, one content card), and its scene diffs
    /// only against ITS OWN previous frame. Hosts that rasterize several
    /// surfaces through one renderer must use this — through the shared-cache
    /// entry every call diffs against a different surface's scene, so every
    /// tile is dirty every call (see [`Self::surface_tiles`]).
    pub fn render_vello_scaled_for(
        &self,
        surface: u64,
        scene: &Scene,
        target_view: &wgpu::TextureView,
        clear: ColorLoad,
        scale: f32,
    ) {
        let rast_mutex = self
            .vello_rasterizer
            .as_ref()
            .expect("Renderer::render_vello_scaled_for requires NetrenderOptions::enable_vello");
        let tile_size = self.tile_cache_tile_size.expect(
            "Renderer::render_vello_scaled_for requires NetrenderOptions::tile_cache_size = Some(_)",
        );
        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        let mut surfaces = self.surface_tiles.lock().expect("surface_tiles lock");
        surfaces.clock += 1;
        let clock = surfaces.clock;
        if !surfaces.map.contains_key(&surface) && surfaces.map.len() >= MAX_SURFACE_TILE_STATES {
            if let Some((&evict, _)) = surfaces.map.iter().min_by_key(|(_, s)| s.last_used) {
                surfaces.map.remove(&evict);
            }
        }
        let entry = surfaces
            .map
            .entry(surface)
            .or_insert_with(|| SurfaceTileState {
                tile_cache: TileCache::new(tile_size),
                tile_scenes: HashMap::new(),
                last_used: 0,
            });
        entry.last_used = clock;
        // Destructure so the borrows of the two halves stay disjoint.
        let SurfaceTileState {
            tile_cache,
            tile_scenes,
            ..
        } = entry;
        self.render_vello_inner(
            &mut rast,
            tile_cache,
            tile_scenes,
            scene,
            target_view,
            clear,
            scale,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn render_vello_inner(
        &self,
        rast: &mut crate::vello_tile_rasterizer::VelloTileRasterizer,
        tc: &mut TileCache,
        tile_scenes: &mut HashMap<crate::tile_cache::TileCoord, vello::Scene>,
        scene: &Scene,
        target_view: &wgpu::TextureView,
        clear: ColorLoad,
        scale: f32,
    ) {
        let base = match clear {
            ColorLoad::Clear(c) => {
                vello::peniko::Color::new([c.r as f32, c.g as f32, c.b as f32, c.a as f32])
            }
            ColorLoad::Load => vello::peniko::Color::new([0.0, 0.0, 0.0, 0.0]),
        };

        // Roadmap D1 — if any layer carries a `backdrop_filter`,
        // pre-render the scene-prefix to a texture, blur it, and
        // inject a SceneImage covering the layer's bounds so the
        // layer paints over the blurred backdrop. Falls through to
        // the no-backdrop fast path when no filters are present.
        let processed = self.preprocess_filters(scene, rast, tc);
        let scene_to_render = processed.as_ref().unwrap_or(scene);

        // Per-frame paint instrumentation. DEBUG (quiet by default; the app
        // raises it) on the load-bearing render op; one structured event per
        // frame with timing + the scene size. NOTE: this is a HOT per-frame
        // path — it needs sampling later (emit every Nth frame); not added now.
        #[cfg(not(target_arch = "wasm32"))]
        use std::time::Instant;
        #[cfg(target_arch = "wasm32")]
        use web_time::Instant;
        let render_start = Instant::now();
        let result =
            rast.render_scaled_with(scene_to_render, tc, tile_scenes, target_view, base, scale);
        let elapsed_us = render_start.elapsed().as_micros();
        match result {
            Ok(()) => {
                tracing::debug!(
                    target: "netrender",
                    elapsed_us,
                    op_count = scene_to_render.ops.len(),
                    viewport_w = scene_to_render.viewport_width,
                    viewport_h = scene_to_render.viewport_height,
                    scale,
                    "frame rendered"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "netrender",
                    elapsed_us,
                    op_count = scene_to_render.ops.len(),
                    viewport_w = scene_to_render.viewport_width,
                    viewport_h = scene_to_render.viewport_height,
                    error = ?e,
                    "frame render failed"
                );
                panic!("vello render_to_texture failed: {:?}", e);
            }
        }
    }

    /// Compose a same-device external texture directly into an
    /// already-rendered target view.
    ///
    /// This is the zero-copy path for WebGL canvas / video / embedder
    /// textures that already live on this renderer's `wgpu::Device`.
    /// The source texture is sampled directly from `source_view` and
    /// blended over `target_view`; unlike `insert_image_vello`, the
    /// source texture does not need `COPY_SRC` usage and is not copied
    /// into vello's image atlas.
    ///
    /// First-slice limitation: this helper is an overlay pass. It
    /// preserves correct composition for external content that is
    /// topmost relative to the vello-rendered scene. Fully interleaved
    /// painter order requires splitting the scene around external
    /// texture ops or moving this pass into the scene compositor.
    pub fn compose_external_texture(
        &self,
        source_view: &wgpu::TextureView,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
        viewport_width: u32,
        viewport_height: u32,
        placement: ExternalTexturePlacement,
    ) {
        let pipe = self.external_texture_pipeline(target_format);

        crate::external_texture::compose_external_texture(
            &self.wgpu_device.core.device,
            &self.wgpu_device.core.queue,
            &pipe,
            source_view,
            target_view,
            viewport_width,
            viewport_height,
            placement,
        );
    }

    fn external_texture_pipeline(
        &self,
        target_format: wgpu::TextureFormat,
    ) -> ExternalTexturePipeline {
        let mut pipelines = self
            .external_texture_pipelines
            .lock()
            .expect("external_texture_pipelines lock");
        pipelines
            .entry(target_format)
            .or_insert_with(|| {
                crate::external_texture::build_external_texture_pipeline(
                    &self.wgpu_device.core.device,
                    target_format,
                )
            })
            .clone()
    }

    /// Encode a same-device external texture operation into a caller-owned
    /// encoder. Graph/executor paths use this participation seam; the public
    /// [`Self::compose_external_texture`] wrapper retains its legacy
    /// self-submitting behavior.
    pub(crate) fn encode_external_texture(
        &self,
        source_view: &wgpu::TextureView,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
        viewport_width: u32,
        viewport_height: u32,
        placement: ExternalTexturePlacement,
        encoder: &mut wgpu::CommandEncoder,
    ) -> bool {
        let pipe = self.external_texture_pipeline(target_format);
        crate::external_texture::encode_external_texture(
            &self.wgpu_device.core.device,
            &pipe,
            source_view,
            target_view,
            viewport_width,
            viewport_height,
            placement,
            encoder,
        )
    }

    fn submit_external_texture(
        &self,
        source_view: &wgpu::TextureView,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
        viewport_width: u32,
        viewport_height: u32,
        placement: ExternalTexturePlacement,
    ) {
        let mut encoder =
            self.wgpu_device
                .core
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("netrender external texture encoder"),
                });
        self.encode_external_texture(
            source_view,
            target_view,
            target_format,
            viewport_width,
            viewport_height,
            placement,
            &mut encoder,
        );
        self.wgpu_device.core.queue.submit([encoder.finish()]);
    }

    /// Render one opaque tenant through a private graph task, then hand the
    /// resulting master texture to the compositor. The tenant's physical
    /// texture is imported as a sampled input; the initialized Vello master is
    /// imported as the single load-preserving color output.
    ///
    /// The existing Vello render and tail redraw remain intact. The returned
    /// receipt describes the logical opaque producer boundary separately from
    /// the caller-reported physical submission count. This first slice uses
    /// the existing full-master plus tail-redraw behavior for one tenant; more
    /// general interleaving remains outside this method.
    ///
    /// Invalid graph admission or execution panics consistently with the
    /// existing renderer methods. Host error disposition is a later RG3b
    /// concern.
    pub fn render_with_opaque_tenant(
        &self,
        scene: &Scene,
        master_format: wgpu::TextureFormat,
        compositor: &mut dyn Compositor,
        base_color: vello::peniko::Color,
        tenant: &OpaqueTenantInput<'_>,
    ) -> OpaqueTenantReceipt {
        let rast_mutex = self.vello_rasterizer.as_ref().expect(
            "Renderer::render_with_opaque_tenant requires NetrenderOptions::enable_vello = true",
        );
        let tc_mutex = self.tile_cache.as_ref().expect(
            "Renderer::render_with_opaque_tenant requires NetrenderOptions::tile_cache_size = Some(_) ",
        );

        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        let mut tc = tc_mutex.lock().expect("tile_cache lock");
        let processed = self.preprocess_filters(scene, &mut rast, &mut tc);
        let render_scene = processed.as_ref().unwrap_or(scene);

        rast.render_to_internal_master(render_scene, &mut tc, master_format, base_color)
            .unwrap_or_else(|e| panic!("vello render_to_texture failed: {:?}", e));

        let master_texture = rast
            .master_texture()
            .expect("master pool guaranteed by render_to_internal_master above")
            .clone();
        let size = wgpu::Extent3d {
            width: scene.viewport_width,
            height: scene.viewport_height,
            depth_or_array_layers: 1,
        };
        let mut graph = RenderGraph::new();
        let tenant_image = graph.import_image(
            tenant.metadata.tenant_name.clone(),
            tenant.texture.size(),
            tenant.texture.format(),
        );
        let master_image = graph.import_image("netrender initialized master", size, master_format);
        let pipe = self.external_texture_pipeline(master_format);
        let placement = tenant.metadata.placement;
        graph
            .add_plan_task(
                "opaque tenant composite",
                vec![ImageUse::sampled_read(tenant_image)],
                ImageUse::color_attachment(master_image, ImageLoad::Load),
                Box::new(move |device, encoder, inputs, output| {
                    assert_eq!(inputs.len(), 1);
                    assert!(crate::external_texture::encode_external_texture(
                        device,
                        &pipe,
                        &inputs[0],
                        output,
                        size.width,
                        size.height,
                        placement,
                        encoder,
                    ));
                }),
            )
            .expect("opaque tenant graph task admission");
        let plan = graph
            .compile(&[master_image])
            .expect("opaque tenant graph compilation")
            .with_raster_execution(crate::renderer::RasterExecution::classic());
        let graph_dump = plan.dump();
        let mut imported = HashMap::new();
        imported.insert(tenant_image, tenant.texture.clone());
        imported.insert(master_image, master_texture.clone());
        plan.execute(
            &self.wgpu_device.core.device,
            &self.wgpu_device.core.queue,
            imported,
        )
        .expect("opaque tenant graph execution");

        let boundary = tenant.metadata.scene_op_boundary.min(scene.ops.len());
        if boundary < scene.ops.len() {
            let tail_scene = scene_tail_fragment(scene, boundary);
            if !tail_scene.ops.is_empty() {
                let (_, tail_view) = make_external_tail_target(
                    &self.wgpu_device.core.device,
                    scene.viewport_width,
                    scene.viewport_height,
                    master_format,
                );
                rast.render_overlay_fragment(
                    &tail_scene,
                    &tail_view,
                    vello::peniko::Color::new([0.0, 0.0, 0.0, 0.0]),
                )
                .unwrap_or_else(|e| panic!("vello overlay tail render failed: {:?}", e));
                let master_view =
                    master_texture.create_view(&wgpu::TextureViewDescriptor::default());
                self.submit_external_texture(
                    &tail_view,
                    &master_view,
                    master_format,
                    scene.viewport_width,
                    scene.viewport_height,
                    ExternalTexturePlacement::new([
                        0.0,
                        0.0,
                        scene.viewport_width as f32,
                        scene.viewport_height as f32,
                    ]),
                );
            }
        }

        let (declares, destroys) = rast.diff_compositor_surfaces(scene);
        for key in &destroys {
            compositor.destroy_surface(*key);
        }
        for (key, bounds) in &declares {
            compositor.declare_surface(*key, *bounds);
        }
        let layers = rast.build_layer_presents(scene, &tc);
        let handles = rast.handles_ref();
        compositor.present_frame(PresentedFrame {
            master: &master_texture,
            handles,
            layers: &layers,
        });
        rast.commit_compositor_state(scene);

        let metadata = &tenant.metadata;
        OpaqueTenantReceipt {
            tenant_name: metadata.tenant_name.clone(),
            producer_path: metadata.producer_path.clone(),
            fallback_count: metadata.fallback_count,
            scene_op_boundary: boundary,
            caller_reported_physical_submission_count:
                metadata.caller_reported_physical_submission_count,
            logical_opaque_producer_boundaries: 1,
            graph_encoder_batches: 1,
            graph_submission_boundaries: 1,
            logical_plan_dump: format!(
                "opaque_tenant tenant_name={:?} producer_path={:?} fallback_count={} scene_op_boundary={} caller_reported_physical_submission_count={:?}\n{}",
                metadata.tenant_name,
                metadata.producer_path,
                metadata.fallback_count,
                boundary,
                metadata.caller_reported_physical_submission_count,
                graph_dump,
            ),
        }
    }
    /// Number of times the path-(b′) master-texture pool has
    /// allocated a fresh `wgpu::Texture` over this Renderer's
    /// lifetime. Returns `None` if `enable_vello` was false.
    ///
    /// Test signal: stable across consecutive `render_with_compositor`
    /// calls at the same viewport / format; increments on resize or
    /// format change.
    pub fn vello_master_allocations(&self) -> Option<usize> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        Some(rast.master_allocations())
    }

    /// Roadmap A4 — return the per-phase timings captured by the
    /// most recent `render_vello` / `render_with_compositor` /
    /// `compose_into` call. Returns `None` if `enable_vello` was
    /// false or no timed render has run yet.
    ///
    /// Spans currently captured:
    ///
    /// - `refresh_image_data` — Path A image cache refresh.
    /// - `tile_invalidate` — `TileCache::invalidate(scene)`.
    /// - `dirty_tile_rebuild` — per-dirty-tile filter + WGSL-style
    ///   translation into per-tile vello scenes.
    /// - `master_compose` — building the master `vello::Scene`.
    /// - `vello_render` (only on render paths that submit to GPU)
    ///   — `vello::Renderer::render_to_texture`.
    /// - `master_append` (only on `compose_into`) —
    ///   `vello::Scene::append`.
    ///
    /// Plus a `total` wall-clock duration on `FrameTimings` itself.
    pub fn last_frame_timings(&self) -> Option<crate::profiling::FrameTimings> {
        let rast_mutex = self.vello_rasterizer.as_ref()?;
        let rast = rast_mutex.lock().expect("vello_rasterizer lock");
        rast.last_timings().cloned()
    }

    /// Path (b′) entry point — render `scene` into an internal
    /// master texture (pool-allocated by `(width, height,
    /// master_format)` on the rasterizer), forward declare/destroy
    /// surface lifecycle events to `compositor`, then hand the
    /// master texture and per-surface `LayerPresent` payload to the
    /// consumer via [`Compositor::present_frame`].
    ///
    /// Per-frame ordering:
    /// 1. Render scene to internal master.
    /// 2. Compute the surface diff against last frame's state.
    /// 3. Emit `destroy_surface` for keys present last frame but
    ///    absent now; emit `declare_surface` for new + bounds-
    ///    changed keys (idempotent per the trait contract).
    /// 4. Build per-surface `LayerPresent` with the four-source
    ///    dirty OR (tile-intersection / absent-last-frame /
    ///    bounds-changed) computed inline.
    /// 5. Call `present_frame` so the consumer can blit dirty
    ///    surface regions and route native textures to the OS.
    /// 6. Commit the frame's surface state to the rasterizer for
    ///    next-frame diff.
    ///
    /// `master_format` must match the format of the consumer-owned
    /// destination textures: `copy_texture_to_texture` requires
    /// identical formats. `wgpu::TextureFormat::Rgba8Unorm` is the
    /// graphshell-shaped consumer default. See design doc §8(1) for
    /// the BGRA-storage caveat on native-compositor paths.
    ///
    /// See
    /// [`netrender-notes/2026-05-05_compositor_handoff_path_b_prime.md`](../../netrender-notes/2026-05-05_compositor_handoff_path_b_prime.md)
    /// for the design.
    ///
    /// # Panics
    ///
    /// - If `enable_vello` was false at construction.
    /// - If `tile_cache_size` was `None` at construction.
    /// - If a vello render error occurs.
    pub fn render_with_compositor(
        &self,
        scene: &Scene,
        master_format: wgpu::TextureFormat,
        compositor: &mut dyn Compositor,
        base_color: vello::peniko::Color,
    ) {
        self.render_with_compositor_and_external_textures(
            scene,
            master_format,
            compositor,
            base_color,
            &[],
        );
    }

    /// Render a scene into the compositor master texture, then blend
    /// same-device external textures into that master before handing the
    /// frame to the consumer compositor.
    ///
    /// `ExternalTextureComposite::scene_op_boundary` preserves painter
    /// order without routing producer textures through Vello's atlas:
    /// ordinary scene content paints once into the master, each external
    /// texture composites at its boundary, and the ordinary scene tail
    /// that should remain above that texture is redrawn into a transparent
    /// scratch target and blended back over the master. Callers that keep
    /// the default `usize::MAX` boundary retain the topmost-overlay fast
    /// path and pay no tail redraw.
    pub fn render_with_compositor_and_external_textures(
        &self,
        scene: &Scene,
        master_format: wgpu::TextureFormat,
        compositor: &mut dyn Compositor,
        base_color: vello::peniko::Color,
        external_textures: &[ExternalTextureComposite<'_>],
    ) {
        let rast_mutex = self.vello_rasterizer.as_ref().expect(
            "Renderer::render_with_compositor requires NetrenderOptions::enable_vello = true",
        );
        let tc_mutex = self.tile_cache.as_ref().expect(
            "Renderer::render_with_compositor requires NetrenderOptions::tile_cache_size = Some(_)",
        );

        let mut rast = rast_mutex.lock().expect("vello_rasterizer lock");
        let mut tc = tc_mutex.lock().expect("tile_cache lock");

        // Apply backdrop + element CSS `filter` preprocessing (same as the
        // `render_vello` path). External-texture boundaries below still index the
        // original `scene` (filters + interleaved external textures is a
        // follow-up); the common no-external-texture path is unaffected.
        let processed = self.preprocess_filters(scene, &mut rast, &mut tc);
        let render_scene = processed.as_ref().unwrap_or(scene);

        // 1. Render the scene into the rasterizer's pool-allocated master.
        rast.render_to_internal_master(render_scene, &mut tc, master_format, base_color)
            .unwrap_or_else(|e| panic!("vello render_to_texture failed: {:?}", e));

        if !external_textures.is_empty() {
            let master_texture = rast
                .master_texture()
                .expect("master_pool guaranteed by render_to_internal_master above");
            let master_view = master_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let mut tail_target: Option<(wgpu::Texture, wgpu::TextureView)> = None;
            let mut previous_boundary = 0usize;
            for external in external_textures {
                let boundary = external.scene_op_boundary.min(scene.ops.len());
                debug_assert!(
                    boundary >= previous_boundary,
                    "external textures must be supplied in nondecreasing scene-op order",
                );
                previous_boundary = boundary;

                self.submit_external_texture(
                    external.source_view,
                    &master_view,
                    master_format,
                    scene.viewport_width,
                    scene.viewport_height,
                    external.placement,
                );

                if boundary >= scene.ops.len() {
                    continue;
                }

                let tail_scene = scene_tail_fragment(scene, boundary);
                if tail_scene.ops.is_empty() {
                    continue;
                }

                let (_, tail_view) = tail_target.get_or_insert_with(|| {
                    make_external_tail_target(
                        &self.wgpu_device.core.device,
                        scene.viewport_width,
                        scene.viewport_height,
                        master_format,
                    )
                });
                rast.render_overlay_fragment(
                    &tail_scene,
                    tail_view,
                    vello::peniko::Color::new([0.0, 0.0, 0.0, 0.0]),
                )
                .unwrap_or_else(|e| panic!("vello overlay tail render failed: {:?}", e));
                self.submit_external_texture(
                    tail_view,
                    &master_view,
                    master_format,
                    scene.viewport_width,
                    scene.viewport_height,
                    ExternalTexturePlacement::new([
                        0.0,
                        0.0,
                        scene.viewport_width as f32,
                        scene.viewport_height as f32,
                    ]),
                );
            }
        }

        // 2. Diff surface lifecycle against last frame.
        let (declares, destroys) = rast.diff_compositor_surfaces(scene);

        // 3. Forward lifecycle events. Destroys first so consumer can
        // free old destination textures before any new declares
        // potentially reuse keys (re-declare with same key after
        // destroy is a valid pattern, though the diff doesn't currently
        // emit that case — declares only fire when bounds differ).
        for key in &destroys {
            compositor.destroy_surface(*key);
        }
        for (key, bounds) in &declares {
            compositor.declare_surface(*key, *bounds);
        }

        // 4. Build LayerPresent vec.
        let layers = rast.build_layer_presents(scene, &tc);

        // 5. Hand off. Re-borrow master + handles after lifecycle calls
        // (which used &self) so the &mut self borrow for the present
        // payload is fresh.
        let master_texture = rast
            .master_texture()
            .expect("master_pool guaranteed by render_to_internal_master above");
        let handles = rast.handles_ref();
        compositor.present_frame(PresentedFrame {
            master: master_texture,
            handles,
            layers: &layers,
        });

        // 6. Persist surface state for next frame.
        rast.commit_compositor_state(scene);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidating_a_surface_drops_only_its_retained_tile_state() {
        let state = || SurfaceTileState {
            tile_cache: TileCache::new(64),
            tile_scenes: HashMap::new(),
            last_used: 1,
        };
        let mut surfaces = SurfaceTiles::default();
        surfaces.map.insert(7, state());
        surfaces.map.insert(9, state());

        assert!(surfaces.invalidate(7));
        assert!(!surfaces.map.contains_key(&7));
        assert!(surfaces.map.contains_key(&9));
        assert!(!surfaces.invalidate(7));
    }
}

#[derive(Debug)]
pub enum RendererError {
    WgpuFeaturesMissing(wgpu::Features),
    /// `NetrenderOptions::enable_vello = true` requires
    /// `tile_cache_size = Some(_)`. The vello rasterizer holds the
    /// per-tile `vello::Scene` cache against the tile cache's
    /// coords; without a tile cache there's nothing for it to cache
    /// against.
    VelloRequiresTileCache,
    /// `vello::Renderer` construction failed during
    /// `create_netrender_instance`. The wrapped string is vello's
    /// error formatted via `{:?}` (vello::Error doesn't implement
    /// `std::error::Error` in 0.8 — the string is informational
    /// only).
    VelloInit(String),
}
