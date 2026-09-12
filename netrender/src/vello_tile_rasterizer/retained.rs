// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Roadmap E4 (spike) — retained fragments.
//!
//! A consumer registers a [`SceneFragment`] once, receives a
//! [`FragmentId`], and thereafter *places* it per frame
//! (`Scene::place_fragment`) instead of re-pushing its ops. The
//! rasterizer caches the fragment's lowered `vello::Scene` across
//! frames and composes placements with `vello::Scene::append(_,
//! Some(affine))`, so a placement-only change (pan / scroll / drag)
//! costs an append rather than a re-lower. Content changes go through
//! `update_fragment`, which bumps the generation and drops the cached
//! lowering.
//!
//! Design: `netrender-notes/2026-08-10_fragment_retention_design.md`.
//! The measured motivation is the pan table in
//! `examples/e1_damage_profile.rs`: 12.9 ms vs 1.2 ms at 4096²
//! pre-spike, all CPU, for a placement-only change.
//!
//! Scenes containing `SceneOp::Fragment` take this module's master
//! path and bypass the tile cache entirely; scenes without fragments
//! keep the exact pre-E4 tile path. Two spike limitations, both
//! deliberate and warned on: a placement inside a `PushLayer` scope
//! falls back to un-retained inlining (correct, just not cached), and
//! nested fragments (fragment content placing another fragment) are
//! skipped at lower time by `scene_to_vello`'s own warn arm.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::Hasher;

use vello::kurbo::{Affine, Rect};
use vello::peniko::{Fill, Mix};

use crate::scene::{FragmentId, Scene, SceneFragment, SceneOp, Transform};
use crate::tile_cache::op_hash;
use crate::vello_rasterizer::{scene_to_vello_with_overrides, transform_to_affine};

use super::VelloTileRasterizer;

/// One registered fragment plus its cached lowering.
pub(super) struct RetainedFragment {
    /// Bumped by `update_fragment`; participates in the frame
    /// signature so content changes rebuild the master.
    generation: u64,
    /// The content, kept for (re-)lowering and the layer fallback.
    fragment: SceneFragment,
    /// Cached lowering, dropped on update. Lazy: first placement
    /// lowers.
    lowered: Option<vello::Scene>,
}

/// Registry + cached master + receipt counters, one per rasterizer.
#[derive(Default)]
pub(crate) struct RetainedState {
    fragments: HashMap<FragmentId, RetainedFragment>,
    next_id: FragmentId,
    /// `(signature, master)` of the last fragment-path frame. Reused
    /// wholesale when the signature matches.
    cached_master: Option<(u64, vello::Scene)>,
    /// Times a fragment was lowered (receipt: retention means this
    /// does not grow under placement-only change).
    lower_count: u64,
    /// Times the cached master was reused wholesale (receipt for the
    /// unchanged-frame short-circuit).
    master_hits: u64,
}

impl RetainedState {
    /// Drop the whole-master shortcut when an external image receives a new
    /// Vello identity or is retired. Its frame signature contains image keys,
    /// not override identities.
    pub(crate) fn invalidate_image_override_cache(&mut self) {
        self.cached_master = None;
        for fragment in self.fragments.values_mut() {
            fragment.lowered = None;
        }
    }

    pub(crate) fn register(&mut self, fragment: SceneFragment) -> FragmentId {
        let id = self.next_id;
        self.next_id += 1;
        self.fragments.insert(
            id,
            RetainedFragment {
                generation: 0,
                fragment,
                lowered: None,
            },
        );
        id
    }

    pub(crate) fn update(&mut self, id: FragmentId, fragment: SceneFragment) -> bool {
        match self.fragments.get_mut(&id) {
            Some(r) => {
                r.generation += 1;
                r.fragment = fragment;
                r.lowered = None;
                // Content changed; the cached master (which composed
                // the old lowering) is stale regardless of signature
                // arithmetic, and the generation bump ensures the
                // signature moves too. Drop it eagerly for clarity.
                self.cached_master = None;
                true
            }
            None => false,
        }
    }

    pub(crate) fn remove(&mut self, id: FragmentId) -> bool {
        let removed = self.fragments.remove(&id).is_some();
        if removed {
            self.cached_master = None;
        }
        removed
    }

    pub(crate) fn lower_count(&self) -> u64 {
        self.lower_count
    }

    pub(crate) fn master_hits(&self) -> u64 {
        self.master_hits
    }
}

/// Does this scene take the fragment path?
pub(super) fn has_fragments(scene: &Scene) -> bool {
    scene
        .ops
        .iter()
        .any(|op| matches!(op, SceneOp::Fragment(_)))
}

