# netrender notes index

This file is the short index to the surviving plans in
`netrender-notes/`. The repo is post-cleanup: vello is the sole
rasterizer on `main`, the upstream WebRender codebase (`webrender_api/`,
`wrench/`, `wr_glyph_rasterizer/`, etc.) has been removed. To spelunk
through the original implementation, check out the
`archive/webrender-0.62-pre-phase-d` tag. (This used to say the code lived
on a `webrender-wgpu-upstream/` side worktree. That checkout is gone; see
[`archive/2026-08-10_branch_archive.md`](archive/2026-08-10_branch_archive.md).)

## Release integration, 2026-08-25

The 0.1.2 release stack is the mainline package contract: `paint_list_api`,
`paint_list_render`, `netrender`, and `netrender_device` carry their published
versions, and the root consumes the published wgpu-30 `netrender-vello` fork.
The regenerated lock records those sources. This closes the stale split where
crates.io consumers selected `netrender` 0.1.2 while git `main` still declared
0.1.1, producing two incompatible `Scene` types in Turnstone.

Unreleased `main` now removes Netrender's public legacy render-graph helper
API. The first release containing that source-breaking change must publish
`netrender` as 0.2.0 and align the `netrender_text` and `paint_list_render`
dependency constraints in the same release pass.

## Current canonical plans

Audited 2026-08-10. Four files, each with one job, plus a research
record at the end.

- [`2026-05-04_feature_roadmap.md`](2026-05-04_feature_roadmap.md)
  — **the only live checklist.** Phase R plus Phases A–G, every entry
  with a trigger and a done condition. Start here to answer "what is
  left". As of 2026-08-10 the answer is **nothing in this repo**: the two
  remaining items are **D3** (native-compositor handoff, netrender side
  complete, genet adapter 5.5 outstanding and out-of-repo) and **R9**
  (linear-light blending, upstream-blocked on vello's compute path,
  R9-canary will fire when it clears). Everything else in Phases
  0.5'–12b' has shipped with receipts.

  A5 and E3 both opened and closed on 2026-08-10. E1 stays open but is
  upstream-gated and, per its own measurement, not the interesting
  number. **E4** (fragment retention) is **spiked as of 2026-08-12**:
  the pan case that re-lowered the world (17.1 ms at 4096²) runs at
  737 µs with the page retained as one fragment, on mainline vello
  (no fork — `Scene::append` was the only primitive needed). Receipts
  in `pe4_fragment_retention` (7/7), finding at verification record
  §11.36. **Sprigging committed as the first consumer 2026-08-12**; the
  bridge (`translate_paint_cmds_to_fragment` +
  `SceneFragment::from_scene`) and the sprigging-shaped e2e receipt
  live in `paint_list_render`. Genet-side host wiring waits on the
  cambium tree settling. Remaining gaps: hit-test resolution for
  fragments, layer-scoped retention, the host profile. See
  [`2026-08-10_fragment_retention_design.md`](2026-08-10_fragment_retention_design.md) §4.5.

- [`2026-05-01_vello_rasterizer_plan.md`](2026-05-01_vello_rasterizer_plan.md)
  — **the live architecture.** The vello pivot, adopted and delivered.
  Phase 7' (Masonry pattern tile cache) is its architectural heart. §12
  is the phase mapping and the accurate status table.

- [`2026-05-01_vello_verification_record.md`](2026-05-01_vello_verification_record.md)
  — **the evidence.** Split out of the plan on 2026-08-10, where it was
  62% of the file. 44 entries, 42 CLEARED, one per spike or capability.
  Section numbers unchanged, so `§11.x` still resolves. This is the
  append target when a roadmap item lands.

- [`2026-04-30_netrender_design_plan.md`](2026-04-30_netrender_design_plan.md)
  — **axioms and crate rationale only.** Partly superseded: §3 axioms
  and §4 crate structure still hold and are cited by number everywhere,
  but §5's phase plan is replaced by the vello plan's §12, §7's open
  questions are all closed or moot (one of them, the "no `hit_test()`
  entry point" rule, was reversed in the code and is now documented as
  such), and §9's time estimate is dead. Do not read it for direction.

