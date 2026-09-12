// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Phase 7' — vello-backed tile cache (Masonry pattern per
//! rasterizer plan §2.1-§2.4).
//!
//! Replaces the parent-plan Phase 7 architecture
//! (`Tile.texture: Option<Arc<wgpu::Texture>>` per tile, one
//! `brush_image_alpha` composite draw per tile) with:
//!
//! - One [`vello::Renderer`] for the lifetime of this struct.
//! - A per-tile [`vello::Scene`] cache keyed by [`TileCoord`].
//! - One `Scene::append` of every cached tile-Scene into a master
//!   frame Scene per render.
//! - One [`vello::Renderer::render_to_texture`] per frame, one submit.
//!
//! `TileCache` keeps its existing job — frame-stamp invalidation,
//! dependency hashing, retain heuristic. The rasterizer holds the
//! GPU-side cache; tile_cache stays rasterizer-agnostic.
//!
//! ## Per-tile clipping at compose time
//!
//! A primitive whose AABB intersects multiple tiles ends up in each
//! tile's filtered Scene. Without clipping, vello would rasterize
//! the same primitive once per tile and the overlapping pixels
//! would be drawn N times — wasteful and incorrect for non-opaque
//! primitives. We solve this by wrapping each tile-Scene's
//! `Scene::append` in `push_layer(tile_world_rect)` /
//! `pop_layer` at compose time. Each tile's draws are clipped to
//! its own world rect; spanning primitives draw correctly without
//! over-rendering.
//!
//! ## Image cache
//!
//! Images uploaded via `scene.image_sources` are converted to
//! `peniko::ImageData` once per `ImageKey` and reused across
//! frames. Vello's internal image atlas dedups by `Blob.id()`, so
//! the same `Arc<Vec<u8>>` re-handed each frame is one upload,
//! not N. New keys are built on first sight; keys that disappear
//! from `scene.image_sources` are evicted.

use std::collections::{HashMap, HashSet};
use vello::{
    AaConfig, AaSupport, RenderParams, Renderer, RendererOptions,
    peniko::{Color, ImageData},
};

use netrender_device::compositor::{LayerPresent, SurfaceKey};
use netrender_device::WgpuHandles;

use crate::scene::{ImageKey, Scene};
use crate::tile_cache::{TileCache, TileCoord, aabb_intersects};
use crate::vello_rasterizer::scene_to_vello_with_overrides;

mod master;
mod retained;

/// Path (b′) per-frame state held across `render_with_compositor`
/// calls. Used to compute the four-source dirty OR for declared
/// compositor surfaces (tile-intersection / newly-declared /
/// bounds-changed / absent-last-frame).
#[derive(Default)]
struct CompositorState {
    seen_last_frame: HashSet<SurfaceKey>,
    prev_bounds: HashMap<SurfaceKey, [f32; 4]>,
}