/// Frame signature. Two frames with equal signatures compose
/// byte-identical masters, so the cached one can be reused.
///
/// Direct ops hash their full field bytes via the tile cache's op
/// hashers (the same functions whose completeness the E3 differential
/// tests pin), plus the *resolved* transform matrix — the field hash
/// carries only the transform id, and ids are stable across frames
/// while their matrices move. Fragments hash identity + generation +
/// placement matrix. Image sources participate by key, matching the
/// tile path's bytes-under-a-stable-key-are-trusted semantics.
fn frame_signature(scene: &Scene, state: &RetainedState) -> u64 {
    let mut h = DefaultHasher::new();
    h.write_u32(scene.viewport_width);
    h.write_u32(scene.viewport_height);
    h.write_u32(scene.root_alpha.to_bits());
    h.write_u8(scene.root_blend_mode as u8);

    for op in &scene.ops {
        match op {
            SceneOp::Fragment(f) => {
                h.write_u8(0xF0);
                h.write_u64(f.id);
                let generation = state
                    .fragments
                    .get(&f.id)
                    .map_or(u64::MAX, |r| r.generation);
                h.write_u64(generation);
                hash_matrix(&mut h, &scene.transforms[f.transform_id as usize]);
            }
            other => {
                h.write_u8(0x0D);
                op_hash::hash_op_fields(&mut h, other);
                if let Some(tid) = op_hash::op_transform_id(other) {
                    hash_matrix(&mut h, &scene.transforms[tid as usize]);
                }
            }
        }
    }

    let mut keys: Vec<_> = scene.image_sources.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        h.write_u64(k);
    }
    h.finish()
}

fn hash_matrix(h: &mut DefaultHasher, t: &Transform) {
    for v in t.m {
        h.write_u32(v.to_bits());
    }
}

/// Build a standalone `Scene` from a fragment so the ordinary lowering
/// path can translate it. Viewport is nominal (lowering never reads
/// it); tables arrive via E2's `append_fragment` id rewriting.
fn fragment_to_scene(fragment: &SceneFragment) -> Scene {
    let mut s = Scene::new(1, 1);
    s.append_fragment(fragment.clone());
    s
}

/// A painter-order run of non-fragment ops, accumulated into a scratch
/// `Scene` that starts from the parent's tables. Owning real tables is
/// what lets the layer fallback splice composed transforms in.
struct Run {
    scene: Scene,
    is_empty: bool,
}

impl Run {
    fn new(parent: &Scene) -> Self {
        let mut scene = Scene::new(parent.viewport_width, parent.viewport_height);
        scene.transforms = parent.transforms.clone();
        scene.fonts = parent.fonts.clone();
        Self {
            scene,
            is_empty: true,
        }
    }

    fn push(&mut self, op: SceneOp) {
        self.scene.ops.push(op);
        self.is_empty = false;
    }

    /// Layer fallback: splice a fragment's ops in, remapped by E2's
    /// `append_fragment` and with the placement composed onto every
    /// spliced op's transform.
    fn inline_fragment(&mut self, fragment: &SceneFragment, placement: Transform) {
        let base = self.scene.ops.len();
        self.scene.append_fragment(fragment.clone());
        self.is_empty = self.scene.ops.is_empty() && self.is_empty;

        // Compose placement * local for each distinct transform the
        // spliced ops reference. Identity (id 0) composes to the
        // placement itself.
        let mut remap: HashMap<u32, u32> = HashMap::new();
        let mut ops = std::mem::take(&mut self.scene.ops);
        for op in &mut ops[base..] {
            if let Some(tid) = op_hash::op_transform_id(op) {
                let new_id = *remap.entry(tid).or_insert_with(|| {
                    let composed = matmul(&placement, &self.scene.transforms[tid as usize]);
                    let id = self.scene.transforms.len() as u32;
                    self.scene.transforms.push(composed);
                    id
                });
                op_hash::set_op_transform_id(op, new_id);
            }
        }
        self.scene.ops = ops;
    }

    fn flush_into(
        &mut self,
        master: &mut vello::Scene,
        parent: &Scene,
        merged_images: &HashMap<u64, vello::peniko::ImageData>,
    ) {
        if self.is_empty {
            return;
        }
        let sub = scene_to_vello_with_overrides(&self.scene, merged_images);
        master.append(&sub, None);
        *self = Run::new(parent);
    }
}