- [`2026-08-04_rasterizer_backend_seam.md`](2026-08-04_rasterizer_backend_seam.md)
  — research record, no implementation proposed. Revisits the vello
  plan's §10 and its 2026-05-01 `vello_hybrid` rejection now that
  hybrid has published 0.1.0 on `wgpu ^29.0.3`. Capability probe
  passes every op netrender lowers, under WebGL2-class limits, on
  wgpu's GL backend. Blocker is that hybrid has no `Scene::append`,
  so Phase 7''s Masonry tile cache has no equivalent and a seam would
  have to tolerate two *retention models*. Also inventories the
  present `vello::Scene` coupling in shared retained state. Posture:
  credible future second backend, not a pending migration.
  Re-verified against hybrid 0.2.0 on 2026-08-10; blocker holds. The ask
  is drafted, unsent, at
  [`2026-08-10_vello_hybrid_upstream_ask.md`](2026-08-10_vello_hybrid_upstream_ask.md).

  **Experiment activated 2026-09-03.** Netrender now names all three Vello
  realizations (`Classic`, `Hybrid`, `Cpu`) while retaining one authoritative
  `Scene`. The opt-in `vello-all` proof pins `mark-ik/all-vellos` at
  `ca3f40ea182216883cd543c7b9deae991268917c`; CPU and Hybrid share one
  geometry/gradient/layer lowerer and return typed admission errors for
  images, patterns, text, filters, and registered fragments that are not yet
  wired. CPU pixel output and Hybrid retained append are covered by focused
  tests. Hybrid also renders through Netrender's wgpu-30 device and passes a
  GPU texture readback. Classic remains the shipping renderer. Commit
  `b06a21407` adds the rasterizer-independent corpus: two direct semantic scenes
  render through all three backends, typed sparse refusals cover unwired
  resource/filter classes, and capability declarations are checked against the
  result. Images, text, filters, and registered fragments remain future sparse
  adapter work rather than silent fallbacks. Commit `cfa0261c2` adds the first
  physical execution-graph participation receipt. Classic declares an opaque
  submit, Hybrid shares the graph encoder, and CPU enters through a named ready
  upload/import. All three pass one filter-free producer fixture through the
  same downstream blur/readback topology, while a separate combined-filter
  scene proves causal Classic pixels and exact Hybrid/CPU refusals.

- **Tenancy boot seam (landed 2026-08-10).** `netrender_device` now owns
  booting one device for netrender *and a tenant renderer* drawing into
  the same frame: `TenantNeeds` (required vs optional features, limits,
  label) plus `boot_shared` / `boot_on` and their async forms. The
  tenancy contract is documented on `TenantNeeds`: one device and queue,
  the tenant owns its target texture, composition is explicit at a
  stated scene-op boundary, receipts name the tenant. Arrived from the
  games wing's R4 extraction review, which ruled the seam belongs *up*
  here rather than sideways in a wing crate, since the composition half
  (`ExternalTextureComposite`) was already netrender's. First consumer:
  Paredros's room probe, which previously ran the adapter dance itself
  and carried its own copy of netrender's inter-stage-variable minimum —
  now `REQUIRED_INTER_STAGE_VARIABLES`, stated once. Receipts in
  `netrender_device/tests/tenancy.rs`, including the trap the work
  surfaced: wgpu 29 advertises experimental features on the adapter but
  refuses the device unless they were asked for deliberately, so
  opportunistic grants mask them out.

A running list of post-pivot findings used to sit in this section. It
duplicated the verification record entry by entry and went stale between
updates, so it is gone. The record is the list.

**Workspace state, 2026-08-10:** see the corpus note below for the
current count. `cargo fmt --all --check` clean.

## Paint-list corpus

[`paint_list_render/tests/corpus/`](../paint_list_render/tests/corpus/README.md)
replays consumer `PaintEnvelope` captures through `translate_paint_list`
and asserts the resulting `Scene` op stream against a recorded golden,
plus postcard wire stability and per-fixture provenance. CPU-only, no
GPU needed.