/// One vello-backed tile rasterizer. Owns the vello::Renderer, the
/// per-tile vello::Scene cache, and the per-frame peniko image data
/// cache. See module docs.
pub struct VelloTileRasterizer {
    handles: WgpuHandles,
    vello_renderer: Renderer,
    tile_scenes: HashMap<TileCoord, vello::Scene>,
    /// Persistent image data built from `scene.image_sources` (Path
    /// A, CPU bytes). Each entry holds an `Arc<Vec<u8>>` (via
    /// `peniko::Blob`) that lives across frames so vello's
    /// `Blob::id()` dedup keeps the GPU upload to once per
    /// `ImageKey`. Entries are added on first sight of a key and
    /// evicted when the key disappears from `scene.image_sources`.
    image_data: HashMap<ImageKey, ImageData>,
    /// Caller-registered GPU textures via `register_texture` (Path B).
    /// Persists across frames; entries survive until the texture is
    /// explicitly unregistered or the rasterizer is dropped.
    image_overrides: HashMap<ImageKey, ImageData>,
    last_dirty_count: usize,
    /// Retained from the most recent `tile_cache.invalidate(scene)`
    /// call, used by `build_layer_presents` to compute per-surface
    /// tile-intersection dirty bits. Cleared back to empty by
    /// `build_master_scene_timed` on each frame before being repopulated.
    last_dirty_tiles: Vec<TileCoord>,
    /// Path (b′) compositor handoff: cached internal master texture,
    /// reused frame-to-frame when `(width, height, format)` matches.
    /// Reallocated on viewport resize or format change. `None` until
    /// the first `render_to_internal_master` call.
    master_pool: Option<MasterEntry>,
    /// Allocation counter for the master texture pool (test signal).
    /// Increments on each fresh allocation; stable when the pool
    /// reuses the cached texture across frames.
    master_allocations: usize,
    /// Per-surface state across frames for the four-source dirty OR.
    compositor_state: CompositorState,
    /// Roadmap A3 — when true, `compose_master` appends a translucent
    /// red wash for tiles dirtied within `dirty_overlay_window_frames`.
    dirty_overlay_enabled: bool,
    /// Roadmap A3 — fade window in frames; opacity decays linearly
    /// from `OVERLAY_PEAK_ALPHA` at `age = 0` to `0` at `age = window`.
    dirty_overlay_window_frames: u32,
    /// Roadmap A4 — most recent frame's per-phase timings, captured
    /// in `render` / `render_to_internal_master` / `compose_into`.
    /// Cleared back to `None` on `clear_last_timings`.
    last_timings: Option<crate::profiling::FrameTimings>,
    /// Roadmap E4 — retained-fragment state (registry, cached master,
    /// receipt counters). See [`retained`].
    pub(crate) retained: retained::RetainedState,
}

struct MasterEntry {
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    texture: wgpu::Texture,
}

