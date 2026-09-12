# Vello rasterizer verification record (§11)

Split out of [`2026-05-01_vello_rasterizer_plan.md`](2026-05-01_vello_rasterizer_plan.md)
on 2026-08-10, where it had grown to 1369 lines: 62% of a file labelled
as the source of truth for the live architecture. It is an append-only
evidence log, not architecture, so it lives on its own.

Section numbers are unchanged. An inbound `§11.x` reference resolves here.

38 entries, 36 of them **CLEARED**. The two without a verdict are §11.6
(closed in practice by Phase 1' first-light, see §11.7, but never
relabelled) and §11.13 (a display-list-format discussion that did not
need one).

**Appending:** when a Phase R wart fix or a deferred-phase item lands,
add a `### 11.x — CLEARED` entry here and strike it from
[`2026-05-04_feature_roadmap.md`](2026-05-04_feature_roadmap.md).

---

## 11. Verification record

All five gates have been verified through research-spike cycles
(2026-05-01). Originals stated "before writing a single line of
`VelloRasterizer`"; what follows is what we now know.

### 11.1 wgpu / vello version compatibility — **CLEARED**

Vello main is on `wgpu = "29.0.1"` (`vello/Cargo.toml:137`); this
is the wgpu-29 bump that "unblocked vello development" per the
linebender team's recent activity. Released-tag 0.8.0 still
targets wgpu 28; we'll consume vello via git ref to main until
their next tagged release.

`VELLO_BASELINE` wgpu features (the Phase-0.5 axiom-10 amendment):
the precise list is not yet enumerated — the `boot()` call site
will surface what's required when we add vello. wgpu's `Features::empty()`
baseline is unlikely to suffice; expect compute-shader + atomics +
storage-binding requirements at minimum. Lavapipe / WARP /
SwiftShader are reported to satisfy vello on community usage but
the §11.5-followup spike (Vulkan validation behavior, see §6.1)
should answer this directly when it runs.