It exists because every other test in this workspace is a scene
netrender wrote for itself, and eight repos depend on this vocabulary.

Five fixtures. Two are real captures off genet's Livery pipeline
(roadmap **A5**), reaching `DrawText` and `DrawBorder`, which no
hand-written test did. Three are hand-built and labelled as such in their
`.provenance`; `nested_stacks` should stay that way, since real scenes
nest too shallowly to reach the case where stack-handling bugs live.
Captures elide font payloads, which is why they are 2 KB rather than
2 MB — see the corpus README for why that is safe and what it costs.

## Host verification

- **2026-07-20 macOS / Metal:** the full workspace suite passes with
  `WGPU_BACKEND=metal`, including Apple system fonts, Apple Color Emoji,
  backdrop filters, registered textures, and compositor plumbing. The pass
  added stable Metal goldens for the two rotated Phase 3 scenes, bounded the
  small cross-submission AA variance in `compose_into`, and fixed offscreen
  filter renders incorrectly sharing retained frame tile-cache state. The
  `demo_card_grid` example also renders successfully through Metal.

## Active follow-up plans (small scope)

- [`2026-05-04_feature_roadmap.md`](2026-05-04_feature_roadmap.md)
  — Phase R (open refinements / wart fixes — was §11.99 of the
  rasterizer plan) + Phases A–G (new capability: diagnostics
  first, then consumer-pull-imminent, then SceneOp expansions,
  then architecturally-significant, then companion lanes).
- [`2026-05-05_compositor_handoff_path_b_prime.md`](2026-05-05_compositor_handoff_path_b_prime.md)
  — axiom-14 native-compositor handoff via path (b′). Sub-phases
  5.1–5.4 shipped on netrender side (commit `9447a852b`); 5.5
  genet adapter pending in separate workspace. Roadmap entry:
  D3.
- [`2026-05-06_webgl_over_wgpu_plan.md`](2026-05-06_webgl_over_wgpu_plan.md)
  — WebGL-over-wgpu companion lane. G0–G6 sequence, gated on
  Genet/Pelt consumer pull. Roadmap entry: G.
