// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Free helpers for the renderer: scene-fragment slicing, the blur kernel
//! planner, backdrop/element-filter scene rewriting helpers, and the CSS
//! color-matrix table. See [`super`].

use crate::scene::{ImageKey, Scene, SceneOp};

/// Build the ordinary-scene suffix that paints after an external texture.
///
/// Scene operation boundaries can fall inside CSS opacity or overflow-clip
/// layers. A raw suffix would then begin with later content (or a `PopLayer`)
/// without the open ancestors that give those operations their meaning. Replay
/// those still-open `PushLayer`s, in outer-to-inner order, before the suffix.
/// The suffix's original matching pops close them, so the fragment is balanced
/// without repainting any prefix content. This only balances the tail redraw;
/// it does not make the external producer itself inherit ancestor clips,
/// transforms, or opacity.
pub(super) fn scene_tail_fragment(scene: &Scene, scene_op_boundary: usize) -> Scene {
    let boundary = scene_op_boundary.min(scene.ops.len());
    let mut open_layers = Vec::new();
    for op in &scene.ops[..boundary] {
        match op {
            SceneOp::PushLayer(layer) => open_layers.push(layer.clone()),
            SceneOp::PopLayer => {
                let popped = open_layers.pop();
                debug_assert!(
                    popped.is_some(),
                    "scene tail boundary has an unbalanced PopLayer in its prefix"
                );
            }
            _ => {}
        }
    }
    let mut fragment = scene.clone();
    fragment.ops = open_layers
        .into_iter()
        .map(SceneOp::PushLayer)
        .chain(scene.ops[boundary..].iter().cloned())
        .collect();
    fragment.compositor_surfaces.clear();
    fragment
}