Software-adapter validation may produce noise on Vulkan due to
[wgpu#5379](https://github.com/gfx-rs/wgpu/issues/5379) (open) —
documented in §6.1; mitigation path identified.

### 11.2 Premultiplied-alpha and color-space — **CLEARED with boundary work**

Verified: `peniko::Color` is straight alpha (not premultiplied);
vello premultiplies internally
(`vello_encoding/src/draw.rs:79`). Our scene's premultiplied colors
need unpremultiply-at-boundary in the encoder. `peniko::Gradient`
defaults to `ColorSpaceTag::Srgb` (sRGB-encoded interpolation);
explicit `with_interpolation_cs(LinearSrgb)` to override, per
§3.3 update.

### 11.3 Vello scene reuse / parallelism model — **CLEARED with architectural revision**

Verified facts (research, no code spike needed):

- One `vello::Scene` per `Renderer::render_to_texture` call. To
  render N targets, call N times.
- `render_to_texture` does NOT take a caller-supplied
  `wgpu::CommandEncoder` — it creates and submits its own per
  call (`wgpu_engine.rs:380-757`). No public path to encoder
  sharing.
- `low_level::Recording` is public but `WgpuEngine::run_recording`
  is `pub(crate)` and there's no roadmap item to expose it. Forking
  is the only path; ruled out for this project.
- No multi-region-of-one-target API. `RenderParams { width, height,
  base_color, antialiasing_method }` lacks viewport/scissor.
- `Renderer` itself amortizes pipelines + Resolver across calls.
  Reuse one `Renderer` per `(Device, surface_format)` pair.
- Resolver caches glyph encodings + ramp LUT bytes + image atlas
  slots across frames; does NOT cache scene-buffer packing,
  ramp-atlas GPU upload, dispatch buffers, or compute dispatches.

`vello_hybrid` (sparse_strips experimental crate) was investigated
as an escape hatch: it does expose caller-supplied
`CommandEncoder`, but lacks multi-region/multi-target/scissor
APIs *and* is workspace-internal at v0.0.7 ("not yet suitable for
production"). Not the answer.

**Architectural decision: Option C (Masonry pattern).** Per-tile
`vello::Scene` cached CPU-side; composed via `Scene::append`
(verified cheap — `extend_from_slice` on bytewise streams in
`vello_encoding/src/encoding.rs:94-172`); one
`render_to_texture` per frame; one submit. See §2.

### 11.4 External-texture import — **CLEARED with cost note**

`Renderer::register_texture(&wgpu::Texture)` exists in vello 0.6+
(`lib.rs:562-590`). Accepts `Rgba8Unorm`, straight alpha, with
`COPY_SRC` usage. **Caveat: copies into vello's atlas every
frame** — not zero-copy. Path A (`Arc<Blob<u8>>`) is the default
since blob ID dedup makes it effectively single-upload across
frames; Path B (`register_texture`) is the right path when the
input is itself a wgpu texture (render-graph output → vello
input). See §3.5 update.

### 11.5 Render-target format — **CLEARED with reframe**

Verified: vello's compute target is hardcoded to `Rgba8Unorm` /
`Bgra8Unorm`. **`Rgba16Float` is not supported** by the public
API. The §6 color contract is reframed accordingly: stay on
`Rgba8Unorm` storage with `Rgba8UnormSrgb` view-format trick for
sample-time sRGB→linear decode. See §6.1 for the chain and the
Vulkan validation asterisk.

The drop-shadow integration test (vello rasterizes → existing
`brush_blur.wgsl` consumes) is now a Phase 6' receipt rather
than a §11 gate; the format compatibility question is settled.

### 11.6 Items still requiring runtime spike

Two narrow questions need a real `cargo add vello` + 50-line test
to resolve, but neither is plan-blocking:

1. Vulkan validation behavior on Lavapipe / radv with
   `Rgba8Unorm` storage + `Rgba8UnormSrgb` view, given wgpu-hal
   doesn't set `EXTENDED_USAGE_BIT`. May produce warnings; may
   assert. Determines whether headless-CI on software-adapter
   Vulkan works without a manual-decode fallback shader.
2. Quantization round-trip exactness: writing `f32` to
   `Rgba8Unorm` storage and reading via `Rgba8UnormSrgb` should
   yield `srgb_decode(round(f * 255) / 255)` with no driver-
   injected linearize step on the storage write. Code-spike
   confirmation; expected to pass.

Both fall out naturally in Phase 1' first-light — schedule there,
not as separate work.

### 11.7 Phase 1' first-light findings (2026-05-02) — **CLEARED**

`netrender/tests/p1prime_vello_first_light.rs` runs three probes
against a real `boot()` device + `Renderer::render_to_texture`:

1. **`p1prime_01_vello_renders_red_rect`** — opaque red round-trips
   to `(255, 0, 0, 255)` ✓. Confirms vello compiles, links, boots on
   our device, and writes through the `Rgba8Unorm` storage with
   `Rgba8UnormSrgb` view-format slot reserved without producing
   adapter-side validation errors. Quantization round-trip clears.
2. **`p1prime_02_alpha_storage_is_straight`** — half-opaque red
   `(255, 0, 0, 128)` lands in storage as `(255, 0, 0, 128)` ✓.
   **Plan correction:** vello stores **straight-alpha**, not
   premultiplied. Internal blend math is premultiplied
   (`fine.wgsl` blend stages), but the output stage at
   `vello_shaders/shader/fine.wgsl:1390-1395` divides by alpha
   before `textureStore`. §6.1 updated: compositor must
   premultiply at sample time.
3. **`p1prime_03_gradient_default_is_srgb_encoded`** — red→blue
   linear gradient midpoint is `(128, 0, 128)` for both default and
   `with_interpolation_cs(LinearSrgb)` ✓. **Plan correction:** the
   GPU compute path ignores `interpolation_cs` entirely.
   `vello_encoding/src/encoding.rs:289-339` doesn't read it;
   `vello_encoding/src/ramp_cache.rs:84-111` hard-codes
   `to_alpha_color::<Srgb>()` for every stop. Linear-light
   gradients are unreachable until upstream wires it through.
   §3.3 updated. Test inverts to known-failure if upstream fixes
   this.

Both 11.6 items resolved as a side effect: no Vulkan validation
errors observed on the dev box (DX12-backed wgpu adapter), and
quantization round-trip is exact for primary opaque colors.

### 11.8 Phase 7' completion findings (2026-05-04) — **CLEARED**

The Masonry-pattern tile cache shipped as
[`netrender/src/vello_tile_rasterizer.rs`](../netrender/src/vello_tile_rasterizer/mod.rs)
(305 lines). All four `p7prime_vello_tile_cache` probes pass + four
`p7prime_renderer_integration` end-to-end probes pass against the
existing batched-pipeline oracle PNGs.

**What we verified:**

1. **`Scene::append` is bytewise-cheap as expected.** No measurable
   per-tile composition overhead in the test harness; the per-frame
   work is dominated by vello's compute dispatches, not the CPU-side
   tile-Scene merge. Aligns with `vello_encoding/src/encoding.rs`
   verification from §11.3.
2. **Per-tile clip layers correctly handle spanning primitives.** A
   half-alpha rect spanning all four tiles of a 2×2 grid renders to
   uniform `(255, 0, 0, 128)` everywhere it covers — no double-blend
   at tile borders. Each tile-Scene is wrapped in
   `push_layer(tile_world_rect)` / `pop_layer` at compose time, which
   constrains each tile's draws to its own region. Verified by
   `p7prime_04_spanning_primitive_no_double_render`.
3. **TileCache invalidation drives the rasterizer correctly.** A
   no-op re-render reports zero dirty tiles
   (`p7prime_02_unchanged_scene_no_dirty`); a single-rect color
   change marks only its tile dirty
   (`p7prime_03_localized_change`). The `cached_tile_count` /
   `last_dirty_count` getters expose this for hit-rate assertions.
4. **Renderer-level integration via `enable_vello: true`.** The two
   pipelines (batched, vello) coexisted briefly via parallel
   entry points (`prepare/render` vs `render_vello`) sharing the
   same `TileCache`; this proved the integration shape, then the
   batched path was retired entirely (§10's "two backends trap"
   decision applied).

**What we deferred or simplified:**

- **No `Rasterizer` trait.** §2.2 originally proposed
  `Box<dyn Rasterizer>` on `Renderer`. With one rasterizer, the
  trait is an abstraction without users. `VelloTileRasterizer` is
  concrete on `Renderer`. Re-introduce only when a second rasterizer
  ships.
- **Per-frame image-cache rebuild — resolved (2026-05-04).**
  `VelloTileRasterizer::refresh_image_data` previously cleared and
  rebuilt the Path A `peniko::ImageData` map every frame, defeating
  vello's `Blob.id()` dedup. Now the map is persistent: new
  `ImageKey`s are added on first sight via `entry().or_insert_with`,
  keys that disappear from `scene.image_sources` are evicted via
  `retain`. Each `Arc<Vec<u8>>` lives across frames so vello's atlas
  uploads once per key. Verified by `p7prime_05` (Blob id stable
  across re-render and Scene-instance swap) and `p7prime_06`
  (eviction when key drops from scene). The same per-frame rebuild
  still exists in `vello_rasterizer::build_image_cache` for the
  non-tile path; that path doesn't own state across frames so the
  fix would require either a stateful wrapper or moving the cache
  up into the caller.
- **No native-compositor handoff (axiom 14).** Confirmed loss as
  predicted in §2.4. Servo doesn't use this today; the v1.5 fallback
  in §recommendation (whole-frame vello + post-render tile slicing)
  remains an option if Firefox-style native compositing becomes
  required.

**Cleanup outcome (2026-05-04):**

After Phase 7' integration, the batched WGSL rasterizer was retired
on `main`:

- `netrender/src/batch.rs` (608 lines) deleted
- `netrender/src/image_cache.rs` (170 lines) deleted
- `Renderer::prepare` / `render` / `prepare_direct` /
  `prepare_tiled` / `render_dirty_tiles*` /
  `build_tile_composite_draw` / `ensure_gradient_pipelines` /
  `insert_image_gpu` removed
- `PreparedFrame` / `FrameTarget` / `ResourceRefs` /
  `ColorAttachment` / `DepthAttachment` / `DrawIntent` /
  `RenderPassTarget` removed
- `netrender_device`'s `brush_solid` / `brush_rect_solid` /
  `brush_image` / `brush_gradient` pipeline factories + WGSL
  sources + bind-group layouts + tests retired (the crate dropped
  from 2394 → 730 lines)
- 11 redundant batched-path tests deleted; remaining tests run
  through `render_vello`
- The legacy upstream WebRender code (`webrender_api`, `wrench`,
  `wr_glyph_rasterizer`, `examples`, `wrshell`,
  `example-compositor`, `fog`, `peek-poke`, `wr_malloc_size_of`,
  `ci-scripts`) was removed from the workspace and the working
  tree (preserved on the `webrender-wgpu-upstream` side worktree)

Net: -90,000 lines on `main` across the cleanup, leaving netrender
(6,034) + netrender_device (730) ≈ 6,764 lines of live Rust. Vello
is the sole rasterizer.

### 11.9 `FontBlob` unified to `peniko::Blob<u8>` (2026-05-04) — **CLEARED**

`netrender::FontBlob` originally held `Arc<Vec<u8>>` plus a `u32`
font-collection index, and `vello_rasterizer::emit_glyph_run`
wrapped it in a fresh `peniko::Blob::new(...)` per glyph run, per
render. Two consequences:

1. **Vello's font atlas couldn't dedup across frames.** Vello keys
   font atlas slots on `Blob::id()`. `Blob::new` mints a unique id
   at construction; reconstructing the blob every render meant
   every frame's font lookup hit a fresh id and re-uploaded.
2. **The parley adapter copied bytes per `push_layout` call** —
   `Arc::new(font_data.data.data().to_vec())` allocated a fresh
   `Vec<u8>` of the TTF size, since `parley::FontData::data` was
   `peniko::Blob<u8>` and `FontBlob.data` was `Arc<Vec<u8>>`.
   Different shape, no conversion path that preserved id.

Resolution: changed `pub data: Arc<Vec<u8>>` to `pub data:
peniko::Blob<u8>` (re-exported via `netrender::peniko::Blob`).
Construction sites in tests now wrap with `Blob::new(Arc::new(..))`
once. `emit_glyph_run` clones the blob (Arc + id copy, no bytes).
Parley adapter clones `font_data.data` directly, no `to_vec()`.

This deliberately leaks `peniko` into `netrender::scene`'s public
API. The earlier doc claim "the wrapper exists so netrender's Scene
API doesn't leak peniko types" was undermined by the rasterizer
already round-tripping through `peniko::Blob` per render and
defeating its dedup. The honest fix is to align the type, accept
the public-API surface, and re-export `peniko` for consumer access.

Receipts:

- All 79 workspace tests pass after the change (the same set that
  passed before, including the parley adapter's `shape_and_paint`
  binary and the renderer integration tests).
- Construction-site updates in
  `netrender/tests/p10prime_a_glyph_api.rs` (5 sites),
  `netrender/tests/p10prime_b_glyph_render.rs` (1 site), and
  `netrender_text/src/lib.rs` (1 site, now `font_data.data.clone()`).
- `vello_rasterizer.rs::emit_glyph_run` simplified from
  `Blob::new(blob.data.clone())` to `blob.data.clone()`.

Side effect: `netrender` now `pub use vello::peniko;` so consumers
can build `FontBlob` without a separate `vello`/`peniko` dep.

### 11.10 Variable-radius box-shadow blur (2026-05-04) — **CLEARED**

`Renderer::build_box_shadow_mask` previously took a `blur_step: f32`
(texel-space sample distance for one fixed 2-pass blur). The 5-tap
binomial kernel saturates at small effective blur — pushing the
step up past ~2 px per tap produces visible 5-tap quantization
instead of a smooth Gaussian, so the API couldn't honestly serve
CSS-style `box-shadow: 0 0 12px` requests.

Resolution: signature is now `blur_radius_px: f32` (CSS-pixel
units). `blur_kernel_plan` picks a per-pass step capped at 2 px
and a pass count `N = ceil((σ_target / step)²)` where
`σ_target = blur_radius_px / 2` (WebKit/Mozilla convention; the
spec is ambiguous, the comment in `renderer/mod.rs` flags this).
`build_box_shadow_mask` then chains `1 + 2N` render-graph tasks:
mask, then N alternating H/V `brush_blur` passes. Pass count
capped at 50 — large blurs that exceed it would benefit from the
classic downscale-blur-upscale trick, not implemented yet.

Receipts:

- Five unit tests (`blur_plan_tests`) cover the planner: zero
  radius, σ at the cap boundary, cascade trigger, the σ_total
  invariant, and the pass-count cap.
- `p11c_02_blur_radius_extends_halo` (in
  `netrender/tests/p11prime_c_box_shadow.rs`) renders the same
  shadow source with `blur_radius_px = 2` and `= 16` and asserts
  the larger blur darkens a probe 8 px outside the source by
  ≥ 25 grayscale levels — visible runtime evidence the cascade
  widens the kernel as the radius grows.
- Demo (`demo_card_grid.rs`) bumped from a 1-pass tight blur to
  `blur_radius_px = 12.0` for Card 5; the resulting halo is
  visibly softer and extends further than the previous output.

Math notes (for future maintainers):

- One 5-tap binomial pass with step = `k` pixels has σ = `k`
  (variance = `k²` — the kernel weights `[1, 4, 6, 4, 1] / 16`
  applied at offsets `[-2k, -k, 0, k, 2k]`).
- Cascading N H+V pairs accumulates variance: `σ_total = k · √N`.
- The empirical receipt at the probe matches Gaussian-edge falloff
  `0.5 · erfc(d / (σ√2))` to within bilinear-sampler precision.

### 11.17 C-architecture readiness — `compose_into` + registries (2026-05-04) — **CLEARED**

Background: graphshell-shaped consumers building multiple netrender
viewports per frame have three architecture options (per the
recommendation discussion this session):

- **A.** Each consumer owns its own `vello::Renderer` and renders to
  its own texture. Cross-consumer interaction = texture sampling.
- **B.** Consumers share one `vello::Renderer` via `Mutex`. Each
  still renders to its own texture; renders serialize at the lock.
- **C.** A single `vello::Scene` per frame, composed from N
  consumers via `Scene::append`, rendered once. Atlas slots dedup
  across consumers via `peniko::Blob::id()`. Cross-consumer
  interaction = bytewise scene composition.

The decision was: don't ship the consumer side of A→B→C yet (no
multi-consumer code paths exist), but **make the netrender side
C-ready now** so when graphshell decides on C the renderer is
already speaking the protocol. The work landed in this finding.

**What changed:**

1. **`ImageData.bytes` unified to `peniko::Blob<u8>`** (analog to
   §11.9's `FontBlob` unification). Required for cross-consumer
   atlas dedup: vello keys atlas slots on `Blob::id()`, which is
   stable through `Arc`-shared bytes but not through fresh
   `Vec<u8>` clones. New constructors `ImageData::from_bytes` and
   `ImageData::from_blob` cover the common cases. 8 construction
   sites updated across tests and the demo.

2. **`netrender::FontRegistry`** (`registry.rs`). HashMap from
   `(Blob::id(), font_index)` → `FontId`. Threaded through
   `netrender_text::push_layout_with_registry` (new function);
   the existing `push_layout` becomes a thin wrapper that builds
   a fresh registry per call. Consumers that build many layouts
   into one Scene per frame share one registry → one entry in
   `scene.fonts` per unique font, regardless of call count.
   Receipts: 3 unit tests (dedup within call, separate distinct
   blobs, separate distinct collection indices).

3. **`netrender::ImageRegistry<K>`** (`registry.rs`). HashMap from
   consumer-supplied key `K: Eq + Hash` → `ImageKey`. The
   consumer-key shape acknowledges that "is image A the same as
   image B" is a consumer-domain question (same URL? content
   hash?) we can't answer — we just provide the bookkeeping.
   Receipts: 3 unit tests (dedup by consumer key, distinct keys
   allocate distinct ImageKeys, `get` doesn't insert).

4. **`VelloTileRasterizer::compose_into`** — the C entry point.
   Same tile-cache update + master-scene composition as `render`,
   but appends the result into a caller-provided `vello::Scene`
   with a caller-provided `Affine` instead of rendering to a
   texture. Internal: factored a private `build_master_scene`
   helper that both `render` and `compose_into` call. Receipts:
   3 integration tests:
   - `compose_into_01_identity_matches_render` — pixel-exact
     match (within ±1 channel) between rendering directly and
     composing-then-rendering at identity transform. Pins the
     contract that `compose_into` is a refactor of the inner
     steps of `render`, not a different code path.
   - `compose_into_02_transform_translates_content` — translate
     transform shifts content by exactly that translate.
   - `compose_into_03_two_consumers_share_atlas` — two
     `VelloTileRasterizer`s composing scenes that reference the
     *same* `Arc`-shared image bytes produce the same Blob id in
     each rasterizer's image cache. The cross-consumer dedup
     signal vello's atlas keys on is reachable.

**What this enables:**

- Graphshell can hold one `vello::Renderer` at app boot, give
  netrender consumers a `&mut vello::Scene` to compose into, and
  do a single `render_to_texture` per frame. No cross-consumer
  texture-sampling boundary; one GPU submit; atlas slots shared
  across panes.
- An animating embedded surface (graph node moving across canvas
  with embedded webview content) re-rasterizes from vector data
  every frame instead of resampling a fixed texture — sharp at
  any zoom and any motion.
- Live thumbnails for a navigator: append a pane's tile-Scenes
  into the swatch's master Scene with a scale transform; one
  rasterization, no texture readback.

**What it does *not* enable on its own:**

- Concurrent N-consumer encoding under B is still serialized at
  the renderer Mutex; only C avoids that.
- Cross-consumer image-data sharing only kicks in when consumers
  hand the same `Arc`-shared bytes (or use a shared
  `ImageRegistry`). Two consumers that each load the same favicon
  from disk into separate `Vec`s still get separate atlas slots
  — by design (we don't try to content-hash bytes for them).

**Workspace state after this finding:** 114 tests passing
(was 105; +9: 6 registry, 3 compose_into), 0 failures,
0 clippy warnings, 0 build warnings.

### 11.11 Unified painter-order op list (2026-05-04) — **CLEARED**

Pre-refactor `Scene` carried six per-type Vecs (`rects`, `strokes`,
`gradients`, `images`, `shapes`, `glyph_runs`) and the rasterizer
walked them in a fixed cross-type order: rects → strokes →
gradients → images → shapes → glyph runs. Painter order was
implicit in primitive *type*, not consumer push order.

The first iteration of the demo's Card 6 made the failure mode
concrete: a magenta "badge" rect pushed *after* an image painted
*under* the image, because rects-before-images is a property of the
type-Vec design regardless of consumer intent. The matching note in
`p11prime_c_box_shadow.rs::p11c_01` flagged the same shape: a drop
shadow image had to land over (rather than under) its associated
card body, since rects come first.

Resolution: replaced the six Vecs with one `pub ops: Vec<SceneOp>`
where

```rust
pub enum SceneOp {
    Rect(SceneRect),
    Stroke(SceneStroke),
    Gradient(SceneGradient),
    Image(SceneImage),
    Shape(SceneShape),
    GlyphRun(SceneGlyphRun),
}
```

Every `Scene::push_*` helper appends one variant; the rasterizer
iterates `ops` once and dispatches per match arm. Tile-cache
dependency hashing (`hash_tile_deps`) and the per-tile filter
(`filter_scene_to_tile`) collapsed similarly — one walk over `ops`
replaces the six separate walks. Convenience iterators
`Scene::iter_rects`, `iter_strokes`, … re-expose the per-type view
where consumers want it (currently used only by tests).

`SceneOp` is now in the public surface alongside `Scene`.

Receipts:

- `netrender/tests/op_list_painter_order.rs` — three new tests:
  `op_order_01` proves a rect pushed after an image paints on top
  (the previous design failed this); `op_order_02` is the symmetric
  case (anchors the contract from the other side); `op_order_03`
  is a structural check that `Scene::ops` accumulates one entry per
  push helper, in call order, with the right variant per primitive
  kind.
- Demo Card 6: the badge rect now visibly paints over the image —
  the rendered PNG is the runtime-visible regression switch.
- Demo Card 5: the drop-shadow image is now pushed *before* the
  card body via a `ShadowDef` parameter to `build_cards`, so it
  sits under the card as CSS expects. Pre-refactor the shadow
  always painted over because images came after rects/gradients
  by type.
- Full workspace: 88 tests passing (was 85 + 3 new).

Migration was tightly scoped: 22 push-call sites in `scene.rs`, 3
iteration sites (`vello_rasterizer.rs`, `tile_cache.rs`,
`vello_tile_rasterizer.rs::filter_scene_to_tile`), and 3 test
files reading per-type Vecs (rewritten to use `iter_*`
accessors). No primitive structs changed; the variants are
pure carriers.

This refactor unblocks Phase 12b' (nested groups) — once Scene
holds an op list, push/pop scope ops slot in as additional
variants without further structural change.

### 11.12 Hit testing (2026-05-04) — **CLEARED**

Open question 3 in the original plan ("hit testing — what's the
return shape?") had been deferred pending consumer pull; the
op-list refactor (§11.11) made it the natural next step since
"top-most primitive at point" maps directly onto "last entry in
`Scene::ops` whose AABB contains the point."

API: `netrender::hit_test::{hit_test, hit_test_topmost,
HitResult, HitOpKind}` (re-exported at the crate root).

```rust
pub fn hit_test(scene: &Scene, point: [f32; 2]) -> Vec<HitResult>;
pub fn hit_test_topmost(scene: &Scene, point: [f32; 2]) -> Option<HitResult>;
```

The stack form is the primitive: returns every primitive covering
the point in top-most-first order. `hit_test_topmost` is the
short-circuiting common case for "what did the user click on."
`HitResult` carries an `op_index` (stable for the scene's lifetime)
and a `HitOpKind` tag mirroring `SceneOp` variants.

**Why a stack, not a single hit:** event bubbling, pick-through-
transparency, drag selection, hover targeting on overlay stacks —
all need to traverse from topmost down. Servo / WebRender's hit
test returns a stack with a short-circuit option; we follow that
shape. `single = .first()` is the special case; the reverse isn't
true.

Precision: AABB-level only.

- Rect / image / gradient: world-space AABB of the primitive's
  local rect, transformed via `scene.transforms`.
- Stroke: AABB inflated by `stroke_width / 2`. The interior of a
  stroked rect counts as a hit (typically what UI consumers want).
- Shape: bounding box of the path. Per-segment point-in-polygon
  is a future addition when consumer pull surfaces it.
- Glyph run: combined AABB of glyph origins, inflated by
  `font_size`. Per-glyph hit-testing needs real font metrics.

`clip_rect` (when set) gates inclusion: a point outside the clip's
AABB does not hit, even if the primitive AABB covers it. Rounded-
corner clips test against their AABB; refining the corner regions
is future work.

Receipts: 7 unit tests in `netrender/src/hit_test.rs::tests` —
empty scene, inside-rect, outside-rect, three-deep stack ordering,
top-most short-circuit, clip-rect exclusion, and mixed-kind stack.
Full workspace 95 tests passing (+7 vs §11.11's 88).

Future refinements (deferred):

- Per-glyph hit testing using the font's outline tables.
- Per-segment point-in-polygon for `SceneOp::Shape`.
- Honoring rounded-rect clip corners precisely (currently AABB).
- Coordinate-space helper for window-to-scene mapping (consumers
  do this themselves today).

### 11.13 Display-list format — discussion (2026-05-04)

After the op-list refactor (§11.11) the question "what's a
display list in this codebase" mostly answers itself: `Vec<SceneOp>`
*is* the display list. Two follow-up questions remain about the
consumer-facing shape; this section captures the design space so a
real consumer can pick.

**What the shape looks like in adjacent projects.** Cross-checked
to inform the decision, not to copy any of them wholesale:

- **WebRender display list** (Servo / Firefox): a flat
  `Vec<DisplayItem>` with rich CSS-shaped variants — `StackingContext`,
  `ScrollFrame`, `ClipChain`, `BoxShadow`, plus the leaf primitives.
  Tuned for cross-process bincode serialization (Servo's content
  process builds it, the GPU process consumes it). Heavy for a
  graphshell-scoped consumer that doesn't have a process boundary.
- **Skia `SkPicture`**: a recorded sequence of canvas calls,
  played back on demand. Same record-and-replay shape as our op
  list, with serialization layered on top. Validates the
  flat-op-list design.
- **Flutter layer trees**: compositor-shaped, not display-list-
  shaped. Different problem; not directly applicable.
- **SVG / CSS painter model**: document order = paint order; the
  spec is itself a flat-list-with-stacking-contexts model. Our
  op list maps onto it directly except for stacking contexts (the
  `push_layer` / `pop_layer` ops we'd add for §12b').

**Three options for the consumer-facing shape:**

A. **Push-helper-only (status quo).** Consumers call
   `scene.push_rect(...)` etc. `Scene::ops` is public for read,
   but consumers don't construct `SceneOp` variants directly;
   they go through the typed helpers. The display list is
   implicit — there's no "format" the consumer hands in.

   *Best for:* ad-hoc scenes, immediate-mode UI loops.

B. **`Vec<SceneOp>` is the canonical format.** Consumers can
   either use push helpers or build `Vec<SceneOp>` directly and
   replace `Scene::ops` wholesale. Recording is `scene.ops.clone()`;
   replay is `scene.ops = recorded`. Mutation is direct Vec
   indexing.

   *Best for:* persistent / mutable display lists, recording UIs,
   editor-shaped consumers that want to manipulate the list
   between frames. This is what `SkPicture`-shaped uses look like.

C. **Higher-level `DisplayItem` enum that lowers to SceneOp.**
   A semantic layer: variants like `Card { bounds, color, border }`
   that the consumer composes, with a translator emitting
   `Vec<SceneOp>`. Decouples the consumer's domain types from
   netrender's primitive types.

   *Best for:* a structured document model where consumer
   "intent" is meaningfully bigger than netrender's primitives
   (browser-style content, full DOM-equivalent representations).

**Recommendation:** ship Option B explicitly when a real consumer
needs it; don't pre-build C.

Concretely: `SceneOp` is already public and clonable. Treat it as
the canonical format. If a consumer surfaces "I want to record /
replay / serialize a scene" we add a thin recorder API
(`scene.snapshot() -> Vec<SceneOp>`, `scene.replay(&[SceneOp])`)
that's literally a Vec clone + assign. No format design work
required — the data type is the format.

Reject C until a real consumer actually has document types whose
mismatch with `SceneOp` is costly. If graphshell ends up wanting
e.g. `Node`-level display items (with edges, ports, labels as
substructure), that's a graphshell crate, not netrender — same
boundary as parley → netrender_text. The display list at the
netrender boundary stays primitive-shaped.

**Don't:** model on WebRender's display list. Stacking contexts
and scroll frames belong to a CSS-conformance project, which this
isn't. The op-list-with-future-push/pop-layer-variants design is
what we have, and it's what we should ship.

### 11.14 Nested layers + arbitrary-path clips (2026-05-04) — **CLEARED**

The op-list refactor (§11.11) ended with the explicit observation
that `push_layer` / `pop_layer` slot in as additional `SceneOp`
variants. Done in this pass:

```rust
pub enum SceneClip {
    None,
    Rect { rect: [f32; 4], radii: [f32; 4] },
    Path(ScenePath),  // Phase 9b'
}

pub struct SceneLayer {
    pub clip: SceneClip,
    pub alpha: f32,
    pub blend_mode: SceneBlendMode,
    pub transform_id: u32,
}

// New variants in `SceneOp`:
SceneOp::PushLayer(SceneLayer),
SceneOp::PopLayer,
```

CSS analogues map cleanly: `opacity` → `SceneLayer::alpha`,
`mix-blend-mode` → `SceneLayer::blend_mode`,
`clip-path` / rounded `overflow: hidden` → `SceneClip` variants,
`isolation: isolate` is the implicit effect of any non-trivial
layer.

The 9b' arbitrary-path clip is a sub-case of 12b': a layer with
`SceneClip::Path(ScenePath)`, alpha 1.0, blend mode Normal. No
separate per-primitive path-clip needed; the layer mechanism
covers it because layers can wrap one primitive just as well as
many.

Rasterizer dispatch: `SceneOp::PushLayer` → `vscene.push_layer`,
`SceneOp::PopLayer` → `vscene.pop_layer`. Debug-builds assert
push/pop balance at scene-translation time. Empty layers (no
inner ops between push and pop) are valid and produce no pixels.

Tile cache: `hash_push_layer` mixes the layer's clip / alpha /
blend / transform into the per-tile dependency hash, and
`SceneOp::PopLayer` contributes a marker byte. Dirty-tracking
treats layer changes as global (every tile inside the layer's
clip-AABB invalidates) — conservative but correct; refining to
clip-AABB-bounded invalidation is future work.

Tile filter (`filter_scene_to_tile`): always includes layer
push/pop ops in the filtered scene so balance is preserved per
tile. Layer's own clip narrows what pixels can be touched anyway.

Receipts (`netrender/tests/p12b_nested_layers.rs`):

- `p12b_01_alpha_layer_fades_inner_content` — alpha 0.5 layer
  wrapping a red rect over white bg produces mid-pink pixels.
- `p12b_02_rect_clip_layer_culls_outer_pixels` — rect clip culls
  pixels outside.
- `p12b_03_rounded_clip_layer_clips_corners` — rounded-rect clip
  produces visible corner clipping.
- `p9b_01_path_clip_layer_culls_outside_path` — triangle-shaped
  `ScenePath` clip wrapping a full-frame rect: only the triangle
  paints.
- `p12b_04_nested_layers_compose` — outer alpha + inner rect clip
  combine correctly (alpha-faded red inside the clip; bg color
  outside).

Plus 2 new hit-test unit tests:

- `layer_ops_skipped_in_hit_walk` — `PushLayer` / `PopLayer` ops
  don't generate hits themselves.
- `per_glyph_hit_returns_glyph_index` (see §11.15).

Caveat: hit testing does not yet honor a layer's clip (an inner
op is hit even if the layer's clip would have culled its pixels).
Documented in `hit_test.rs`'s module doc; future work is a
clip-stack-aware walk. Today's behavior is conservative — consumer
can post-filter the stack if they need clip-respecting hits.

### 11.15 Per-glyph hit testing (2026-05-04) — **CLEARED**

`HitResult` gained a `glyph_index: Option<usize>` field. For a
[`HitOpKind::GlyphRun`] hit, it's the index of the specific glyph
whose approximate AABB contains the point, or `None` if the point
is in the run's overall AABB but doesn't land on any individual
glyph (e.g., trailing whitespace or inter-glyph gap). `None` for
all other kinds.

Per-glyph AABB (no font metrics required): each glyph at
`(x, y)` gets a box

```text
(x, y - font_size, x + advance, y + font_size * 0.25)
```

where `advance = next_glyph.x - this_glyph.x`, or `font_size` for
the last glyph. A `0.25 * font_size` floor on advance keeps
combining marks / narrow glyphs clickable. This sketches an em-
box top-to-shallow-descender; real font metrics (via skrifa, which
parley already pulls in transitively) would tighten the box.
Deferred until a consumer needs the precision; for "click on this
character" UI the approximation is enough.

Receipt: `hit_test::tests::per_glyph_hit_returns_glyph_index` —
constructs a 3-glyph run at known x positions, hits each glyph's
box, verifies the returned `glyph_index`. Also confirms
`glyph_index = None` for non-glyph-run hits.

See §11.99 below for the consolidated open-items catalogue
(per-glyph metric refinement, point-in-polygon for shapes, etc.).

### 11.16 Polish sweep (2026-05-04) — **CLEARED**

Closed in one batch:

- **Edition bump.** All three workspace crates (`netrender`,
  `netrender_device`, `netrender_text`) moved from edition `2018`
  to `2021`. Unblocks `{var}` capture syntax in `format!` /
  `assert!`, IntoIterator-for-arrays, and the prelude additions
  (`TryFrom`, `TryInto`, `FromIterator`).
- **`Scene::clear_ops()`** — drops the op list without touching
  `fonts`, `transforms`, or `image_sources`. Lets streaming
  consumers do "rebuild ops per frame, reuse asset palette"
  without the boilerplate.
- **Layer-clip-aware hit testing.** `hit_test` and
  `hit_test_topmost` now run a forward pre-pass that tracks the
  active layer-clip stack at each op index, then a reverse pass
  that skips ops whose visibility is occluded by an enclosing
  layer's clip. Two new tests
  (`layer_clip_culls_inner_op_outside_clip`,
  `nested_layer_clips_intersect`) pin the contract: nested clips
  intersect, an outer clip culls inner ops correctly. AABB-only
  for non-axis-aligned clip shapes (rounded-rect corners and
  arbitrary path interiors register as visible at AABB level —
  same conservative tradeoff as elsewhere).
- **Decoration painting in `netrender_text`.** The parley adapter
  now emits underline / strikethrough rects from
  `Style::underline` / `Style::strikethrough` and the run's
  `RunMetrics`. Painting order matches the CSS text-decoration
  spec (underline → glyphs → strikethrough). Receipt at
  `netrender_text_03_decorations_emit_rects` checks the rect
  count, the brush colors, and the painter-order invariant.

105 tests passing across the workspace; 0 failures.

### 11.18 Color emoji / COLR fonts (2026-05-06) — **CLEARED**

Roadmap [B3 verification probe](2026-05-04_feature_roadmap.md):
*"vello + skrifa already handle COLR layer rendering on the glyph
path; we likely get this for free."*

**Verified.** A probe loading Segoe UI Emoji on Windows
(`C:\Windows\Fonts\seguiemj.ttf`, 12.4 MB), shaping `"😀🎉🌈"` via
parley at 48 px, rendering through `Renderer::render_vello`, and
reading back pixels measures **91% chromatic ratio** (4118 of 4524
painted pixels have channel divergence > 32 / 255). That is
overwhelmingly above the 5% threshold separating "COLR layers
honored" from "achromatic silhouette only." vello's GPU glyph
path renders peniko's COLR-decoded layers without any netrender-
side work.

Receipt at
[`netrender_text/tests/pb3_color_emoji_probe.rs`](../netrender_text/tests/pb3_color_emoji_probe.rs).
Skipped vacuously on hosts without one of the canonical emoji
font paths (Segoe UI Emoji / Apple Color Emoji / Noto Color
Emoji); CI that wants to enforce this should bundle Noto under
`tests/data/`.

No netrender-side work item. Re-run the probe on text-stack
changes (vello / skrifa / parley bumps) as a cheap regression
canary.

### 11.19 Selection rects + caret helpers (2026-05-06) — **CLEARED**

Roadmap [B1](2026-05-04_feature_roadmap.md): selection highlight +
caret emission for nematic's Gemini/Gopher/Scroll viewers,
Markdown editors, and feed readers.

`netrender_text` now exposes:

- `selection_rects(&Layout, Range<usize>) -> Vec<[f32; 4]>` — one
  rect per visual line that the byte range touches; thin wrapper
  over `parley::Selection::geometry`. Bidi-correct (RTL runs
  produce the right line-anchored bands by parley's own logic).
- `caret_rect(&Layout, byte_index, Affinity, width) -> [f32; 4]` —
  caret rectangle at a byte position; thin wrapper over
  `parley::Cursor::geometry`. Caret blink is consumer-side
  (alternate paint / no-paint at the platform's cadence); we just
  return the shape.

Both pure CPU, no GPU dependency. Receipts at
[`netrender_text/tests/pb1_selection_and_caret.rs`](../netrender_text/tests/pb1_selection_and_caret.rs)
cover collapsed ranges (empty), single-line bands, multi-line
bands ordered top-to-bottom, caret position at start, monotonic
caret advance through text, stable caret height across the same
line, and partial-vs-full line widths.

The roadmap had B1 framed as consumer-pull-gated ("nematic ships
shaped text via parley and asks for selection rects"). Closer
look at parley's API showed the trigger was protective rather
than technical: `parley::Selection::geometry` and
`parley::Cursor::geometry` already exposed exactly the right
shape, so wrapping them as netrender_text helpers was a
no-speculation ~30-line job.

### 11.20 Path-precise hit testing for shapes + path/rounded clips (2026-05-06) — **CLEARED**

Roadmap [R2 + R3](2026-05-04_feature_roadmap.md): tighten
`hit_test` from AABB-conservative to path-precise for arbitrary
`SceneOp::Shape` ops and for `SceneClip::Path` / rounded-rect
`SceneClip::Rect` clips.

Both fixes are thin wraps around `kurbo::Shape::contains`:

- `op_contains_point` for `SceneOp::Shape` now AABB-pre-passes,
  inverse-transforms the world point to the shape's local space,
  builds a `BezPath` from the `ScenePath`, and calls `contains`.
- `clip_aabb_contains_point` for `SceneClip::Path` does the same
  (BezPath::contains in local space). For `SceneClip::Rect` with
  non-zero radii, it builds a `kurbo::RoundedRect` and calls its
  `contains`. Sharp axis-aligned rects skip the path-precise check.
- Non-invertible transforms (degenerate scale, etc.) fall back to
  AABB-conservative — same protective default as before, just
  scoped to the cases where the inverse can't be computed.

The `transform_to_affine` and `build_bez_path` helpers from
[`vello_rasterizer`](../netrender/src/vello_rasterizer/mod.rs) were
promoted from module-private to `pub(crate)` so `hit_test` can
reuse them.

Receipts: 8/8 in
[`netrender/tests/pr2_pr3_path_precise_hits.rs`](../netrender/tests/pr2_pr3_path_precise_hits.rs)
covering triangle centroid hits, AABB-corner-but-outside-triangle
misses, transformed-shape path-precision, rounded-rect clip
corner-cutout misses, sharp-rect-clip unchanged behavior, path
clip path-precise, and a combined shape-inside-path-clipped-layer
case. The original AABB-only tests in `hit_test::tests` still
pass (the AABB pre-pass + path-precise refinement is a strict
narrowing).

This was another consumer-pull-gated item where the upstream API
was already in shape — a no-speculation ship-now per the
"consumer-pull gates need a sanity check" feedback memory.

### 11.21 Inline-box walker in `netrender_text` (2026-05-06) — **CLEARED**

Roadmap [R6](2026-05-04_feature_roadmap.md): expose a per-line
walker that surfaces glyph runs and inline-box placements in
visual order so consumers (graphshell-shaped, nematic, …) can
paint inline images / nested widgets / embedded layouts without
re-deriving line geometry.

`netrender_text` now exposes:

- `push_layout_with_inline_boxes(scene, registry, layout, origin,
  on_inline_box)` — single integrated walker. Glyph runs flow into
  the scene with the same logic as `push_layout` (font dedup,
  decorations, positioning); each `PositionedLayoutItem::InlineBox`
  fires the callback with a typed `InlineBoxPlacement` carrying
  scene-space coordinates (origin already applied), the
  consumer-supplied id, width, and height. Items emerge in parley's
  visual order (top-to-bottom by line, left-to-right within a line
  after BiDi reordering); inline boxes and glyph runs interleave
  in their natural order.
- `InlineBoxPlacement { x, y, width, height, id }` — scene-space
  placement record.

The plain `push_layout` / `push_layout_with_registry` entry points
are now thin wrappers around the inline-box-aware walker with an
empty callback — same behavior as before, no inline-box surface
exposed, no glyph-run emission duplicated. A new `emit_glyph_run`
internal helper holds the shared body.

Receipts: 6/6 in
[`netrender_text/tests/pr6_inline_box_walker.rs`](../netrender_text/tests/pr6_inline_box_walker.rs)
covering metadata round-trip (id, dimensions), origin-as-translation-
delta, glyph-run emission alongside callbacks, multi-box visual-
order ordering, the no-inline-box thin-wrapper case, and box-x in
layout bounds.

Same consumer-pull-gate sanity-check pattern: parley's
`PositionedLayoutItem` was already in shape; the wrap was no-
speculation work.

### 11.22 R9-canary wired (2026-05-06) — **CLEARED (trigger detector only; R9 itself remains blocked)**

Roadmap [R9-canary](2026-05-04_feature_roadmap.md): wire a CI
tripwire that signals the moment vello's GPU compute path starts
honoring `peniko::Gradient::interpolation_cs`. The wrap itself
(R9: `Scene::interpolation_color_space` field) stays parked until
the canary turns green.

Implementation:

- `linear-light-canary` cargo feature on the netrender crate; off
  by default so normal builds skip the canary entirely.
- `p1prime_03_canary_linear_light_is_honored` test gated under
  that feature, asserting the **fixed** behavior (LinearSrgb
  gradient midpoint differs from default by ≥ 16/255 per channel).
- Today the canary is **RED**: `mid_default = mid_linear =
  [127, 0, 128, 255]`, max_chan_diff = 0. Vello's GPU compute
  path still hard-codes `to_alpha_color::<Srgb>()` per
  `vello_encoding/src/ramp_cache.rs:86,97`.
- CI usage:
  `cargo test --features linear-light-canary -p netrender
   p1prime_03_canary_linear_light_is_honored`. Run on every
  vello-dep bump; failure today is informational, not a build
  block.

The canary panics with a loud RED-state message describing
exactly what's still missing and why R9 stays parked. When it
turns GREEN it prints a follow-up notice telling the next reader
to ship the R9 wrap and retire both the canary and the twin
`p1prime_03_gradient_default_is_srgb_encoded`. The two flip
together: when the canary greens, the twin starts failing because
the LinearSrgb-equals-default invariant breaks.

R9 itself remains [open on the roadmap](2026-05-04_feature_roadmap.md)
— this entry only clears the trigger-detector wiring.

### 11.23 Per-glyph hit testing via skrifa metrics (2026-05-06) — **CLEARED**

Roadmap [R1](2026-05-04_feature_roadmap.md): replace the em-box
approximation in `glyph_run_per_glyph_hit` with real font-supplied
glyph bounds.

`hit_test::glyph_run_per_glyph_hit` now opens the font via
`skrifa::FontRef::from_index(blob.data.data(), blob.index)` and
queries `GlyphMetrics::bounds(GlyphId)` per glyph. Bounds come back
in font (y-up) space; we mirror around the glyph origin's y to
match netrender's screen (y-down) convention.

Em-box fallback covers three cases: the `font_id == 0` sentinel,
font-parse failures (corrupt or empty bytes), and glyphs without
outline bounds (notably COLR emoji where the outline table is
empty — color emoji glyphs still hit the run-level AABB and the
em-box fallback is fine for those).

Added `skrifa = "0.42"` as a direct netrender dep (matching the
version vello pulls transitively).

Receipt at
[`netrender/tests/pr1_per_glyph_hit_metrics.rs`](../netrender/tests/pr1_per_glyph_hit_metrics.rs)
(3/3): a probe at the descender tail of a real 'g' hits under real
metrics where it would have missed under em-box; an above-glyph
point misses; a sentinel-font glyph still hits via em-box.

### 11.24 Image cache for the simple rasterizer (2026-05-06) — **CLEARED**

Roadmap [R4](2026-05-04_feature_roadmap.md): mirror
`VelloTileRasterizer::image_data` for the simple (non-tile) path.

New `vello_rasterizer::VelloRasterizer` struct holds
`image_data: HashMap<ImageKey, ImageData>` and
`image_overrides: HashMap<ImageKey, ImageData>` across calls.
`scene_to_vello(&mut self, scene)` refreshes the cache against
`scene.image_sources` (insert new, evict missing, no work for
unchanged keys), merges Path A + Path B, and calls a new
`scene_to_vello_with_cache` (extracted from
`scene_to_vello_with_overrides`).

Same Path B `register_texture` / `unregister_texture` interface as
the tile rasterizer. Existing `scene_to_vello` /
`scene_to_vello_with_overrides` free functions are unchanged
(back-compat).

Receipt at
[`netrender/tests/pr4_simple_rasterizer_image_cache.rs`](../netrender/tests/pr4_simple_rasterizer_image_cache.rs)
(7/7): cache fills on first call, stays stable across identical
calls, grows on new keys, evicts dropped keys, register/unregister
round-trips.

### 11.25 Stroke decorations (2026-05-06) — **CLEARED**

Roadmap [C1](2026-05-04_feature_roadmap.md): line caps, joins, and
dash patterns plumbed through to kurbo's stroke.

`SceneStroke` gained `cap: SceneStrokeCap`, `join: SceneStrokeJoin`,
`dash_pattern: Vec<f32>`, and `dash_offset: f32`. New netrender-
owned enums (`SceneStrokeCap` = Butt / Round / Square,
`SceneStrokeJoin` = Bevel / Miter / Round) keep peniko/kurbo out of
the Scene API. Mapped 1:1 to `kurbo::Stroke::with_caps` /
`with_join` / `with_dashes` in `emit_stroke`. Existing helper
constructors (`push_stroke`, `push_stroke_rounded`,
`push_stroke_full`) default to Butt / Miter / no-dashes. New
`Scene::push_stroke_decorated` takes cap / join / dash_pattern as
explicit arguments.

Tile-cache `hash_stroke` extended to include cap, join, dash
pattern, and dash offset — changes to any of these invalidate
covered tiles.

Receipt at
[`netrender/tests/pc1_stroke_decorations.rs`](../netrender/tests/pc1_stroke_decorations.rs)
(8/8): defaults documented, push_stroke_decorated applies args,
each of cap / join / dash_pattern / dash_offset triggers tile
invalidation, unchanged decorations keep tiles clean.

### 11.26 SceneOp::Pattern (2026-05-06) — **CLEARED**

Roadmap [C2](2026-05-04_feature_roadmap.md): repeated-tile fill
primitive.

New `ScenePattern { tile: ImageKey, extent: [f32; 4], scale: f32,
transform_id, clip_rect, clip_corner_radii }` struct, exposed as
`SceneOp::Pattern(ScenePattern)`. New `Scene::push_pattern(tile,
extent, scale)` helper.

Translation: `emit_pattern` builds an
`ImageBrush::new(img).with_extend(Extend::Repeat)` and fills the
extent rect. The `scale` is threaded as a brush_transform
(`Affine::scale(scale)`), so a unit step in image-pixel space
becomes `scale` units in scene-local space — a tile is
`image_size * scale` wide. Negative or zero `scale` clamps to 1.0
defensively.

Hit testing: `HitOpKind::Pattern` added; `op_contains_point` uses
the AABB of the extent rect.

Tile-cache: `hash_pattern` includes tile key, extent, scale,
transform, clip. `filter_scene_to_tile` intersects on extent
world-AABB. `dump_ops` (A1 inspector) labels the new op.

Receipt at
[`netrender/tests/pc2_pattern_op.rs`](../netrender/tests/pc2_pattern_op.rs)
(8/8): push appends a Pattern op, iter_images / iter_rects don't
see it, tile/extent/scale changes invalidate, hit-testing reports
Pattern, dump_ops labels it.

### 11.27 Variable fonts axis interpolation (2026-05-06) — **CLEARED**

Roadmap [C4](2026-05-04_feature_roadmap.md): thread variable-font
axis values through to vello's glyph path.

`SceneGlyphRun` gained `font_axis_values: Vec<(SceneFontAxisTag,
f32)>` where `SceneFontAxisTag = [u8; 4]` (ASCII bytes per the
OpenType spec; e.g., `*b"wght"` for the weight axis). User-space
values (e.g., 100, 400, 700 for weight) flow into the new
`compute_normalized_coords` helper which:

1. Parses the font with `skrifa::FontRef`.
2. Calls `font.axes().location(settings)` to do user→normalized
   conversion (skrifa's existing path; respects each axis's
   user-space range and avar mapping).
3. Extracts the `F2Dot14` coords as raw `i16` bits — the
   representation `vello::NormalizedCoord` (= i16) consumes.

`DrawGlyphs::normalized_coords` then receives the slice when
non-empty; empty axis values keep the font at its default location
(common case, no overhead).

New `Scene::push_glyph_run_variable` helper takes axis values
explicitly. `Scene::push_glyph_run` and `push_glyph_run_full`
default the field to empty.

Tile-cache `hash_glyph_run` includes axis values so weight / width
animations invalidate covered tiles.

Receipt at
[`netrender/tests/pc4_variable_fonts.rs`](../netrender/tests/pc4_variable_fonts.rs)
(7/7): default empty-axis-values, push_glyph_run_variable
applies args, each of value/tag/add invalidates, unchanged keeps
tiles clean. Plus a GPU smoke that loads Bahnschrift on Windows,
renders 'B' at wght = 300 / 400 / 700, and asserts bold paints
visibly more ink than light (the receipt the roadmap asked for).

### 11.28 Downscale-blur-upscale for large blurs (2026-05-06) — **CLEARED**

Roadmap [R5](2026-05-04_feature_roadmap.md): lift the σ-clip cap
beyond ~28 px so CSS-style large blurs (e.g.,
`backdrop-filter: blur(40px)` and bigger) reach their intended σ.

New `blur_kernel_plan_with_downscale(radius)` returns
`(level, passes, step_px)` where `level ∈ {1, 2, 4, 8}` is the
work-resolution divisor. The heuristic clamps `level` so the
*scaled* radius stays within the single-level cap (28 px). For
radii ≤ 224 px the cascade runs unclipped; beyond 224 the chosen
level (8) clamps and σ-clip returns — documented as the known
limit.

`Renderer::build_box_shadow_mask` rewritten to:

1. Render the (clip rectangle) mask at full `dim`.
2. If `level > 1`, prepend a brush_blur task at step=0 with output
   extent `dim/level` — bilinear sampler on the input texture acts
   as box-filter pre-AA.
3. Run the H+V cascade at `dim/level`.
4. If `level > 1`, append an upscale brush_blur step=0 task at
   output extent `dim` — bilinear smooths the upsample.
5. Register the final output via `insert_image_vello`.

Receipt at
[`netrender/tests/pr5_downscale_blur.rs`](../netrender/tests/pr5_downscale_blur.rs):
GPU smoke shows `blur_radius=64` produces an edge transition zone
substantially wider than `blur_radius=16` — without R5, the cascade
σ-clips at 28px and the difference would be marginal. CPU planner
receipts in `blur_plan_tests::pr5_*` cover level selection
(1/2/4/8) at characteristic radii.

### 11.29 Alpha-mask compose mode (2026-05-06) — **CLEARED**

Roadmap [C3](2026-05-04_feature_roadmap.md): mask-image fills
without pre-baking.

New `SceneCompose` enum (SrcOver default, DestIn for masks) added
as a per-layer field on `SceneLayer`. `SceneLayer::alpha_mask()`
constructs a layer with `compose: DestIn`; new
`Scene::push_alpha_mask_layer` helper appends one. The standard
outer-layer-then-content-then-inner-mask pattern in vello applies
without further changes (peniko's existing BlendMode supports
DestIn natively).

`emit_push_layer` now reads `layer.compose` and threads it through
to vello via the new `map_layer_blend(blend_mode, compose)`
function (replacing the SrcOver-hardcoded `map_blend_mode`).
`hash_push_layer` includes the compose byte so SrcOver vs DestIn
layers hash differently and trigger correct tile invalidation.

Receipt at
[`netrender/tests/pc3_alpha_mask_layer.rs`](../netrender/tests/pc3_alpha_mask_layer.rs)
(5/5): default compose check, alpha_mask helper produces DestIn,
push_alpha_mask_layer appends DestIn op, compose change
invalidates tile cache, and a GPU smoke that masks a red rect by a
half-and-half image and verifies the left half is red while the
right half is masked to near-black.

### 11.30 Backdrop filter via prefix-render + blur (2026-05-08) — **CLEARED**

Roadmap [D1](2026-05-04_feature_roadmap.md): CSS-style
`backdrop-filter` (frosted-glass blur of what's behind a
translucent rect).

New `SceneFilter::Blur(f32)` enum + `SceneLayer.backdrop_filter:
Option<SceneFilter>` field. The rasterizer multi-pass
orchestration lives entirely inside `Renderer::render_vello`:

1. Detect any layer carrying a `backdrop_filter` via
   `has_backdrop_filter(scene)` — fast path through the existing
   single-pass render when none.
2. For each backdrop-filter layer (painter order), build a
   "prefix scene" with `build_prefix_scene` — every op before the
   PushLayer, with unclosed scope balanced by appended PopLayers,
   and any prefix `backdrop_filter` stripped (D1 first-cut
   doesn't recurse).
3. Render the prefix to a fresh `Rgba8Unorm` texture at viewport
   dimensions via `render_scene_to_texture`.
4. Blur via the new `build_blurred_image` helper — same R5
   downscale-aware cascade as `build_box_shadow_mask`, but takes
   an arbitrary input texture as an `external` render-graph
   input.
5. Register the blurred texture as a fresh `ImageKey` (using a
   reserved range starting at `u64::MAX - 1` to avoid colliding
   with consumer keys).
6. Inject a `SceneImage` covering the layer's bounds (with UV
   sampling the corresponding region of the blurred prefix) right
   after the PushLayer in the processed scene.
7. Strip the backdrop_filter from the processed PushLayer so the
   inner render is the no-backdrop fast path.

Tile-cache `hash_push_layer` includes the backdrop_filter
discriminant + radius so changes invalidate covered tiles.

First-cut limit: each backdrop layer renders its prefix
independently (no sharing). For typical UI usage (one or two
backdrop elements per frame) this is fine; heavier consumers can
revisit caching when profiles surface it. Multi-level recursion
(backdrop layer's prefix containing another backdrop layer) is
guarded by stripping `backdrop_filter` from prefix layers — the
prefix renders without recursion, which is correct for the simple
case but may visually under-resolve nested filters. Document
when/if a real consumer hits it.

Receipt at
[`netrender/tests/pd1_backdrop_filter.rs`](../netrender/tests/pd1_backdrop_filter.rs)
(4/4): default-None + tile-cache invalidation on filter set /
radius change (CPU), and a GPU smoke that renders a 16-stripe
busy background twice (with and without `Blur(12)` covering a
horizontal band) and asserts the filtered band has <50% the
local horizontal variance of the reference. The "frosted-glass
nav bar over a busy background" receipt the roadmap asked for.

### 11.31 Animated values — `interpolate` module (2026-05-08) — **CLEARED**

Roadmap [D2](2026-05-04_feature_roadmap.md): CSS-style timing
curves and value interpolation for animated alpha / transform /
color values.

New [`netrender::interpolate`](../netrender/src/interpolate.rs)
module — pure functions, no clock, no scene-side state. The
consumer owns the time domain (winit frame timer, media-player
clock, scrubber state, replay) and uses these helpers to convert
a normalised `t ∈ [0, 1]` parameter into the eased value to push
into the next frame's Scene.

Provided:

- **Easing**: `linear`, `ease`, `ease_in`, `ease_out`,
  `ease_in_out`, `step_start`, `step_end`, plus the generic
  `cubic_bezier(p1x, p1y, p2x, p2y, x)` (Newton iteration on the
  x-axis bezier with bisection fallback, matching WebKit /
  Blink).
- **Lerp**: generic `lerp<T>(a, b, t)` for any
  `Add+Sub+Mul<f32>` type, `lerp_array<const N>` for fixed-size
  arrays, `lerp_color` for premultiplied RGBA (premult-aware
  blend, matching netrender's color contract).
- **Keyframes**: `sample_keyframes(&[(time, value)], t, default)`
  walks a sorted keyframe list and lerps between bracketing
  pairs.

Why no `Animated<T>` wrapper or scene-side animation field:
storing animation state on a Scene would couple rendering to
wall-clock time and break the A2 snapshot/replay determinism
invariant. Keeping the curves pure preserves "Scene is a frame
description; replays deterministically." Documented in the
module-level rustdoc.

Receipt: 14 unit tests under `netrender::interpolate::tests` —
endpoint clamping, ease symmetry, cubic-bezier extrapolation,
scalar / array / color lerps, keyframe edge cases (empty,
clamping, zero-span, easing composition).

### 11.32 Multi-thread scene building (2026-05-08) — **CLEARED**

Roadmap [E2](2026-05-04_feature_roadmap.md): per-thread scene
fragments + a join API that merges them into a parent Scene.

New `SceneFragment` struct mirrors Scene's op-list shape (ops,
transforms, fonts, image_sources) but omits scene-level state
(viewport, root alpha/blend, compositor surfaces) — those live
on the owning Scene. `SceneFragment::new` initialises with the
identity transform at index 0 and the sentinel font at index 0,
matching Scene's palette invariants.

`Scene::append_fragment(fragment)` does the merge with id
remapping:

- Fragment local index 0 (identity transform / sentinel font)
  stays at 0 in the parent scene.
- Fragment local indices ≥ 1 map to scene indices
  `(scene_old_len + k - 1)` — the parent extends with the
  fragment's `[1..]` slice, skipping the local identity entry.
- Image keys are caller-assigned `u64`s and aren't remapped;
  caller partitions the keyspace across threads.
- Append is order-preserving: fragment ops land at the end of
  `parent.ops` in the order `append_fragment` is called.

Helper functions `remap_op_ids` and `op_transform_id_mut` cover
the variant-dependent id rewrite (every visible-content variant
carries `transform_id`; only `GlyphRun` carries `font_id`;
`PopLayer` is a no-op).

Receipt at
[`netrender/tests/pe2_scene_fragment.rs`](../netrender/tests/pe2_scene_fragment.rs)
(9/9): empty fragment, single-fragment remap, identity stays at 0,
sentinel font stays at 0, two fragments keep transforms separate,
image-key collision overwrites + disjoint keyspace coexists, and
two parallel-build receipts — one with 4 threads × 100 rects each
(verifies deterministic join + per-quadrant colors), one with 4
threads × 2500 rects each (10k total) that exercises the
`SceneFragment` builder under a thread::spawn workload.

The roadmap framing was protective ("trigger: A4 data shows
scene-build CPU pressure"). The fragment API has value before
measured pressure shows up — it's just a structural decomposition
API. Shipping unblocks consumers who want it without forcing them
to wait for a profiler-surfaced receipt.

### 11.34 Phase A diagnostics + B2 scrolling — roadmap reconciliation (2026-05-08) — **CLEARED**

The Phase A diagnostics (A1 op-list inspector, A2 scene capture /
replay, A3 tile-dirty visualizer, A4 frame profiler) and B2
scrolling convenience were all shipped in lib code earlier in the
roadmap walk but the roadmap entries weren't migrated to CLEARED
form. This is purely a paperwork reconciliation finding plus one
test fix.

Test fix: [`pa2_scene_capture_replay.rs`](../netrender/tests/pa2_scene_capture_replay.rs)
constructed a `SceneLayer` literal that had bit-rotted when D1
(backdrop_filter) and C3 (compose) added fields. Added
`compose: Default::default()` and `backdrop_filter: None` to the
literal; 8/8 tests now green under `--features serde`.

Roadmap entries A1, A2, A3, A4, B2 moved from `[ ]` to `[x]
**CLEARED**` with receipts and lib-side line refs. No new code, no
behavioral change — just bookkeeping that matches the on-disk
state.

### 11.33 Library wasm32 readiness — `boot_async` (2026-05-08) — **CLEARED**

Roadmap [F2](2026-05-04_feature_roadmap.md): WebAssembly target was
framed as "real cost (wasm build infra), not a protective gate."
Audit showed otherwise.

`cargo check -p netrender --target wasm32-unknown-unknown` already
compiled clean before this finding. The only wasm-runtime hazards in
lib code were two `pollster::block_on` calls in
`netrender_device::core::boot` — `pollster` polls once and panics on
wasm32-unknown-unknown because the browser provides no executor.

Fix: split the boot into a portable async core
[`boot_async`](../netrender_device/src/core.rs) and gate the blocking
`boot()` wrapper to `#[cfg(not(target_arch = "wasm32"))]`.
`WgpuDevice::boot_async` mirrors the pattern. Browser consumers drive
`boot_async().await` from `wasm-bindgen-futures::spawn_local` (or any
executor); native consumers keep the existing blocking entry points
unchanged. Both `netrender_device` and `netrender` now `cargo check`
clean against `wasm32-unknown-unknown`.

The wasm-bindgen *demo* crate (running the card grid in a browser
canvas) remains gated on a real consumer commitment, but that's an
embedder example, not a netrender library cost. The library-side
readiness landed as a thin-wrap over `wgpu`'s already-async API,
similar in shape to B1, R1–R6, C1–C4, D1, D2, E2 — all of which
turned out to be protective gates rather than real costs once the
upstream API was checked first.

### 11.35 Dirty detection was the frame (2026-08-10) — **CLEARED**

Roadmap [E3](2026-05-04_feature_roadmap.md). Found by measuring E1
rather than by reading code, and it inverted what E1 assumed.

**How it surfaced.** E1 has said since 2026-05-04 that vello re-encodes
the whole scene every frame, that this is the cost of scrolling content,
and that the fix is upstream's. Building the evidence for that
conversation meant instrumenting the mostly-static case: hold damage at
one tile, sweep total tile count, read the A4 spans
([`../netrender/examples/e1_damage_profile.rs`](../netrender/examples/e1_damage_profile.rs)).

The premise did not survive. On an RTX 4060 / Vulkan, release, median of
30 frames, microseconds:

| viewport | tiles | ops | invalidate | rebuild | compose | vello | total |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 512² | 4 | 62 | 6.3 | 4.3 | 3.1 | 205.5 | 219.3 |
| 1024² | 16 | 296 | 40.5 | 5.4 | 6.7 | 221.0 | 275.2 |
| 2048² | 64 | 1094 | 295.2 | 8.4 | 18.6 | 226.1 | 545.8 |
| 4096² | 256 | 4422 | 3428.1 | 25.3 | 100.9 | 404.4 | 3939.0 |

vello grows 2× across a 64× increase in tiles, so it is not the
bottleneck for a static frame. Phase 7's tile cache is also working:
`rebuild` stays cheap because only dirty tiles are re-lowered. The cost
was **ours**. `hash_tile_deps` walked the entire op list once per tile,
recomputing each op's world AABB and full field hash every time — and
`world_aabb_glyph_run` and `hash_glyph_run` both walk every glyph in a
run, the shape equivalents every path segment. `invalidate` called it for
every tile, every frame: O(tiles × ops), both factors growing with page
area. At 4096² that is ~1.1M intersection tests, 3.4 ms, **~87% of the
frame**, to discover that one tile changed.

**The fix.** New [`tile_cache::index`](../netrender/src/tile_cache/index.rs)
inverts the loop. One O(ops) pass computes each op's world AABB and a
single `u64` digest of its fields, then files `(op_index, digest)` into
the bin of every tile the AABB covers. Each tile hashes only its own bin.
The tile-cover range is derived to agree exactly with `aabb_intersects`,
which is half-open on both rects, giving
`floor(a.x0/T) ..= ceil(a.x1/T) - 1`; degenerate and inverted AABBs fall
out of the same arithmetic, and non-finite bounds fall back to the whole
grid because over-marking costs a redundant re-lower while under-marking
leaves a stale tile on screen. Bins live on the `TileCache` so their
allocations survive across frames.

Result, `invalidate` column, same machine and scene:

| viewport | ops | before | after | per-op after |
| --- | --- | --- | --- | --- |
| 512² | 62 | 6.3 | 5.9 | 95 ns |
| 1024² | 296 | 40.5 | 26.9 | 91 ns |
| 2048² | 1094 | 295.2 | 97.9 | 89 ns |
| 4096² | 4422 | 3428.1 | 419.3 | 95 ns |

8.2× at 4096². The flat per-op figure across a 55× spread in tiles × ops
is the actual result: cost now tracks op count, not the product.
`invalidate` fell from ~87% of the frame to ~36%; whole-frame total from
~3.9 ms to ~1.15 ms.

**Two things preserved deliberately.** Painter order: bins are built by
ascending op index, and layer scope ops (which still apply to every tile)
are kept in a separate list and *merged* back by index at hash time.
Concatenating instead of merging would fail to notice a primitive moving
across a `PushLayer`, which changes the render. And the conservative
layer rule itself is untouched — tightening `PushLayer` to its clip AABB
is a separate change with its own correctness argument.

**Correctness gate.** The pre-E3 function is retained as
`hash_tile_deps_reference` under `cfg(test)` and 8 differential tests
assert the two implementations produce identical *dirty sets*. Not
identical hashes: the fast path mixes per-op digests instead of streaming
fields inline, so the test compares partitions, which is the semantics
that matters. Cases covered: unchanged scenes, colour change, positional
nudge, scene-level alpha, reordering two overlapping primitives, moving a
primitive across a layer boundary, off-grid and full-bleed primitives,
and degenerate or tile-aligned bounds. `pa3_tile_dirty_tracking` passes
unchanged (8/8), including
`moved_rect_under_stable_transform_id_dirties_its_tile`, which is the
receipt for the AABB-in-hash rule that `hash_aabb`'s doc comment exists
to explain.

**What is left.** The residual ~95 ns/op is this design's floor: every op
is digested once per frame because the consumer rebuilds the `Scene` from
scratch each frame, so there is no op identity to cache a digest against.
Going below it means incremental scene construction, which is a
consumer-side change, not a tile-cache one. E1 remains open and
upstream-gated, but the roadmap now carries a note not to cite scrolling
cost in that conversation, because these numbers would not support it.

### 11.36 Fragment retention spiked (2026-08-12) — **CLEARED (spike)**

Roadmap [E4](2026-05-04_feature_roadmap.md), design at
[`2026-08-10_fragment_retention_design.md`](2026-08-10_fragment_retention_design.md).
Implemented as a local demonstration ahead of E4's consumer-commitment
gate, with the profiler standing in as the consumer; the gate still
holds for productionizing the consumer contract.

**Mechanism.** `SceneOp::Fragment { id, transform_id }` (appended last,
so the serde wire encoding of prior variants is unchanged) references a
`Renderer`-owned registry: `register_fragment(SceneFragment) ->
FragmentId`, `update_fragment` (bumps a generation, drops the cached
lowering), `remove_fragment`. Scenes placing fragments take a retained
master path in `vello_tile_rasterizer::retained` and bypass the tile
cache: direct ops lower fresh in painter-order runs, placements append
the cached per-fragment `vello::Scene` under the placement affine
(mainline `vello::Scene::append` — no vello changes were needed), and a
whole-frame signature (per-op field hashes via the shared
`tile_cache::op_hash` dispatcher + resolved matrices + fragment
generations) short-circuits identical frames to a cached master.
Fragment-free scenes keep the exact pre-E4 tile path.

**Result** (same machine/run, RTX 4060 / Vulkan, release, median of 30
frames; this run's baseline is noisier than §11.35's, so compare within
it):

| 4096², 4423 ops | flat pan | fragment pan |
| --- | --- | --- |
| signature / invalidate | 841.9 | 1.7 |
| rebuild | 15302.5 | 0 |
| compose | 209.1 | 38.1 |
| vello | 883.7 | 672.8 |
| **total** | **17122.8** | **737.0** |

23× on the pan frame, fragment lowered once across 30 frames
(`lowr = 1`), and the signature is flat at ~1.5 µs across a 70× op-count
spread because it walks the *placed* scene (a placement + a caret), not
the content. E4's done condition asked for the pan row within ~2× of the
static row; it lands at 0.4× (737 µs vs 1922 µs), beating the static
tile path because it never pays invalidate. The frame is now ~91%
`vello_render`, which is E1's territory, upstream.

**Receipts.** `pe4_fragment_retention` (7/7), pixel-first: a placed
fragment renders **byte-identical** to the flat scene with the same ops
at every placement tried, including a placement composed over
fragment-local transforms and a fragment interleaved between direct ops
in painter order. Retention is asserted by counter
(`fragment_lower_count` stays 1 across placements, +1 per
`update_fragment`), master reuse by `fragment_master_hits`, and the
content-update path re-checks against a fresh flat reference. All
pre-existing receipts pass unchanged: 294 passing, 0 failed, 60 suites.

**Spike limitations, all deliberate, all warned on.** A placement inside
a `PushLayer` scope falls back to un-retained inlining (pixel-correct —
receipt covers it — but uncached, because the open layer lives in the
run's sub-scene and an append to the master would escape it). Nested
fragments are skipped at lower time. Hit testing does not resolve
fragment content (`hit_test` has no registry access — it reports
nothing for placed fragments; this is E4's largest remaining gap).
`hash_tile_deps_reference` and the E3 index carry defensive
conservative arms for fragments that should never reach them. The
cached-master hit clones the encoding per frame; if profiles ever show
the clone mattering, the fix is an append-from-cache path in
`compose_into`.

### 11.37 The scaled render stopped one row short of its target (2026-09-03) — **CLEARED**

Auto-DPI D2. A defect in `render_scaled_with`, found from outside: genet's
host-zoom work measured **1120 fully transparent pixels** in a headed capture
at layout scale 1.8 and traced them here
(`genet/design_docs/2026-09-03_host_ui_zoom_plan.md`, Findings).

**The mechanism.** A host lays out at `physical / scale` and carries the
result as an integer viewport, so the division has already truncated before
the scene arrives. `render_scaled_with` then rebuilt the render size as
`round(viewport * scale)` and handed *that* to `RenderParams`. The two numbers
are not the same: 800 physical over 1.8 lays out at `trunc(444.44) = 444`, and
`round(444 * 1.8)` is 799, so vello was asked for 799 rows of an 800-row
texture and the last one kept the texture's zero fill. The 1120 pixels are one
row of an 1120x800 frame — the width survived (`round(622 * 1.8) = 1120`), only
the height lost.

Not a zoom defect and not new. The loss fires wherever the scale does not
divide the physical size: every fractional scale in practice — a 150% display
reproduces it with no content zoom at all (`round(666 * 1.5) = 999`) — and an
odd surface at device scale 2.0 as well. Zoom only made fractional layout
scales ordinary instead of rare, so **every consumer of `render_vello_scaled`
was affected before this**.

**The repair.** The render size is the target's. `render_scaled_with` reads it
off `target_view.texture()` (wgpu 30's `TextureView::texture()`, backend-neutral)
rather than re-deriving a number the caller has already rounded. `scale` keeps
its one job, the root affine. The doc contract on `render`, `render_scaled` and
`Renderer::render_vello{,_scaled}` now says the target view's texture dimensions
ARE the render size, and asks for a full mip-0 view — every texture netrender
and its hosts create is `mip_level_count: 1`, so that costs nothing.

The API did not change and genet was not touched: the physical size was already
in netrender's hands, on the view it was handed. **One site only** — the same
`viewport * scale` arithmetic appears nowhere else. `render_overlay_fragment`
and `render_to_internal_master` size themselves from the viewport too, but they
allocate their own target from those same numbers and take no `scale`, so they
are self-consistent (and, being scale-free, the path-b′ compositor path cannot
honour a layout scale at all — a separate gap, not this one).

**Receipts.** `fractional_scale_target_coverage` (4/4), two CPU and two GPU.
The GPU pair renders a logical square into a physical-square target through
`Renderer::render_vello_scaled` and counts pixels vello was never asked to
write, which is exactly the host capture's measurement. Instrument proved
against the old arithmetic first: it reports **1599** untouched pixels
(800² − 799²), first at (799, 0). The CPU pair pins the no-op claim — the old
derivation already landed on the target at scale 1.0 for every size and at any
integer scale that *divides* the surface, so those hand vello the same number.
63 suites, **304 passing, 0 failed, 1 ignored**; the 300 pre-existing tests are
unchanged.

Headed, through genet's host smoke on a 200% display (artifacts in
`Code/testing/genet/ui_zoom/`): the zoom-0.9 `resized` frame goes
`transparent=1120 alpha=0..255` → **`transparent=0 alpha=255..255`**, and its
`busy` frame is byte-identical (0 differing bytes) because 1800x1280 already
divided exactly.

**A note on that instrument.** The zoom-1.0 no-op was *not* provable by digest
comparison, and the plan's claim that `busy`/`resized` are byte-stable across
runs does not survive n=6: the **same unchanged binary** returns `busy` in
{`c0d00467bf8493b3`, `27c01a961612a3ba`} and `resized` in
{`6f77dbe3db6fba94`, `c7369bb9c9ddb531`}, the pairs differing by exactly **one
byte** — one channel of one pixel, ±1. A build carrying only this change was
seen on both sides of that coin, and a control build carrying the *old*
arithmetic reproduced the "after" digest too. What settles it instead is the
mechanism: a temporary diagnostic that logged every call where the derived size
disagreed with the target emitted **nothing at all** across a full zoom-1.0
scenario, so the numbers reaching vello are bit-identical there. Digest equality
on these captures is worth about ±1 pixel; prefer the mechanism.

### 11.38 Render graph refuses malformed work (2026-09-04) — **CLEARED**

Execution-graph plan RG0, commit `fa5526051`. The Phase 6 graph previously
collected tasks into a `HashMap`, silently replaced duplicate IDs, omitted
unknown inputs through `filter_map`, and returned a partial schedule when a
cycle blocked work. Its documented insertion-order tie break also depended on
`HashMap` iteration and therefore was not true.

The repair keeps the public `Task` shape while using insertion-indexed storage
and a stable Kahn queue. `RenderGraph::execute` now validates before creating
an encoder and returns `Result<_, RenderGraphError>`. Typed errors cover
duplicate task IDs, missing inputs, dependency cycles, and a collision between
an imported texture ID and a task output ID. That fourth case came from the
independent adversarial review: the old map insertion would silently replace
the imported texture. Callback views are now built by mapping every declared
input in its original order, including repeated inputs.

All in-repo graphs are hardcoded renderer machinery, so their callers use
specific `expect` messages after construction. The fallible public API is
intentional: an outside caller of `RenderGraph::execute` must now handle a
malformed graph rather than receiving incomplete output.

**Receipts** (isolated target directory, `-j 1`):

- `cargo test -p netrender --lib render_graph::tests` — **6 passed**, covering
  deterministic ready order, duplicate IDs, external/task collision, missing
  input, repeated input, and a direct cycle;
- `cargo test -p netrender --test p6_render_graph` — **2 passed**;
- `p9a_clip_rectangle` + `p9b_box_shadow` + `p9c_clip_fast_path` — **6 passed**;
- `p11prime_c_box_shadow` + `p9prime_rounded_clip` +
  `pd1_backdrop_filter` + `pr5_downscale_blur` — **11 passed**;
- `cargo check -p paint_list_render -p netrender_text` — passed;
- `cargo fmt --check` and `git diff --check` — passed.

The public deterministic plan dump, typed resources, requested-output culling,
and separate compile/execute phases remain RG1 work. RG0 establishes the
admission boundary they require.

### 11.39 RG1 compiles and measures an image plan (2026-09-04) — **CLEARED**

Execution-graph plan RG1 first slice, commit `975d9df4f`. The Phase 6 graph
previously combined graph construction, sorting, texture allocation, encoding,
and submission in one call over raw `u64` IDs. It could reject malformed work
after RG0, but it could not inspect or cull a selected plan, describe logical
resource lifetimes, or measure allocation pressure before touching the GPU.

The new crate-private plan path gives every graph a process-local `GraphId` and
every imported or transient image a graph-local `ImageNode`. Tasks declare
named sampled reads and color-target writes. Compilation starts from requested
outputs, removes disconnected work and imports, preserves insertion order for
independent ready tasks, and emits a deterministic dump of resources, accesses,
edges, selected outputs, lifetimes, the encoder batch, and its submit boundary.
Foreign handles remain invalid in release builds.

Admission now refuses missing or duplicate producers, dependency cycles,
invalid input/output access direction, texture descriptors missing required
attachment or sampled usage, loads from fresh transient outputs, imported task
outputs, and mixed legacy/planned modes. Execution validates every reachable
physical import's size, format, and required usage before creating an encoder.
Callbacks remain trusted crate-private machinery and must begin and close their
own pass; the public raw callback API remains only for unmigrated compatibility
callers.

`ExecutionReport` separates compile, allocate, encode, and submit host
durations. Its allocation fields are descriptor-derived logical evidence, not
GPU timing or physical allocator measurements: creation count and estimated
bytes, plus peak-live count/bytes globally and per exact size/format/usage
descriptor. Footprints use the format's block dimensions and copy-block size;
formats without one report byte values as unavailable. The committed 2x2
RGBA8 fixture compiles input -> mask -> horizontal blur -> vertical blur ->
color matrix while culling another branch. It reports **4 creations / 64
logical bytes** and **2 peak-live images / 32 logical bytes** for one descriptor.

`build_blurred_image` is the first migrated runtime consumer. Its transient
descriptors retain `COPY_SRC` because the surrounding Vello path copies the
exported result. An adversarial run caught that requirement before commit: the
first draft passed the graph tests but failed wgpu validation in the headed
backdrop-blur smoke.

**Receipts** (isolated target directory, `-j 1`):

- `cargo test -p netrender --lib render_graph::tests` — **11 passed**,
  including selected-subgraph culling, four-node lifetime/allocation metrics,
  foreign-node refusal, deterministic sibling order, access/usage/load
  refusal, and physical-import metadata matching;
- `cargo test -p netrender --test p6_render_graph` — **2 passed** on the
  compatibility path;
- `cargo test -p netrender --test pd1_backdrop_filter` — **4 passed**,
  including the migrated backdrop-blur GPU smoke;
- `cargo check -p netrender`, `cargo fmt --all -- --check`, and
  `git diff --check` — passed.

This clears the RG1 first-slice receipt, not the whole phase. Color-matrix and
box-shadow/clip builders still use the compatibility graph, so retiring raw
task IDs and callbacks waits for their migration. Physical reuse waits for the
new report. At this receipt point, the first honest combined
backdrop-plus-element fork/join still needed explicit Vello execution
boundaries. RG2b later delivered those boundaries; RG2c now owns the actual
multi-input effect graph.

### 11.40 Production filters use the RG1 image plan (2026-09-04) — **CLEARED**

RG1 production migration, commit `69b9179c0`. The remaining runtime users of
the legacy raw-task graph were `build_color_matrix_image` and
`build_box_shadow_mask`. Both now declare logical images and sampled/color
accesses, compile a selected plan, and execute it through the RG1 path.

The color-matrix plan binds one imported image and selects one transient
output. The box-shadow plan declares its generated mask plus optional
downsample, blur pairs, and upscale as transient images. Every descriptor keeps
the former `RENDER_ATTACHMENT | TEXTURE_BINDING | COPY_SRC` usage contract;
the last flag matters when a result crosses into Vello or a readback receipt.

A new Scene-level GPU receipt closes a pre-existing evidence gap:
`pd2_element_filter` puts opaque red content in a layer carrying
`SceneFilter::Invert(1.0)` and observes cyan output pixels. This proves the
renderer preprocessing path, the migrated color-matrix plan, texture
registration, and final composition together rather than testing only a
callback. Existing Scene-level backdrop blur and production box-shadow/large
blur receipts cover the other migrated builders.

The scoped-encoder experiment did not earn a new abstraction. Current
callbacks begin and drop their pass within one call. A facade that retained raw
`CommandEncoder` access would only rename that convention; a facade that owned
the pass would require a broader lifetime/resource preparation redesign. The
planned builder and executor remain crate-private, so this trusted callback
shape is not presented as tenant isolation or a public contract.

**Receipts** (isolated target directory, `-j 1`):

- `cargo test -p netrender --lib render_graph::tests` — **11 passed**;
- `cargo test -p netrender --test pd2_element_filter` — **1 passed**;
- `cargo test -p netrender --test pd1_backdrop_filter` — **4 passed**;
- `p11prime_c_box_shadow` plus `pr5_downscale_blur` — **4 passed**;
- `p6_render_graph` plus `p9a_clip_rectangle`, `p9b_box_shadow`, and
  `p9c_clip_fast_path` — **8 passed** on the unchanged compatibility path;
- `cargo check -p netrender`, `cargo fmt --all -- --check`, and
  `git diff --check` — passed.

Every production filter builder now uses the image plan. The public
`Task`/`TaskId` compatibility API remains solely for the four direct GPU test
files above. RG1 closes after those assertions are re-homed behind the
crate-private planned path and the raw API has no in-repo caller. The logical
report's four creations versus two peak-live images makes transient pooling a
measurement candidate, not yet an implementation mandate.

### 11.41 RG1 retires the legacy graph API (2026-09-04) — **CLEARED**

Compatibility retirement, commit `0af85a62f`. The two Phase 6 and six Phase 9
GPU assertions now live under crate-local `render_graph_tests` and construct,
compile, and execute typed image plans. Their useful pixel oracles remain;
they no longer keep raw task IDs, graph mutation, encoder callbacks, or the
graph module public.

The public `Task`, `TaskId`, `RenderGraphError`, `RenderGraph::push`, legacy
`execute`, and root re-exports are removed. `filter` and `render_graph` are
private modules, and the filter callback constructors plus `EncodeCallback`
are crate-private. Repository search found no remaining source or CI caller.

Moving the GPU receipts into one library test binary exposed access violations
when the default Rust test harness ran independent device tests concurrently.
A crate-local `OnceLock<Mutex<()>>` now serializes only these eight GPU tests;
the default parallel harness passes without a global `--test-threads=1`
setting.

**Receipts** (isolated target directory, `-j 1`):

- `cargo test -p netrender --lib render_graph::tests` — **7 passed**;
- `cargo test -p netrender --lib render_graph_tests` — **8 passed** under the
  default parallel test harness;
- `pd1_backdrop_filter` + `pd2_element_filter` +
  `p11prime_c_box_shadow` + `pr5_downscale_blur` — **9 passed**;
- `cargo check -p netrender`, `cargo fmt --all -- --check`, and
  `git diff --check` — passed.

RG1 is complete. At this commit, the logical allocation report still needed a
repeated-plan timing or memory-pressure measurement before RG5 pooling could be
admitted; §11.42 supplies that measurement and keeps RG5 deferred. This
source-breaking removal is unreleased relative to `netrender 0.1.2`; any
release containing it must be `0.2.0` and align the `netrender_text` and
`paint_list_render` constraints in the release pass.

### 11.42 Repeated-plan allocation does not activate RG5 (2026-09-05) — **CLEARED**

Measurement harness and production-helper split, commit `35cb54ea5`. The
ignored `rg1_repeated_box_shadow_measurement` uses the same crate-private graph
builder and executor as public `Renderer::build_box_shadow_mask`, while keeping
the public Vello image registry outside the repeated loop. The public API and
production output path are unchanged.

One booted NVIDIA GeForce RTX 4060 Laptop GPU, Vulkan backend, NVIDIA driver
610.88 ran two release workloads. Each used 16 fully completed warmups and 64
fully completed samples, with a bounded five-second `device.poll` after every
submission. The harness reports construction, compile, allocation, encode,
submit, residual execute overhead, completion, and total-to-completion without
outlier deletion. Pixel/readback oracles for both workloads run outside the
timed sample sets.

| Workload | Allocation median / p95 | Host-work p95 | Total median / p95 | Creations | Projected peak |
| --- | ---: | ---: | ---: | ---: | ---: |
| 256 x 256, blur 16 | 0.067 / 0.111 ms | 1.349 ms | 1.429 / 1.978 ms | 33 | 2 |
| 1024 x 1024, blur 64 | 0.055 / 0.102 ms | 1.091 ms | 1.276 / 1.751 ms | 35 | 2 |

The large row contains two 1024 x 1024 creations with projected peak 1 and 33
256 x 256 creations with projected peak 2. The small row has 33 exact 256 x
256 creations with projected peak 2. These establish structural reuse
pressure. They do not establish physical peak residency: the executor still
creates every selected task output before encoding, so logical peak-live is a
projected lower bound for a possible pool.

The materiality rule was fixed before the run. Given exact-descriptor pressure,
allocation p95 must reach either 2% of `NETRENDER_RG_FRAME_BUDGET_MS` (0.333 ms
at the default 16.667 ms) or both 15% of host-work p95 and a 0.10 ms floor.
Both rows return `NOT_MATERIAL`. RG5 texture pooling stays deferred; RG2a is the
next execution-graph slice.

**Receipts** (isolated target directory, `-j 1`):

- `$env:WGPU_BACKEND='vulkan'; cargo test -p netrender --release --lib
  render_graph_tests::rg_measurement::rg1_repeated_box_shadow_measurement -j 1
  --target-dir C:\Users\mark_\.cargo-target-rg-measure -- --ignored --nocapture
  --test-threads=1` — **1 passed**, including both out-of-band mask oracles;
- `p11prime_c_box_shadow` + `pr5_downscale_blur` — **4 passed**; the large-blur
  receipt retained max alpha 139, scanline paint 11298, and transition widths
  44 at blur 16 versus 128 at blur 64;
- `cargo test -p netrender --lib render_graph` — **15 passed, 1 ignored**;
- `cargo fmt --all -- --check` and `git diff --check` — passed.

### 11.43 RG2a three-backend scene conformance (2026-09-05) — **CLEARED**

Rasterizer-independent corpus and admission refinement, commit `b06a21407`.
One feature-gated test binary now sends the same two resource-free direct
`Scene` fixtures through Classic, Hybrid, and CPU. It boots one shared wgpu
device for both GPU paths and renders CPU independently. Each backend is judged
against semantic regions rather than another rasterizer's bytes.

The corpus covers solid and stroked geometry, transforms, device-space clips,
filled and stroked paths, linear/radial/conic gradients, rounded layer clips,
and nested alpha layers. Tightening the transformed-clip anchors first failed
on Hybrid. The shared sparse lowerer had recorded primitive clips under the
primitive's world transform, while Classic correctly treats `clip_rect` as
device-space. Rect, Stroke, Shape, and Gradient now record their clip under
identity and restore the primitive transform before painting; each path has a
positive and clipped-out corpus anchor.

`BackendCapabilities` now distinguishes transforms, clips, nested layers,
patterns, element filters, backdrop blur, and backdrop color filters. The last
distinction closed an existing overclaim: Classic implements element filter
chains and backdrop blur, but its backdrop preprocessing skips color filters.
It now reports `backdrop_color_filters = false`, and operation-level
`validate_scene_for_backend` returns a typed refusal instead of claiming that
semantic operation is supported. The validator explicitly excludes Classic
registry/resource state; the registry-bearing `Renderer` remains authoritative
for external images and retained fragments.

The refusal table checks both sparse adapters for images, patterns, glyph runs,
retained fragments, element filters, and backdrop filters. Invalid transforms,
early and end-of-scene layer imbalance, and oversized sparse viewports carry
backend identity and an operation index where one exists.

**Measured row:** NVIDIA GeForce RTX 4060 Laptop GPU, Vulkan, NVIDIA 610.88;
wgpu 30; Classic `netrender-vello` 0.10.0; Hybrid and CPU `vello` 0.2.0 at
`ca3f40ea182216883cd543c7b9deae991268917c`.

**Receipts** (isolated target directory, `-j 1`):

- `cargo test -p netrender --features vello-all --test rg2a_scene_corpus --
  --nocapture --test-threads=1` — **2 passed**;
- `cargo test -p netrender --features vello-all --lib -- --test-threads=1` —
  **70 passed, 1 ignored**;
- `p3_transforms` + `p8prime_vello_gradients` + `p9prime_rounded_clip` +
  `p12b_nested_layers` — **20 passed**;
- all-Vello and default `cargo check`, `cargo fmt --all -- --check`, and
  `git diff --check` — passed.

### 11.44 RG2b Vello execution boundaries (2026-09-05) — **CLEARED**

Three-backend execution-boundary proof, commit `cfa0261c2`. The literal first
draft of RG2b conflicted with RG2a: Hybrid and CPU correctly refuse a
filter-bearing `PushLayer`. The delivered receipt keeps that refusal and
separates two questions:

- one direct `Scene` combines backdrop blur with an element `Invert` filter;
  Classic renders it, while Hybrid and CPU return exact backend-attributed
  typed refusals for the same operation;
- a separate filter-free direct `Scene` supplies the scheduling fixture. All
  three backends feed one downstream blur plan and visible readback on the same
  `WgpuHandles`.

This clears the execution-boundary slice only. The shared graph in this
receipt is unary, and external composition is encoder-participating but not a
logical graph node. RG2c retains the promotion gate: a physical two-input
backdrop-plus-element effect graph with a real join. RG3 may proceed as a
closed-tenant integration proof, but neither slice substitutes for RG2c when
evaluating general graph claims or extraction.

Classic performs its opaque Vello submission before the graph batch. Hybrid
records rasterization, external-texture composition, and graph work into one
caller-owned encoder. CPU rasterizes to a host pixmap, enters through an
ordered ready queue upload/import, then records external composition and graph
work in one encoder. `ExecutionPlan::encode_into` supplies the shared-encoder
seam. External-texture composition now has an encoder-participating form while
the existing public helper preserves its convenience submit. Plan dumps name
the selected rasterizer and producer boundary; batch and submission counts are
labeled as graph-segment counts.

The combined Classic receipt uses partial-alpha element content so the
backdrop remains observable. Measured anchors were center
`[161, 180, 59, 255]` versus `[215, 161, 40, 255]` without the element filter,
and boundary `[40, 59, 180, 255]` versus `[0, 19, 220, 255]` without backdrop
blur. This proves both effects causally rather than merely checking that the
frame is nontransparent.

**Measured row:** NVIDIA GeForce RTX 4060 Laptop GPU, Vulkan, NVIDIA 610.88;
wgpu 30; Classic `netrender-vello` 0.10.0; Hybrid and CPU `vello` 0.2.0 at
`ca3f40ea182216883cd543c7b9deae991268917c`.

**Receipts** (isolated target directory, `-j 1`):

- focused RG2b unit/semantic tests — **2 passed, 1 physical receipt ignored**;
- explicit physical three-backend RG2b receipt — **1 passed**;
- full all-Vello library suite — **72 passed, 2 physical receipts ignored**;
- RG2a scene corpus — **2 passed**;
- all-Vello and default `cargo check`, `cargo fmt --all -- --check`, and
  `git diff --check` — passed.

## External canvas scene composition — CLEARED (2026-09-11)

Host textures can now be GPU-staged as normal scene images. The paint-list
translator preserves their command position, active transform, and enclosing
clip/opacity layers. Staging accepts a sampled source without `COPY_SRC` and
converts premultiplied pixels to the straight-alpha RGBA8 input required by
Vello's image atlas. Two cached alpha pipelines avoid per-frame shader creation.
Unchanged source generations skip staging; changed generations refresh the
atlas, and new identities or removal invalidate tile and retained-fragment
lowerings. Producer keys occupy a checked lower-62-bit namespace.

With Rust 1.97.1, the focused GPU library test
`external_image_stages_premultiplied_pixels_inside_scene_layers_at_fractional_dpr`
passed on an actual required adapter. It checks analytic RGBA and adjacent clip
edge pixels at scales 1, 1.5 and 2, unresolved-to-live registration, same-size
refresh, resize and removal. Both `external_image_key` tests and both
`paint_list_render` external-texture tests passed. GPU logs are preserved at
`Code/testing/genet/ortet-compositing-netrender/`.

The opacity/source-over oracle follows
[CSS Color 4](https://www.w3.org/TR/css-color-4/#transparency) and
[Compositing and Blending 1](https://www.w3.org/TR/compositing-1/#simplealphacompositing).
Native host acceptance belongs to Genet's Ortet plan. The legacy direct-overlay
API retains its flat composition contract; callers must choose one composition
path per producer. GPU staging and atlas copying are additional work, without
CPU readback; this receipt makes no performance or full WebGL conformance claim.

## 11.99 Open items — moved (2026-05-05)

The catalogue of deferred refinements that originally lived here
has been folded into the feature roadmap as
[Phase R in `2026-05-04_feature_roadmap.md`][roadmap-r] so all
open items live in one place.

The originally-deferred items (12c' backdrop filter, 13'
compositor handoff, linear-light blending) all activated 2026-05-05;
their canonical entries now live on the roadmap as **D1**, **D3**,
**R9** respectively. The activation-history record is preserved in
[`archive/2026-05-05_deferred_phases.md`](archive/2026-05-05_deferred_phases.md).
The path (b′) design for D3 lives in
[`2026-05-05_compositor_handoff_path_b_prime.md`](2026-05-05_compositor_handoff_path_b_prime.md).

When a wart fix from Phase R lands, record it as a `§11.x —
CLEARED` finding here and remove it from the roadmap. When a
deferred-phase item lands (D1 / D3 / R9), do the same and update
the relevant follow-up plan.

[roadmap-r]: 2026-05-04_feature_roadmap.md