- [`2026-09-04_wgpu_execution_graph_plan.md`](2026-09-04_wgpu_execution_graph_plan.md)
  — **RG0 through RG3 and render-executor extraction delivered; RG2c passed
  the graph-promotion gate; Paredros and Mesocosm consumer receipts, headed
  presentation, and Paredros rebuild-all are complete; RG4 is untriggered,
  RG5 deferred, and broader GPU execution remains gated.** Evolves the
  delivered Phase 6 filter DAG into a validated, inspectable execution plan
  over the existing shared `WgpuHandles`. `vk-graph` informs compiled GPU work;
  AnyRender informs the semantic adapter seam above it. Both are prior art,
  not dependencies. RG0 landed in `fa5526051`: deterministic push-order
  scheduling plus typed refusals for duplicate tasks, external/task ID
  collisions, missing inputs, and cycles. The pre-RG1 probe found only unary
  raw graph consumers; the first real logical fork/join is the direct-`Scene`
  combined backdrop-plus-element-filter path. RG2b supplies its required Vello
  boundaries, while RG2c still owns the compiled multi-input execution proof.
  Commit `975d9df4f` delivers the image-only compiler:
  typed logical nodes, compile/execute separation, deterministic culling and
  plan dump, logical lifetime/allocation evidence, physical import admission,
  and the first migrated blur path. Commit `69b9179c0` moves color matrix and
  box-shadow/clip onto the plan too; a new Scene-level `Invert(1)` receipt and
  the existing blur/shadow receipts are green. The scoped encoder facade was
  rejected because it did not strengthen the crate-private pass boundary.
  Commit `0af85a62f` re-homes the eight useful legacy GPU assertions behind
  the planned path and removes the public raw task/callback API. This is an
  unreleased 0.2.0 boundary. Commit `35cb54ea5` measures 16 completed warmups
  and 64 completed release samples of both 256 x 256 and 1024 x 1024 production
  box-shadow plans. Allocation p95 is 0.111 ms and 0.102 ms on the measured
  RTX 4060/Vulkan row, below the predeclared absolute and relative thresholds,
  so RG5 remains deferred despite 33/35 logical creations. Commit `b06a21407`
  delivers RG2a's independent scene corpus and typed three-backend semantic
  preflight. It also fixes sparse transformed primitive clips discovered by
  the cross-backend anchors. Commit `cfa0261c2` delivers RG2b's explicit
  Classic/Hybrid/CPU producer boundaries, caller-owned graph encoding,
  encoder-participating external composition, physical shared-device
  readback, and the corrected filter-evidence split. Its shared graph remains
  unary; RG2c is the real effect-fork/join promotion gate. RG3 now owns the
  first closed tenant frame plus error attribution and presentation policy and
  may proceed independently. Commits `42ef0420a` and `d08f713d8` add the one
  legal load-preserving imported color-output shape and a public opaque-tenant
  envelope over a real private graph composite. Its sRGB-to-unorm physical
  receipt byte-matches the existing filter-free boundary-zero legacy path and
  reports the logical opaque boundary, caller-reported physical count, and
  graph-only submission separately. Paredros commits `1491c2b`, `81a2f08`, and
  `0bfd2f7` adopt that envelope for the normal Renderling room, byte-match the legacy
  final master with 466 distinct colors, install the host-owned validation and
  device-fault gate, keep optimistic scope resolution nonblocking, model
  awaited and optimistic suppression in pure reducer tests, capture a real
  scoped wgpu validation failure, and submit valid work afterward. A headed
  failure through the real headed room surface and the shared-device rebuild
  lifecycle are delivered in `3ddf1d3` and `bc452ca`. Renderling commit
  `3683dd6` adds complete-stage caller-owned encoding for direct draws, and
  Paredros commit `9a647c9` adopts it. The strengthened RG3 receipt measures
  the 20-pass Renderling tenant as one caller submission with zero internal
  submissions, followed by one separate Netrender graph submission, while
  retaining the 466-colour byte-match. Mesocosm commits
  `bccdbac` and `6d598e3` supply the second consumer: its production Section is
  one closed opaque tenant, the receipt reports one caller submission plus one
  graph batch and submission, and graph-backed replay preserves the recorded
  world hash. Its
  first headed run found a shared composite-uniform overwrite that made the
  surface black; the fix gives each encoded draw immutable rectangle data, and
  the user confirmed the corrected build was visible. Chrome now joins the
  master before one host presentation blit, with a further 1,800-frame headed
  run and graph-backed capture. The two renderer consumers justified
  extraction, and commit `93b221a5e` moves the typed image graph,
  deterministic compiler/lifetime plan, bound executor, and reports into
  `netrender_device`. The new task seam prepares resources from declared
  inputs and returns commands for one executor-owned render pass, so producers
  never receive the raw command encoder or choose an undeclared target.
  Scene/Vello/filter builders and the public tenant envelope remain in
  `netrender`. Workspace, device/tenancy, RG1/RG2b/RG2c/RG3 tests and physical
  backend receipts remain green. Paredros repeats its room suite and 466-colour
  physical RG3 composition through the sibling path; Mesocosm commit `f16e802`
  pins `93b221a5e` and repeats its adapter, raster, render, and physical RG3
  byte-match receipts;
  a future resident-buffer path must carry producer-owned revision/epoch
  metadata, and RG4 must split reusable plan structure from bound callbacks.
- [`wasm-portability-checklist.md`](wasm-portability-checklist.md)
  — note: this is for the WebRender wgpu-backend work (separate project,
  now the `archive/*` tags), retained for reference. A netrender-specific
  portability list will be authored when F2 (wasm) triggers.

## Archived branches

All non-`main` branches were retired to `archive/*` tags on 2026-08-10.
Index and restore instructions:
[`archive/2026-08-10_branch_archive.md`](archive/2026-08-10_branch_archive.md).

## Historical / superseded — archived