pub(super) fn make_external_tail_target(
    device: &wgpu::Device,
    viewport_width: u32,
    viewport_height: u32,
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("netrender external texture ordered tail"),
        size: wgpu::Extent3d {
            width: viewport_width,
            height: viewport_height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// Pick a (pass count, per-pass step in pixels) for `brush_blur`
/// such that the cascaded 5-tap binomial kernel approximates a
/// Gaussian with σ = `blur_radius_px / 2` (the conventional CSS
/// blur-radius → σ relation).
///
/// One binomial 5-tap pass with `step = k` pixels has σ ≈ k.
/// N cascaded H+V passes accumulate: σ_total = k · √N.
///
/// We cap the per-pass step at 2 px so each pass keeps a tight tap
/// spread (avoids the visible "5-tap quantization" you get when one
/// pass's step is large relative to the feature size). Larger
/// blurs absorb the budget by running more passes.
///
/// Pass count is capped at `MAX_PASSES` for sanity — at the cap a
/// blur radius of ~28 px is achievable. Roadmap R5 lifts this cap
/// for large blurs via [`blur_kernel_plan_with_downscale`], which
/// picks a downscale level and runs the cascade at the smaller
/// resolution before upscaling back.
fn blur_kernel_plan(blur_radius_px: f32) -> (usize, f32) {
    const MAX_STEP_PX: f32 = 2.0;
    const MAX_PASSES: usize = 50;

    let target_sigma = (blur_radius_px * 0.5).max(0.5);
    if target_sigma <= MAX_STEP_PX {
        // One pass suffices; pick step = σ so the kernel covers σ.
        return (1, target_sigma);
    }
    let passes = ((target_sigma / MAX_STEP_PX).powi(2)).ceil().max(1.0) as usize;
    (passes.min(MAX_PASSES), MAX_STEP_PX)
}

/// Roadmap D1 — does this scene have any layer with a
/// `backdrop_filter` set? Used by `render_vello` to decide whether
/// to take the no-backdrop fast path or the multi-pass path.
pub(super) fn has_backdrop_filter(scene: &Scene) -> bool {
    scene
        .ops
        .iter()
        .any(|op| matches!(op, crate::scene::SceneOp::PushLayer(l) if l.backdrop_filter.is_some()))
}

/// Roadmap D1 — build a "prefix scene" containing every op before
/// the layer at `cutoff_idx`, with any unclosed `PushLayer` scopes
/// closed by appending `PopLayer` ops so the prefix is balanced.
/// Reuses the parent scene's transforms, fonts, and image_sources
/// (cheap; image data is `peniko::Blob` Arc-shares).
pub(super) fn build_prefix_scene(scene: &Scene, cutoff_idx: usize) -> Scene {
    use crate::scene::SceneOp;
    let mut prefix = Scene::new(scene.viewport_width, scene.viewport_height);
    prefix.transforms = scene.transforms.clone();
    prefix.fonts = scene.fonts.clone();
    prefix.image_sources = scene.image_sources.clone();
    prefix.root_alpha = scene.root_alpha;
    prefix.root_blend_mode = scene.root_blend_mode;
    prefix.ops = scene.ops[..cutoff_idx].to_vec();
    // Strip any backdrop_filter from prefix layers — D1 first-cut
    // doesn't recurse into nested filters; later filters processed
    // independently see the unfiltered prefix.
    for op in &mut prefix.ops {
        if let SceneOp::PushLayer(l) = op {
            l.backdrop_filter = None;
        }
    }
    // Balance unclosed PushLayer scopes by appending PopLayer ops.
    let mut depth: i32 = 0;
    for op in &prefix.ops {
        match op {
            SceneOp::PushLayer(_) => depth += 1,
            SceneOp::PopLayer => depth -= 1,
            _ => {}
        }
    }
    for _ in 0..depth.max(0) {
        prefix.ops.push(SceneOp::PopLayer);
    }
    prefix
}

/// Sentinel `ImageKey` region for backdrop-filter results — the top of the u64
/// space (same convention as the `u64::MAX` font sentinel), counting down.
pub(super) const BACKDROP_FILTER_KEY_BASE: ImageKey = u64::MAX - 1;
/// Sentinel `ImageKey` region for element-filter results, counting down from the
/// midpoint — disjoint from [`BACKDROP_FILTER_KEY_BASE`] so the two passes can't
/// collide, and a key above this marks a backdrop image (used to keep element
/// `filter` from filtering the backdrop).
pub(super) const ELEMENT_FILTER_KEY_BASE: ImageKey = u64::MAX / 2;

/// True if any layer carries a non-empty CSS `filter` chain (element filter).
pub(super) fn has_element_filter(scene: &Scene) -> bool {
    use crate::scene::SceneOp;
    scene
        .ops
        .iter()
        .any(|op| matches!(op, SceneOp::PushLayer(l) if !l.filters.is_empty()))
}

/// Index of the `PopLayer` that matches the `PushLayer` at `push_idx` (depth
/// counting). `None` if the scene is unbalanced (a consumer bug).
pub(super) fn matching_pop(ops: &[crate::scene::SceneOp], push_idx: usize) -> Option<usize> {
    use crate::scene::SceneOp;
    let mut depth: i32 = 0;
    for (i, op) in ops.iter().enumerate().skip(push_idx) {
        match op {
            SceneOp::PushLayer(_) => depth += 1, // counts push_idx itself -> depth 1
            SceneOp::PopLayer => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The layer's *own content* — the ops strictly between `push_idx` and
/// `pop_idx` — as a flat sub-scene (the same palette clones as
/// [`build_prefix_scene`], so inner `transform_id`/`font_id`/`key` indices stay
/// valid). The outer `PushLayer`/`PopLayer` are excluded: the layer's
/// alpha/blend/clip get re-applied when the filtered image composites back.
/// Nested filters/backdrops are stripped so this first cut does not recurse.
pub(super) fn build_layer_content_scene(scene: &Scene, push_idx: usize, pop_idx: usize) -> Scene {
    use crate::scene::SceneOp;
    let mut content = Scene::new(scene.viewport_width, scene.viewport_height);
    content.transforms = scene.transforms.clone();
    content.fonts = scene.fonts.clone();
    content.image_sources = scene.image_sources.clone();
    content.root_alpha = scene.root_alpha;
    content.root_blend_mode = scene.root_blend_mode;
    // The layer's own content, minus any backdrop-filter image the backdrop pass
    // injected as the first content op: CSS `filter` must affect only the
    // element, never the backdrop. Backdrop sentinel keys sit above the element
    // region, so a high key marks the injected backdrop image. (It stays in the
    // outer scene, behind the element's filtered result — see the splice below.)
    content.ops = scene.ops[push_idx + 1..pop_idx]
        .iter()
        .filter(|op| !matches!(op, SceneOp::Image(im) if im.key > ELEMENT_FILTER_KEY_BASE))
        .cloned()
        .collect();
    for op in &mut content.ops {
        if let SceneOp::PushLayer(l) = op {
            l.backdrop_filter = None;
            l.filters.clear();
        }
    }
    // The interior is already balanced; this is a defensive no-op.
    let mut depth: i32 = 0;
    for op in &content.ops {
        match op {
            SceneOp::PushLayer(_) => depth += 1,
            SceneOp::PopLayer => depth -= 1,
            _ => {}
        }
    }
    for _ in 0..depth.max(0) {
        content.ops.push(SceneOp::PopLayer);
    }
    content
}

/// Identity color matrix (row-major 4x5; out RGBA = M * [r,g,b,a,1]).
const FILTER_IDENTITY: [f32; 20] = [
    1.0, 0.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, 0.0,
];

/// CSS Filter Effects L1 shorthand color functions as a 4x5 color matrix
/// (row-major; out RGBA = M * [r,g,b,a,1]). Operates on **sRGB-encoded**,
/// **straight** (non-premultiplied) color — the `cs_color_matrix` shader does
/// the unpremultiply/clamp/re-premultiply around it. `Blur` is a spatial pass
/// (not a matrix); the caller dispatches it separately, so it maps to identity
/// here. Coefficients per the spec: saturate/hue-rotate use 0.213/0.715/0.072;
/// grayscale uses the BT.709 0.2126/0.7152/0.0722; identity amounts are 1.0 for
/// brightness/contrast/saturate and 0.0 for grayscale/sepia/invert/hue-rotate.
pub(super) fn scene_filter_to_matrix(f: crate::scene::SceneFilter) -> [f32; 20] {
    use crate::scene::SceneFilter as F;
    match f {
        F::Blur(_) => FILTER_IDENTITY,
        F::Grayscale(a) => {
            let s = 1.0 - a.clamp(0.0, 1.0);
            [
                0.2126 + 0.7874 * s,
                0.7152 - 0.7152 * s,
                0.0722 - 0.0722 * s,
                0.0,
                0.0, //
                0.2126 - 0.2126 * s,
                0.7152 + 0.2848 * s,
                0.0722 - 0.0722 * s,
                0.0,
                0.0, //
                0.2126 - 0.2126 * s,
                0.7152 - 0.7152 * s,
                0.0722 + 0.9278 * s,
                0.0,
                0.0, //
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ]
        }
        F::Sepia(a) => {
            let s = 1.0 - a.clamp(0.0, 1.0);
            [
                0.393 + 0.607 * s,
                0.769 - 0.769 * s,
                0.189 - 0.189 * s,
                0.0,
                0.0, //
                0.349 - 0.349 * s,
                0.686 + 0.314 * s,
                0.168 - 0.168 * s,
                0.0,
                0.0, //
                0.272 - 0.272 * s,
                0.534 - 0.534 * s,
                0.131 + 0.869 * s,
                0.0,
                0.0, //
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ]
        }
        F::Saturate(a) => {
            let s = a.max(0.0);
            [
                0.213 + 0.787 * s,
                0.715 - 0.715 * s,
                0.072 - 0.072 * s,
                0.0,
                0.0, //
                0.213 - 0.213 * s,
                0.715 + 0.285 * s,
                0.072 - 0.072 * s,
                0.0,
                0.0, //
                0.213 - 0.213 * s,
                0.715 - 0.715 * s,
                0.072 + 0.928 * s,
                0.0,
                0.0, //
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ]
        }
        F::HueRotate(deg) => {
            let t = deg.to_radians();
            let (c, n) = (t.cos(), t.sin());
            [
                0.213 + c * 0.787 - n * 0.213,
                0.715 - c * 0.715 - n * 0.715,
                0.072 - c * 0.072 + n * 0.928,
                0.0,
                0.0, //
                0.213 - c * 0.213 + n * 0.143,
                0.715 + c * 0.285 + n * 0.140,
                0.072 - c * 0.072 - n * 0.283,
                0.0,
                0.0, //
                0.213 - c * 0.213 - n * 0.787,
                0.715 - c * 0.715 + n * 0.715,
                0.072 + c * 0.928 + n * 0.072,
                0.0,
                0.0, //
                0.0,
                0.0,
                0.0,
                1.0,
                0.0,
            ]
        }
        F::Brightness(a) => {
            let m = a.max(0.0);
            [
                m, 0.0, 0.0, 0.0, 0.0, //
                0.0, m, 0.0, 0.0, 0.0, //
                0.0, 0.0, m, 0.0, 0.0, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ]
        }
        F::Contrast(a) => {
            let m = a.max(0.0);
            let b = 0.5 * (1.0 - m);
            [
                m, 0.0, 0.0, 0.0, b, //
                0.0, m, 0.0, 0.0, b, //
                0.0, 0.0, m, 0.0, b, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ]
        }
        F::Invert(a) => {
            let a = a.clamp(0.0, 1.0);
            let d = 1.0 - 2.0 * a;
            [
                d, 0.0, 0.0, 0.0, a, //
                0.0, d, 0.0, 0.0, a, //
                0.0, 0.0, d, 0.0, a, //
                0.0, 0.0, 0.0, 1.0, 0.0,
            ]
        }
    }
}

/// Roadmap R5 — upgraded planner that introduces a downscale level
/// for blurs beyond what the cascade alone can reach.
///
/// Returns `(level, passes, step_px)` where `level ∈ {1, 2, 4, 8}`
/// is the resolution divisor for the blur intermediate. A `level`
/// of 1 keeps everything at native resolution (existing behavior);
/// higher levels halve the work-resolution `level` times, so the
/// effective blur radius in source-pixel units becomes
/// `step_px · √passes · level`.
///
/// Heuristic: at native resolution the cascade caps at
/// `SINGLE_LEVEL_MAX_RADIUS ≈ 28` px (50 passes at MAX_STEP_PX = 2,
/// 2σ → blur_radius). Beyond that we step down by powers of 2 so
/// the *scaled* radius stays under the cap.
///
/// `passes` and `step_px` are then `blur_kernel_plan(blur_radius_px
/// / level)` — the cascade plan for the radius as it appears at
/// the scaled resolution.
pub(crate) fn blur_kernel_plan_with_downscale(blur_radius_px: f32) -> (u32, usize, f32) {
    const SINGLE_LEVEL_MAX_RADIUS: f32 = 28.0;
    const MAX_LEVEL: u32 = 8; // Stops the level chain at quarter-quarter res.

    let level: u32 = if blur_radius_px <= SINGLE_LEVEL_MAX_RADIUS {
        1
    } else {
        let raw = (blur_radius_px / SINGLE_LEVEL_MAX_RADIUS).ceil() as u32;
        raw.next_power_of_two().min(MAX_LEVEL)
    };
    let scaled_radius = blur_radius_px / level as f32;
    let (passes, step_px) = blur_kernel_plan(scaled_radius);
    (level, passes, step_px)
}

#[cfg(test)]
mod scene_tail_tests {
    use super::scene_tail_fragment;
    use crate::{Scene, SceneLayer, SceneOp};

    #[test]
    fn tail_reopens_nested_layers_without_replaying_prefix_content() {
        let mut scene = Scene::new(64, 64);
        scene.push_layer(SceneLayer::alpha(0.75));
        scene.push_rect(0.0, 0.0, 8.0, 8.0, [1.0, 0.0, 0.0, 1.0]);
        scene.push_layer(SceneLayer::alpha(0.5));
        scene.push_rect(8.0, 8.0, 16.0, 16.0, [0.0, 1.0, 0.0, 1.0]);
        let boundary = scene.ops.len();
        scene.push_rect(16.0, 16.0, 24.0, 24.0, [0.0, 0.0, 1.0, 1.0]);
        scene.pop_layer();
        scene.pop_layer();

        let tail = scene_tail_fragment(&scene, boundary);
        assert_eq!(tail.ops.len(), 5);
        assert!(matches!(tail.ops[0], SceneOp::PushLayer(ref layer) if layer.alpha == 0.75));
        assert!(matches!(tail.ops[1], SceneOp::PushLayer(ref layer) if layer.alpha == 0.5));
        assert!(
            matches!(tail.ops[2], SceneOp::Rect(ref rect) if rect.color == [0.0, 0.0, 1.0, 1.0])
        );
        assert!(matches!(tail.ops[3], SceneOp::PopLayer));
        assert!(matches!(tail.ops[4], SceneOp::PopLayer));
    }

    #[test]
    fn tail_after_closed_layers_does_not_reopen_or_duplicate_them() {
        let mut scene = Scene::new(64, 64);
        scene.push_layer(SceneLayer::alpha(0.5));
        scene.push_rect(0.0, 0.0, 8.0, 8.0, [1.0, 0.0, 0.0, 1.0]);
        scene.pop_layer();
        let boundary = scene.ops.len();
        scene.push_rect(8.0, 8.0, 16.0, 16.0, [0.0, 1.0, 0.0, 1.0]);

        let tail = scene_tail_fragment(&scene, boundary);
        assert_eq!(tail.ops.len(), 1);
        assert!(
            matches!(tail.ops[0], SceneOp::Rect(ref rect) if rect.color == [0.0, 1.0, 0.0, 1.0])
        );
    }
}

#[cfg(test)]
mod blur_plan_tests {
    use super::blur_kernel_plan;

    #[track_caller]
    fn assert_close(actual: f32, expected: f32, tol: f32, label: &str) {
        let diff = (actual - expected).abs();
        assert!(
            diff <= tol,
            "{}: actual {}, expected {} (diff = {}, tol = {})",
            label,
            actual,
            expected,
            diff,
            tol,
        );
    }

    #[test]
    fn zero_radius_collapses_to_single_tight_pass() {
        let (passes, step) = blur_kernel_plan(0.0);
        assert_eq!(passes, 1);
        assert_close(step, 0.5, 0.01, "step at radius 0 floors to 0.5");
    }

    #[test]
    fn small_radius_uses_one_pass_with_step_eq_sigma() {
        let (passes, step) = blur_kernel_plan(2.0); // σ_target = 1.0
        assert_eq!(passes, 1);
        assert_close(step, 1.0, 0.01, "step matches target σ when σ ≤ 2");
    }

    #[test]
    fn radius_at_step_cap_still_one_pass() {
        let (passes, step) = blur_kernel_plan(4.0); // σ_target = 2.0
        assert_eq!(passes, 1);
        assert_close(step, 2.0, 0.01, "σ at MAX_STEP_PX still single-pass");
    }

    #[test]
    fn large_radius_cascades() {
        // σ_target = 5, MAX_STEP_PX = 2 → passes = ceil((5/2)²) = 7
        let (passes, step) = blur_kernel_plan(10.0);
        assert_eq!(passes, 7);
        assert_close(step, 2.0, 0.01, "step pinned at MAX_STEP_PX for cascaded");

        // σ_total = step·√passes ≈ 2·√7 ≈ 5.29 ≥ target 5.0
        let actual_sigma = step * (passes as f32).sqrt();
        assert!(
            actual_sigma >= 5.0,
            "cascaded σ {} should reach target 5.0",
            actual_sigma,
        );
    }

    #[test]
    fn pass_count_capped() {
        let (passes, _) = blur_kernel_plan(1000.0);
        assert!(
            passes <= 50,
            "MAX_PASSES = 50 should cap unbounded radii; got {}",
            passes,
        );
    }

    use super::blur_kernel_plan_with_downscale;

    #[test]
    fn pr5_small_radius_keeps_level_one() {
        let (level, _, _) = blur_kernel_plan_with_downscale(8.0);
        assert_eq!(level, 1, "small radii skip downscale");
        let (level, _, _) = blur_kernel_plan_with_downscale(28.0);
        assert_eq!(level, 1, "exactly at the cap stays at level 1");
    }

    #[test]
    fn pr5_medium_radius_picks_level_two() {
        // Radii between 28 and 56 round up to level 2 (next power
        // of 2 above ceil(radius / 28)).
        let (level, _, _) = blur_kernel_plan_with_downscale(40.0);
        assert_eq!(level, 2, "radius 40 picks level 2");
    }

    #[test]
    fn pr5_large_radius_picks_higher_level() {
        let (level, _, _) = blur_kernel_plan_with_downscale(100.0);
        assert!(level >= 4, "radius 100 picks level ≥ 4, got {level}");
        let (level, _, _) = blur_kernel_plan_with_downscale(1000.0);
        assert!(level <= 8, "level capped at 8, got {level}");
    }

    #[test]
    fn pr5_passes_stay_unclipped_for_realistic_radii() {
        // At every level chosen by the heuristic, for radii up to
        // MAX_LEVEL * SINGLE_LEVEL_MAX_RADIUS = 8 * 28 = 224, the
        // scaled cascade should stay within the 50-pass cap. Beyond
        // 224 the downscale heuristic clamps at level 8 and σ-clip
        // returns; documenting that as a known limit.
        for &r in &[8.0_f32, 28.0, 40.0, 64.0, 100.0, 200.0, 224.0] {
            let (level, passes, _) = blur_kernel_plan_with_downscale(r);
            assert!(
                passes <= 50,
                "radius {r}: at level {level}, passes = {passes} exceeds MAX_PASSES"
            );
            // For radii in this range the cascade should not be at
            // the cap (it has headroom).
            if r <= 200.0 {
                assert!(
                    passes < 50,
                    "radius {r}: at level {level}, passes = {passes} should be below the 50-pass cap (downscale path has headroom)"
                );
            }
        }
    }
}