impl VelloTileRasterizer {
    /// Construct a rasterizer over the given wgpu device. Boots a
    /// fresh `vello::Renderer` immediately; subsequent renders reuse
    /// it. Returns an error if vello pipeline construction fails.
    pub fn new(handles: WgpuHandles) -> Result<Self, vello::Error> {
        let vello_renderer = Renderer::new(
            &handles.device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: AaSupport::area_only(),
                num_init_threads: None,
                pipeline_cache: None,
            },
        )?;
        Ok(Self {
            handles,
            vello_renderer,
            tile_scenes: HashMap::new(),
            image_data: HashMap::new(),
            image_overrides: HashMap::new(),
            last_dirty_count: 0,
            last_dirty_tiles: Vec::new(),
            master_pool: None,
            master_allocations: 0,
            compositor_state: CompositorState::default(),
            dirty_overlay_enabled: false,
            dirty_overlay_window_frames: 30,
            last_timings: None,
            retained: retained::RetainedState::default(),
        })
    }

    /// The rasterizer's own (legacy shared) per-tile scene store — the one the
    /// unkeyed render paths pair with the renderer's shared `TileCache`. The
    /// keyed per-surface path owns its stores instead
    /// (`Renderer::surface_tiles`).
    pub(crate) fn tile_scenes_mut(&mut self) -> &mut HashMap<TileCoord, vello::Scene> {
        &mut self.tile_scenes
    }

    /// Roadmap A4 — return the per-phase timings captured by the most
    /// recent render call (`render` / `render_to_internal_master` /
    /// `compose_into`). `None` until the first render call returns.
    pub fn last_timings(&self) -> Option<&crate::profiling::FrameTimings> {
        self.last_timings.as_ref()
    }

    /// Roadmap A3 — toggle the tile-dirty overlay. When `enabled`,
    /// `compose_master` appends a translucent red wash on top of every
    /// tile that's been reported dirty within the last `window_frames`.
    /// `window_frames` is clamped to `>= 1` (zero would never paint).
    pub fn set_dirty_overlay(&mut self, enabled: bool, window_frames: u32) {
        self.dirty_overlay_enabled = enabled;
        self.dirty_overlay_window_frames = window_frames.max(1);
    }

    /// Roadmap A3 — read the current overlay flag (introspection helper).
    pub fn dirty_overlay_enabled(&self) -> bool {
        self.dirty_overlay_enabled
    }

    /// Roadmap A3 — read the current fade window in frames.
    pub fn dirty_overlay_window_frames(&self) -> u32 {
        self.dirty_overlay_window_frames
    }

    /// Number of times the master-texture pool allocated a fresh
    /// `wgpu::Texture` over the rasterizer's lifetime. Stays constant
    /// across consecutive `render_to_internal_master` calls at the
    /// same `(width, height, format)`; increments on viewport resize
    /// or format change.
    pub fn master_allocations(&self) -> usize {
        self.master_allocations
    }

    /// Borrow the cached master texture from the path-(b′) pool, if
    /// any. `None` until the first `render_to_internal_master` call.
    pub fn master_texture(&self) -> Option<&wgpu::Texture> {
        self.master_pool.as_ref().map(|e| &e.texture)
    }

    /// Borrow the underlying `WgpuHandles`. Used by
    /// `Renderer::render_with_compositor` to populate
    /// `PresentedFrame.handles` so the consumer can encode + submit
    /// its own GPU copies during `present_frame`.
    pub fn handles_ref(&self) -> &WgpuHandles {
        &self.handles
    }

    /// Diff `scene.compositor_surfaces` against last frame's seen
    /// state. Returns `(declares, destroys)` where:
    ///
    /// - `declares` lists `(key, bounds)` for surfaces newly added
    ///   this frame OR whose bounds changed since last frame. The
    ///   caller forwards each as a `Compositor::declare_surface`
    ///   call (idempotent on repeat keys per the trait contract).
    /// - `destroys` lists keys present last frame but absent this
    ///   frame.
    ///
    /// Pure query — does not mutate `compositor_state`. Persistence
    /// happens in [`Self::commit_compositor_state`] *after*
    /// `present_frame` returns.
    pub fn diff_compositor_surfaces(
        &self,
        scene: &Scene,
    ) -> (Vec<(SurfaceKey, [f32; 4])>, Vec<SurfaceKey>) {
        let mut declares = Vec::new();
        let mut destroys = Vec::new();

        let current_keys: HashSet<SurfaceKey> =
            scene.compositor_surfaces.iter().map(|s| s.key).collect();

        for s in &scene.compositor_surfaces {
            let prev = self.compositor_state.prev_bounds.get(&s.key).copied();
            if prev != Some(s.bounds) {
                declares.push((s.key, s.bounds));
            }
        }
        for key in &self.compositor_state.seen_last_frame {
            if !current_keys.contains(key) {
                destroys.push(*key);
            }
        }

        (declares, destroys)
    }

    /// Build the per-frame `LayerPresent` vec for `scene.compositor_surfaces`,
    /// in declaration order (vec position = z-order).
    ///
    /// `LayerPresent.dirty` ORs four sources per design doc §4:
    /// - tile-intersection: any tile in `last_dirty_tiles` intersects
    ///   the surface's bounds;
    /// - newly-declared / absent-last-frame: surface key was not in
    ///   the previous frame's seen-set;
    /// - bounds-changed: previous-frame bounds differ from current.
    ///
    /// `source_rect_in_master` clamps `surface.bounds` to the master
    /// pixel space `[0..viewport_width, 0..viewport_height)`.
    pub fn build_layer_presents(&self, scene: &Scene, tile_cache: &TileCache) -> Vec<LayerPresent> {
        let mw = scene.viewport_width as f32;
        let mh = scene.viewport_height as f32;
        scene
            .compositor_surfaces
            .iter()
            .map(|s| {
                let absent = !self.compositor_state.seen_last_frame.contains(&s.key);
                let bounds_changed =
                    self.compositor_state.prev_bounds.get(&s.key).copied() != Some(s.bounds);
                let tile_dirty = self.last_dirty_tiles.iter().any(|c| {
                    tile_cache
                        .tile_world_rect(*c)
                        .is_some_and(|tr| aabb_intersects(tr, s.bounds))
                });
                let dirty = absent || bounds_changed || tile_dirty;

                // Clamp to master pixel space; ensures x0 <= x1 and y0 <= y1
                // even if surface bounds are out-of-order (defensive).
                let clamp = |v: f32, lo: f32, hi: f32| v.max(lo).min(hi);
                let mut x0 = clamp(s.bounds[0], 0.0, mw) as u32;
                let mut y0 = clamp(s.bounds[1], 0.0, mh) as u32;
                let mut x1 = clamp(s.bounds[2], 0.0, mw) as u32;
                let mut y1 = clamp(s.bounds[3], 0.0, mh) as u32;
                if x1 < x0 {
                    std::mem::swap(&mut x0, &mut x1);
                }
                if y1 < y0 {
                    std::mem::swap(&mut y0, &mut y1);
                }

                // Compose `bounds.origin` into `world_transform` so the
                // consumer gets one transform that already places the
                // surface at its declared world position. The
                // user-supplied `s.transform` is column-major
                // `[a, b, c, d, tx, ty]` representing
                // `| a c tx |`
                // `| b d ty |`
                // `| 0 0  1 |`
                // and pre-composing a translation by
                // `(bounds.origin.x, bounds.origin.y)` yields
                // `[a, b, c, d, tx + origin.x, ty + origin.y]` —
                // the linear part is unchanged; only the translation
                // column shifts.
                //
                // Without this, every consumer that holds the surface
                // at layer-local origin (e.g. macOS CALayer) would
                // need to remember `bounds.origin` from declare and
                // re-apply it in present, which the
                // `OsCompositorBackend` trait surface doesn't even
                // carry today (declare gets dims, present gets
                // transform/clip/opacity — neither passes origin).
                // Cleaner to compose here and hand the consumer one
                // transform that's complete on its own.
                //
                // Use the original `s.bounds[0]` / `s.bounds[1]`
                // (not the clamped/swapped `x0` / `y0` above) — those
                // were normalized for `source_rect_in_master`'s
                // master-pixel-space contract; world_transform stays
                // in the user's coordinate space.
                let origin_x = s.bounds[0];
                let origin_y = s.bounds[1];
                let world_transform = [
                    s.transform[0],
                    s.transform[1],
                    s.transform[2],
                    s.transform[3],
                    s.transform[4] + origin_x,
                    s.transform[5] + origin_y,
                ];
                LayerPresent {
                    key: s.key,
                    source_rect_in_master: [x0, y0, x1, y1],
                    world_transform,
                    clip: s.clip,
                    opacity: s.opacity,
                    dirty,
                }
            })
            .collect()
    }

    /// Persist the current frame's compositor-surface state for
    /// next-frame dirty/diff computation. Call after the consumer's
    /// `present_frame` returns.
    pub fn commit_compositor_state(&mut self, scene: &Scene) {
        self.compositor_state.seen_last_frame =
            scene.compositor_surfaces.iter().map(|s| s.key).collect();
        self.compositor_state.prev_bounds = scene
            .compositor_surfaces
            .iter()
            .map(|s| (s.key, s.bounds))
            .collect();
    }

    /// Register a GPU-resident wgpu texture as an image source for
    /// subsequent `render` calls under the given `ImageKey`. The
    /// texture is handed to vello via
    /// `vello::Renderer::register_texture` (Path B from rasterizer
    /// plan §3.5); vello copies into its internal atlas every frame
    /// the image is referenced by a scene.
    ///
    /// Use this when an image source is a render-graph output (blur
    /// result, mask coverage texture, etc.) that exists only on the
    /// GPU and has no CPU-side `ImageData`. Overrides win over
    /// `scene.image_sources` entries with the same `ImageKey`.
    pub fn register_texture(&mut self, key: ImageKey, texture: wgpu::Texture) {
        let image = self.vello_renderer.register_texture(texture);
        self.image_overrides.insert(key, image);
    }

    /// Drop a previously-registered `register_texture` entry.
    /// No-op if `key` was never registered.
    pub fn unregister_texture(&mut self, key: ImageKey) {
        if let Some(image) = self.image_overrides.remove(&key) {
            self.vello_renderer.unregister_texture(image);
        }
    }

    /// Replace the backing GPU texture while preserving Vello's `ImageData`
    /// identity. This is the correct update path for a same-sized source: tile
    /// and retained scenes keep referring to that identity, and Vello is told
    /// to copy the fresh pixels into its atlas on the next render.
    pub fn refresh_texture(&mut self, key: ImageKey, texture: wgpu::Texture) -> bool {
        let Some(image) = self.image_overrides.get(&key) else {
            return false;
        };
        self.vello_renderer.override_image(
            image,
            Some(wgpu::TexelCopyTextureInfoBase {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            }),
        );
        self.vello_renderer.mark_override_image_dirty(image);
        true
    }

    /// Invalidate lowered scene caches after an image override gains a new
    /// `ImageData` identity or is retired. The `ImageKey` embedded in a scene
    /// is intentionally stable, so neither the tile hash nor the retained
    /// fragment signature can otherwise observe this resource replacement.
    pub(crate) fn invalidate_image_override_caches(&mut self) {
        self.tile_scenes.clear();
        self.retained.invalidate_image_override_cache();
    }

    /// Number of tiles whose Scenes were rebuilt by the last
    /// `render` call. Useful for tile-cache hit-rate assertions.
    pub fn last_dirty_count(&self) -> usize {
        self.last_dirty_count
    }

    /// Number of tile-Scenes currently held in the rasterizer's
    /// cache (one per tile present in `TileCache` at last render).
    pub fn cached_tile_count(&self) -> usize {
        self.tile_scenes.len()
    }

    /// Render `scene` into `target_view` via the tile-cache path.
    ///
    /// `target_view`'s texture dimensions are the render size — pass a full
    /// mip-0 view of a viewport-sized texture. See
    /// [`render_scaled`](Self::render_scaled) for the scaled form.
    ///
    /// Steps:
    /// 1. Refresh peniko image data from `scene.image_sources` (Path
    ///    A blobs, dedup by `Blob.id()` if Arc-shared across frames).
    /// 2. `tile_cache.invalidate(scene)` → list of dirty tile coords.
    /// 3. For each dirty tile, build a filtered `vello::Scene`
    ///    containing only the primitives whose AABB intersects the
    ///    tile's world rect.
    /// 4. Evict tile-Scenes whose coords no longer appear in
    ///    `tile_cache` (handled by the tile cache's RETAIN_FRAMES
    ///    eviction).
    /// 5. Compose all cached tile-Scenes into a master Scene with
    ///    per-tile clip layers, render once.
    pub fn render(
        &mut self,
        scene: &Scene,
        tile_cache: &mut TileCache,
        target_view: &wgpu::TextureView,
        base_color: Color,
    ) -> Result<(), vello::Error> {
        self.render_scaled(scene, tile_cache, target_view, base_color, 1.0)
    }

    /// Like [`render`](Self::render) but rasterizes the (logical-coord) scene into a
    /// `scale`×-larger target, applying `scale` as a root affine on the vector master
    /// scene — so a scene laid out at logical (DIP) coordinates fills a physical-pixel
    /// texture crisply (vello rasterizes the scaled vectors at the target resolution).
    /// `scale == 1.0` is the plain path. The target view must be a full mip-0 view of a
    /// texture `scale`× the scene's viewport; **its dimensions are the render size**, so
    /// a host that laid out at a truncated `physical / scale` still fills every row.
    /// (Auto-DPI D2 — content device-pixel-ratio.)
    pub fn render_scaled(
        &mut self,
        scene: &Scene,
        tile_cache: &mut TileCache,
        target_view: &wgpu::TextureView,
        base_color: Color,
        scale: f32,
    ) -> Result<(), vello::Error> {
        // Legacy shared-store path: the rasterizer's own tile-scene map rides
        // along (take/put — `render_scaled_with` borrows it independently of
        // `self`).
        let mut tile_scenes = std::mem::take(&mut self.tile_scenes);
        let result = self.render_scaled_with(
            scene,
            tile_cache,
            &mut tile_scenes,
            target_view,
            base_color,
            scale,
        );
        self.tile_scenes = tile_scenes;
        result
    }

    /// Like [`render_scaled`](Self::render_scaled) but with a caller-owned
    /// per-tile scene store. One store must pair with one [`TileCache`] and one
    /// logical SURFACE: interleaving different scenes through a single shared
    /// (cache, store) pair — the multi-surface host shape — invalidates every
    /// tile on every call, because each scene diffs against the previous
    /// (unrelated) one. The keyed per-surface path
    /// (`Renderer::render_vello_scaled_for`) passes each surface's own pair.
    pub fn render_scaled_with(
        &mut self,
        scene: &Scene,
        tile_cache: &mut TileCache,
        tile_scenes: &mut HashMap<TileCoord, vello::Scene>,
        target_view: &wgpu::TextureView,
        base_color: Color,
        scale: f32,
    ) -> Result<(), vello::Error> {
        use crate::profiling::{FrameTimings, Span};
        let total_span = Span::start("total");
        let mut timings = FrameTimings::empty();

        let master = self.build_master_scene_timed(scene, tile_cache, tile_scenes, &mut timings);

        // At DPR > 1 the master (in logical coords) is appended into a fresh scene under
        // a scale affine; vello then rasterizes the scaled vectors crisply at the
        // physical target size. At 1.0 the master renders directly.
        let scaled_master = if (scale - 1.0).abs() >= 1e-3 {
            let mut s = vello::Scene::new();
            s.append(&master, Some(vello::kurbo::Affine::scale(scale as f64)));
            Some(s)
        } else {
            None
        };
        let to_render = scaled_master.as_ref().unwrap_or(&master);
        // The render size is the TARGET's, not `viewport * scale`. A host lays
        // out at `physical / scale` and stores that viewport as an integer, so
        // the division has already truncated by the time the scene reaches us:
        // 800 physical over a layout scale of 1.8 lays out at 444, and
        // `round(444 * 1.8)` is 799 — one row of the 800-tall texture vello is
        // never asked to write, left transparent. Any scale that does not divide
        // the physical size drops a row or a column that way: every fractional
        // scale in practice (a 150% display produces one with no content zoom at
        // all), and an odd surface at device scale 2.0 too.
        //
        // The texture the caller allocated IS the size it asked for, so read it
        // off the view rather than re-deriving a number the caller has already
        // rounded. Wherever the old derivation landed on the target — scale 1.0
        // always, and any integer scale that divides the surface — this hands
        // vello the same number, so nothing moves there. It relies on the
        // documented contract above: a full mip-0 view of a `scale`×-viewport
        // texture. Receipt: `tests/fractional_scale_target_coverage.rs`.
        let target = target_view.texture();
        let render_w = target.width();
        let render_h = target.height();

        let vello_span = Span::start("vello_render");
        let result = self.vello_renderer.render_to_texture(
            &self.handles.device,
            &self.handles.queue,
            to_render,
            target_view,
            &RenderParams {
                base_color,
                width: render_w,
                height: render_h,
                antialiasing_method: AaConfig::Area,
            },
        );
        vello_span.stop_recording(&mut timings);

        timings.total = total_span.stop();
        self.last_timings = Some(timings);
        result
    }

    /// Render an already-ordered overlay fragment without mutating the
    /// tile cache or per-tile scene cache that tracks the full frame.
    ///
    /// `Renderer::render_with_compositor_and_external_textures` uses
    /// this after direct-sampling an interleaved external texture: the
    /// ordinary scene tail that should remain above that texture is
    /// rendered into a transparent scratch target, then composited over
    /// the master. The full-scene render remains the only caller that
    /// updates tile invalidation state.
    pub fn render_overlay_fragment(
        &mut self,
        scene: &Scene,
        target_view: &wgpu::TextureView,
        base_color: Color,
    ) -> Result<(), vello::Error> {
        let mut merged_images = self.image_data.clone();
        for (key, image) in &self.image_overrides {
            merged_images.insert(*key, image.clone());
        }
        let overlay_scene = scene_to_vello_with_overrides(scene, &merged_images);
        self.vello_renderer.render_to_texture(
            &self.handles.device,
            &self.handles.queue,
            &overlay_scene,
            target_view,
            &RenderParams {
                base_color,
                width: scene.viewport_width,
                height: scene.viewport_height,
                antialiasing_method: AaConfig::Area,
            },
        )
    }

    /// Path (b′) entry point — render `scene` into an internal
    /// master texture pool-allocated by `(width, height, format)`,
    /// returning a reference to it. The caller (typically
    /// `Renderer::render_with_compositor`) hands this reference
    /// onward to a `Compositor::present_frame` call.
    ///
    /// The master texture is owned by the rasterizer and reused
    /// across frames at the same dimensions / format. Viewport
    /// resize or format change reallocates (visible via
    /// [`Self::master_allocations`]).
    ///
    /// `master_format` is the texture format only; the pool always
    /// allocates with `STORAGE_BINDING | TEXTURE_BINDING | COPY_SRC`
    /// usage so the consumer can use the result as a copy source.
    ///
    /// Returns `(master_texture, handles)` — both borrowed from the
    /// rasterizer. The caller uses these to construct a
    /// `PresentedFrame` for the consumer's `Compositor::present_frame`.
    /// Returning both via one `&mut self` call avoids a second borrow
    /// after the master is rendered.
    pub fn render_to_internal_master(
        &mut self,
        scene: &Scene,
        tile_cache: &mut TileCache,
        master_format: wgpu::TextureFormat,
        base_color: Color,
    ) -> Result<(&wgpu::Texture, &WgpuHandles), vello::Error> {
        use crate::profiling::{FrameTimings, Span};
        let total_span = Span::start("total");
        let mut timings = FrameTimings::empty();

        self.ensure_master_texture(scene.viewport_width, scene.viewport_height, master_format);

        let mut tile_scenes = std::mem::take(&mut self.tile_scenes);
        let master_scene =
            self.build_master_scene_timed(scene, tile_cache, &mut tile_scenes, &mut timings);
        self.tile_scenes = tile_scenes;

        // The master_pool entry is guaranteed by ensure_master_texture above.
        let entry = self
            .master_pool
            .as_ref()
            .expect("master_pool guaranteed by ensure_master_texture");
        let view = entry
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let vello_span = Span::start("vello_render");
        let result = self.vello_renderer.render_to_texture(
            &self.handles.device,
            &self.handles.queue,
            &master_scene,
            &view,
            &RenderParams {
                base_color,
                width: scene.viewport_width,
                height: scene.viewport_height,
                antialiasing_method: AaConfig::Area,
            },
        );
        vello_span.stop_recording(&mut timings);
        result?;

        timings.total = total_span.stop();
        self.last_timings = Some(timings);

        Ok((&self.master_pool.as_ref().unwrap().texture, &self.handles))
    }
}
