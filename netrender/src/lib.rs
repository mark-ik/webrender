// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! `netrender` — the renderer shell built on top of the
//! [`netrender_device`] foundation.
//!
//! Phase 0.5 of the netrender design plan
//! ([`netrender-notes/2026-04-30_netrender_design_plan.md`](../../netrender-notes/2026-04-30_netrender_design_plan.md))
//! splits the wgpu device foundation into its own crate so the
//! renderer-internal types (tile cache, render-task graph, picture
//! cache, vello rasterizer) can't leak into consumers that only
//! need the device + WGSL pipeline pattern.
//!
//! As of the batched-path retirement, the renderer is vello-only:
//! `Renderer::render_vello` is the single rendering entry point.
//! The render-task graph (Phase 6) still uses WGSL pipelines from
//! `netrender_device` for blur / clip-mask tasks; their outputs feed
//! into vello scenes via [`Renderer::insert_image_vello`].

#![allow(
    clippy::unreadable_literal,
    clippy::new_without_default,
    clippy::too_many_arguments,
    unknown_lints,
    mismatched_lifetime_syntaxes
)]

pub mod external_texture;
mod filter;
pub mod hit_test;
pub mod interpolate;
pub mod profiling;
pub mod registry;
#[cfg(test)]
mod render_graph_tests;
mod renderer;
pub mod scene;
pub mod tile_cache;
pub mod vello_backends;
pub mod vello_rasterizer;
pub mod vello_tile_rasterizer;

pub use crate::hit_test::{HitOpKind, HitResult, hit_test, hit_test_topmost};
pub use crate::external_texture::{
    EXTERNAL_IMAGE_KEY_BASE, ExternalTextureComposite, ExternalTexturePlacement, SourceAlpha,
    external_image_key,
};
pub use crate::registry::{FontRegistry, ImageRegistry};
pub use crate::renderer::init::{NetrenderOptions, create_netrender_instance};
pub use crate::renderer::{
    ColorLoad, OpaqueTenantInput, OpaqueTenantMetadata, OpaqueTenantReceipt, Renderer,
    RendererError,
};

// `FontBlob.data` is `peniko::Blob<u8>`; consumers constructing one
// directly need access to the `Blob` type. Re-export the peniko
// surface so they don't have to add a separate vello dep just to
// build a font handle.
pub use vello::peniko;
pub use crate::scene::{
    CompositorSurface, FontBlob, FontId, Glyph, GradientKind, GradientStop, ImageData, ImageKey,
    NO_CLIP, PathOp, SHARP_CLIP, Scene, SceneBlendMode, SceneClip, SceneCompose, SceneFilter,
    SceneFontAxisTag, SceneGlyphRun, SceneGradient, SceneImage, SceneLayer, SceneOp, ScenePath,
    ScenePathStroke, ScenePattern, SceneFragment, SceneRect, SceneShape, SceneStroke,
    SceneStrokeCap, SceneStrokeJoin, SurfaceKey, Transform,
};
pub use crate::tile_cache::{TileCache, TileCoord};

// Re-export the device-foundation surface embedders need to construct shared
// handles and pipelines, and to implement compositor handoff surfaces.
// `Compositor`/`LayerPresent`/`PresentedFrame` are the path-(b′)
// trait surface; consumers (genet, etc.) implement `Compositor`
// for native-compositor handoff.
#[cfg(not(target_arch = "wasm32"))]
pub use netrender_device::{boot, boot_on, boot_shared, boot_with};
pub use netrender_device::{
    BrushBlurPipeline, ClipRectanglePipeline, Compositor, LayerPresent, PresentedFrame,
    REQUIRED_FEATURES, REQUIRED_INTER_STAGE_VARIABLES, TenantNeeds, WgpuDevice, WgpuHandles,
    boot_async, boot_async_on, boot_async_shared, boot_async_with, build_brush_blur,
    build_clip_rectangle,
};