impl VelloTileRasterizer {
    /// Roadmap E4 — build the master scene for a fragment-bearing
    /// scene. Bypasses the tile cache: direct ops lower fresh in
    /// painter-order runs (expected few — the retained content is the
    /// bulk), fragment placements append cached lowerings.
    ///
    /// Returns a clone of the cached master on signature match. The
    /// clone is an encoding memcpy, paid so callers keep ownership
    /// semantics; if profiles show it mattering, the fix is an
    /// append-from-cache path in `compose_into`, not avoiding the
    /// cache.
    pub(super) fn build_master_scene_fragments(
        &mut self,
        scene: &Scene,
        timings: &mut crate::profiling::FrameTimings,
    ) -> vello::Scene {
        use crate::profiling::Span;

        self.refresh_image_data(scene);

        let sig_span = Span::start("fragment_signature");
        let signature = frame_signature(scene, &self.retained);
        sig_span.stop_recording(timings);

        if let Some((cached_sig, cached)) = &self.retained.cached_master {
            if *cached_sig == signature {
                self.retained.master_hits += 1;
                self.last_dirty_count = 0;
                self.last_dirty_tiles.clear();
                let hit_span = Span::start("master_compose");
                let master = cached.clone();
                hit_span.stop_recording(timings);
                return master;
            }
        }

        let compose_span = Span::start("master_compose");

        // Merged Path A + Path B image map, same as the tile path.
        let mut merged_images = self.image_data.clone();
        for (key, image) in &self.image_overrides {
            merged_images.insert(*key, image.clone());
        }

        let mut master = vello::Scene::new();

        // Phase 12a' scene-level alpha + blend wrap, mirroring
        // `compose_master`.
        let scene_alpha = scene.root_alpha.clamp(0.0, 1.0);
        let scene_blend = super::master::map_blend_mode(scene.root_blend_mode);
        let needs_root_layer = scene_alpha < 1.0 || scene_blend.mix != Mix::Normal;
        if needs_root_layer {
            let viewport = Rect::new(
                0.0,
                0.0,
                scene.viewport_width as f64,
                scene.viewport_height as f64,
            );
            master.push_layer(
                Fill::NonZero,
                scene_blend,
                scene_alpha,
                Affine::IDENTITY,
                &viewport,
            );
        }

        let mut run = Run::new(scene);
        let mut layer_depth: u32 = 0;
        let mut warned_layer_fallback = false;
        let mut relowered: u64 = 0;

        for op in &scene.ops {
            match op {
                SceneOp::Fragment(f) if layer_depth == 0 => {
                    let Some(retained) = self.retained.fragments.get(&f.id) else {
                        log::warn!(
                            "fragment path: placed FragmentId {} is not registered; skipped",
                            f.id
                        );
                        continue;
                    };
                    run.flush_into(&mut master, scene, &merged_images);
                    if retained.lowered.is_none() {
                        let tmp = fragment_to_scene(&retained.fragment);
                        let lowered = scene_to_vello_with_overrides(&tmp, &merged_images);
                        let r = self.retained.fragments.get_mut(&f.id).unwrap();
                        r.lowered = Some(lowered);
                        self.retained.lower_count += 1;
                        relowered += 1;
                    }
                    let r = &self.retained.fragments[&f.id];
                    let affine = transform_to_affine(&scene.transforms[f.transform_id as usize]);
                    master.append(r.lowered.as_ref().unwrap(), Some(affine));
                }
                SceneOp::Fragment(f) => {
                    // Inside a layer scope: the open layer lives in the
                    // run's sub-scene, so an append to the master would
                    // escape it. Inline instead — correct, un-retained.
                    if !warned_layer_fallback {
                        log::warn!(
                            "fragment path: FragmentId {} placed inside a PushLayer scope; \
                             inlining un-retained for layer-scoped placements this frame",
                            f.id
                        );
                        warned_layer_fallback = true;
                    }
                    match self.retained.fragments.get(&f.id) {
                        Some(r) => {
                            let placement = scene.transforms[f.transform_id as usize];
                            run.inline_fragment(&r.fragment, placement);
                        }
                        None => log::warn!(
                            "fragment path: placed FragmentId {} is not registered; skipped",
                            f.id
                        ),
                    }
                }
                SceneOp::PushLayer(_) => {
                    layer_depth += 1;
                    run.push(op.clone());
                }
                SceneOp::PopLayer => {
                    layer_depth = layer_depth.saturating_sub(1);
                    run.push(op.clone());
                }
                other => run.push(other.clone()),
            }
        }
        run.flush_into(&mut master, scene, &merged_images);

        if needs_root_layer {
            master.pop_layer();
        }

        compose_span.stop_recording(timings);

        // Honest dirty accounting for this path: no tiles exist, so
        // report the number of fragments re-lowered this frame.
        self.last_dirty_count = relowered as usize;
        self.last_dirty_tiles.clear();

        self.retained.cached_master = Some((signature, master.clone()));
        master
    }
}

/// 4x4 column-major multiply: `a * b` (apply `b`, then `a`).
fn matmul(a: &Transform, b: &Transform) -> Transform {
    let mut m = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut acc = 0.0;
            for k in 0..4 {
                acc += a.m[k * 4 + row] * b.m[col * 4 + k];
            }
            m[col * 4 + row] = acc;
        }
    }
    Transform { m }
}