The plans below predate the vello pivot or have collapsed into other
docs. They describe approaches that are no longer the path forward
or work that has been completed and rolled into the canonical plans.
All have been moved under [`archive/`](archive/) — kept for
historical context, not for guidance.

**Activated and folded:**

- [`archive/2026-05-05_deferred_phases.md`](archive/2026-05-05_deferred_phases.md)
  — was the holding pen for three architecturally-significant
  deferrals (12c' backdrop filter, 13' compositor handoff,
  linear-light blending). All three activated 2026-05-05; canonical
  entries now live on the roadmap as D1, D3, R9. Doc retained as
  the activation history record.

**Pre-vello-pivot (the WebRender wgpu-backend lane):**

- `archive/2026-04-28_idiomatic_wgsl_pipeline_plan.md` — the
  idiomatic-wgpu-pipeline branch's approach (authored WGSL only, no GL,
  no SPIR-V intermediate). Was the active plan before the vello pivot;
  the code is preserved at the `archive/idiomatic-wgpu-pipeline` tag.
- `archive/2026-04-28_renderer_body_wgpu_adapter_plan.md` — `WgpuDevice`
  adapter early-stage planning. Subsumed by netrender_device's
  current shape.
- `archive/2026-04-29_pipeline_first_migration_plan.md` — typed-pipeline
  migration, batch-builder discussion. Pre-cleanup.
- `archive/2026-04-30_phase_d_rollback_to_skeleton.md` — record of the
  rollback that preceded the netrender split.
- `archive/2026-04-30_servo_wgpu_integration_assessment.md` — pre-fork
  servo-integration assessment.
- `archive/2026-04-08_live_full_reftest_confirmation.md` — last
  GL/wrench reftest confirmation before the fork (412/412 passing).
  Now load-bearing prior art for the WebGL-over-wgpu plan §3.1.
- `archive/2026-04-18_spirv_shader_pipeline_plan.md` — dead direction.
- `archive/2026-04-18_upstream_cherry_pick_plan.md` — superseded by the
  fork.
- `archive/2026-04-21_spirv_pipeline_reset_execution.md` — superseded.
- `archive/2026-04-22_upstream_cherry_pick_reevaluation.md` —
  superseded.
- `archive/2026-04-24_tile_with_spacing_validation_error.md` —
  historical bug diagnostic.
- `archive/2026-04-26_track3_legacy_assembly_isolation_lane.md` —
  superseded.
- `archive/2026-04-27_dual_servo_parity_plan.md` — superseded by the
  fork.
- `archive/2026-04-28_session_brief.md` — historical session note.
- `archive/2026-03-01_webrender_wgpu_renderer_implementation_plan.md` —
  the original convergence history, no longer canonical.

**Pre-vello-pivot small-scope plans (WebRender wgpu-backend lane,
code no longer in this repo):**

- `archive/draw_context_plan.md` — `WgpuDrawContext` + encoder
  batching plan against `webrender/src/device/wgpu_device.rs` and
  `webrender/src/renderer/mod.rs`. WebRender lane.
- `archive/typed_pipeline_metadata_plan.md` — flat
  `WgpuShaderVariant` enum to replace `(name, config)` string-tuple
  pipeline keys. WebRender lane.
- `archive/texture_cache_cleanup_plan.md` — WebRender wgpu-backend
  texture subsystem cleanup (`WgpuFrameDataTextures`, dither,
  unsafe-byte-slice utility, mali workaround). WebRender lane.
- `archive/servo_wgpu_integration.md` — Servo + WebRender
  wgpu-backend integration guide (DPR=1/2 confirmed 2026-04-02).
  Pre-vello-pivot. The active netrender-side Servo integration
  story now lives in the path (b′) plan and the WebGL-over-wgpu
  plan; this guide documents the historical lane.

## Local-only

- `archive/` — dated progress snapshots and older branch-shape notes,
  kept for historical traceability.
- `logs/` — local-only, gitignored except for its `.gitignore`. Only
  retain artifacts supporting an active note or unresolved diagnostic.
