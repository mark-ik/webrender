// Copyright 2026 Mark Alan Boykin
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// SPDX-License-Identifier: MPL-2.0

//! Translator unit tests.

use crate::composite::merge_layers;

use paint_list_api::{
    ColorF, CommonPlacement, DeviceIntSize, EngineId, LayoutPoint, LayoutRect, PaintCmd, PaintList,
    PrimitiveFlags, RectItem,
};
use serde::{Deserialize, Serialize};

use super::*;

/// Minimal `PaintList` impl for driving `translate_paint_list`
/// from tests without pulling in a producer crate.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StubPaintList {
    viewport: DeviceIntSize,
    commands: Vec<PaintCmd>,
}

impl PaintList for StubPaintList {
    fn engine_id(&self) -> EngineId {
        EngineId::GENET
    }
    fn viewport(&self) -> DeviceIntSize {
        self.viewport
    }
    fn generation_id(&self) -> u64 {
        0
    }
    fn commands(&self) -> &[PaintCmd] {
        &self.commands
    }
}

fn box2d(x: f32, y: f32, w: f32, h: f32) -> LayoutRect {
    LayoutRect::new(LayoutPoint::new(x, y), LayoutPoint::new(x + w, y + h))
}

fn placement_at(bounds: LayoutRect) -> CommonPlacement {
    CommonPlacement {
        bounds,
        flags: PrimitiveFlags::empty(),
    }
}

fn list_with(viewport: DeviceIntSize, cmds: Vec<PaintCmd>) -> StubPaintList {
    StubPaintList {
        viewport,
        commands: cmds,
    }
}

#[test]
fn translucent_colors_premultiply_at_lowering() {
    // ColorF is unpremultiplied; scene colors are premultiplied and every
    // consumer divides by alpha on the way back out. An unpremultiplied
    // copy therefore over-brightened every translucent fill toward white:
    // half-alpha pure red became (1/0.5 -> clamped) full white at half
    // cover. Found by mesocosm's minimap, whose 35%-alpha cells rendered
    // as a white sheet.
    let list = list_with(
        DeviceIntSize::new(64, 64),
        vec![PaintCmd::DrawRect(RectItem {
            placement: placement_at(box2d(0.0, 0.0, 32.0, 32.0)),
            color: ColorF::new(1.0, 0.0, 0.0, 0.5),
        })],
    );
    let scene = translate_paint_list(&list);
    let rect_color = scene
        .ops
        .iter()
        .find_map(|op| match op {
            netrender::SceneOp::Rect(r) => Some(r.color),
            _ => None,
        })
        .expect("one rect");

    assert_eq!(
        rect_color,
        [0.5, 0.0, 0.0, 0.5],
        "premultiplied, alpha intact"
    );
}

#[test]
fn empty_list_translates_to_empty_scene() {
    let list = list_with(DeviceIntSize::new(800, 600), Vec::new());
    let scene = translate_paint_list(&list);
    assert_eq!(scene.viewport_width, 800);
    assert_eq!(scene.viewport_height, 600);
    assert_eq!(scene.ops.len(), 0);
}

#[test]
fn draw_rect_emits_scene_rect() {
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![PaintCmd::DrawRect(RectItem {
            placement: placement_at(box2d(10.0, 20.0, 100.0, 50.0)),
            color: ColorF {
                r: 1.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
        })],
    );
    let scene = translate_paint_list(&list);
    assert_eq!(scene.ops.len(), 1);
    assert!(matches!(scene.ops[0], netrender::SceneOp::Rect(_)));
}

/// Composing layers with no side-tables (the orrery underlay + a doc, today)
/// concatenates command streams back-to-front into one scene.
#[test]
fn commands_only_layers_concatenate_into_one_scene() {
    let underlay = vec![PaintCmd::DrawRect(RectItem {
        placement: placement_at(box2d(0.0, 0.0, 10.0, 10.0)),
        color: ColorF {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        },
    })];
    let doc = vec![PaintCmd::DrawRect(RectItem {
        placement: placement_at(box2d(5.0, 5.0, 10.0, 10.0)),
        color: ColorF {
            r: 0.0,
            g: 0.0,
            b: 1.0,
            a: 1.0,
        },
    })];
    let layers = [
        CompositeLayer::commands_only(&underlay),
        CompositeLayer::commands_only(&doc),
    ];
    let out = composite_paint_layers(DeviceIntSize::new(800, 600), &layers);
    let rects = out
        .scene
        .ops
        .iter()
        .filter(|o| matches!(o, netrender::SceneOp::Rect(_)))
        .count();
    assert_eq!(rects, 2, "both layers' rects composite into one scene");
}

/// Two layers that BOTH mint a font under the default namespace+index (the
/// collision producers fall into) must come out with distinct keys, and each
/// layer's `DrawText` rewritten to its own remapped key — otherwise the
/// translator's `FontInstanceKey -> FontId` map clobbers one font with the
/// other. (Pins the font/image key-namespacing the seam exists for.)
#[test]
fn merge_namespaces_colliding_font_keys() {
    use paint_list_api::{FontInstanceKey, FontResource, IdNamespace, TextOptions, TextRunItem};

    let collide = FontInstanceKey::new(IdNamespace(0), 0);
    let mk = |face: u8| {
        let fonts = vec![FontResource {
            key: collide,
            data: std::sync::Arc::new(vec![face]),
            index: 0,
        }];
        let cmds = vec![PaintCmd::DrawText(TextRunItem {
            placement: placement_at(box2d(0.0, 0.0, 10.0, 10.0)),
            font_instance: collide,
            font_size: 16.0,
            color: ColorF::BLACK,
            glyphs: vec![],
            options: TextOptions::default(),
        })];
        (fonts, cmds)
    };
    let (f0, c0) = mk(0xAA);
    let (f1, c1) = mk(0xBB);
    let layers = [
        CompositeLayer {
            commands: &c0,
            fonts: &f0,
            images: &[],
        },
        CompositeLayer {
            commands: &c1,
            fonts: &f1,
            images: &[],
        },
    ];
    let (commands, fonts, images) = merge_layers(&layers);

    assert!(images.is_empty());
    assert_eq!(fonts.len(), 2, "both fonts survive the merge");
    assert_ne!(fonts[0].key, fonts[1].key, "colliding keys made distinct");
    assert_eq!(
        *fonts[0].data,
        vec![0xAA],
        "font bytes paired to the right new key"
    );
    assert_eq!(*fonts[1].data, vec![0xBB]);
    let text_keys: Vec<_> = commands
        .iter()
        .filter_map(|c| match c {
            PaintCmd::DrawText(t) => Some(t.font_instance),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_keys,
        vec![fonts[0].key, fonts[1].key],
        "each layer's DrawText references its own remapped font",
    );
}

/// `DrawStroke` (the orrery's edge primitive — a straight or routed polyline)
/// lowers to one stroked `SceneShape`, so edges render. (Was warn-skipped.)
#[test]
fn draw_stroke_emits_stroked_scene_shape() {
    use paint_list_api::{LayoutPoint, PathCommand, PathData, StrokeCap, StrokeItem, StrokeJoin};

    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![PaintCmd::DrawStroke(StrokeItem {
            placement: placement_at(box2d(0.0, 0.0, 100.0, 100.0)),
            path: PathData {
                commands: vec![
                    PathCommand::MoveTo(LayoutPoint::new(0.0, 0.0)),
                    PathCommand::LineTo(LayoutPoint::new(100.0, 80.0)),
                ],
            },
            color: ColorF {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            width: 2.0,
            cap: StrokeCap::Butt,
            join: StrokeJoin::Miter,
            dash: None,
        })],
    );
    let scene = translate_paint_list(&list);
    let stroked = scene
        .ops
        .iter()
        .filter(|o| matches!(o, netrender::SceneOp::Shape(s) if s.stroke.is_some()))
        .count();
    assert_eq!(stroked, 1, "DrawStroke lowers to one stroked SceneShape");
}

/// A `DrawStroke` with round cap / join and a dash pattern carries those
/// decorations onto the lowered `ScenePathStroke` (not just color + width), so a
/// dashed, round-capped widget stroke renders instead of a solid butt one.
#[test]
fn draw_stroke_carries_cap_join_dash() {
    use paint_list_api::{
        DashPattern, LayoutPoint, PathCommand, PathData, StrokeCap, StrokeItem, StrokeJoin,
    };

    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![PaintCmd::DrawStroke(StrokeItem {
            placement: placement_at(box2d(0.0, 0.0, 100.0, 100.0)),
            path: PathData {
                commands: vec![
                    PathCommand::MoveTo(LayoutPoint::new(0.0, 0.0)),
                    PathCommand::LineTo(LayoutPoint::new(100.0, 80.0)),
                ],
            },
            color: ColorF {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            width: 2.0,
            cap: StrokeCap::Round,
            join: StrokeJoin::Round,
            dash: Some(DashPattern {
                intervals: vec![5.0, 3.0],
                offset: 1.0,
            }),
        })],
    );
    let scene = translate_paint_list(&list);
    let shape = scene
        .ops
        .iter()
        .find_map(|o| match o {
            netrender::SceneOp::Shape(s) if s.stroke.is_some() => Some(s),
            _ => None,
        })
        .expect("stroked shape");
    let stroke = shape.stroke.as_ref().unwrap();
    assert_eq!(stroke.cap, netrender::SceneStrokeCap::Round);
    assert_eq!(stroke.join, netrender::SceneStrokeJoin::Round);
    assert_eq!(stroke.dash_pattern, vec![5.0, 3.0]);
    assert_eq!(stroke.dash_offset, 1.0);
}

#[test]
fn push_pop_layer_emits_layer_pair() {
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![
            PaintCmd::PushLayer(ple::LayerSpec {
                opacity: 0.5,
                ..ple::LayerSpec::default()
            }),
            PaintCmd::PopLayer,
        ],
    );
    let scene = translate_paint_list(&list);
    assert_eq!(scene.ops.len(), 2);
    assert!(matches!(scene.ops[0], netrender::SceneOp::PushLayer(_)));
    assert!(matches!(scene.ops[1], netrender::SceneOp::PopLayer));
}

#[test]
fn push_layer_carries_filter_chain() {
    // CSS `filter` lands on `SceneLayer::filters` (the layer's own output
    // chain); `opacity()` in the chain folds into the layer alpha instead.
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![
            PaintCmd::PushLayer(ple::LayerSpec {
                filters: vec![
                    ple::FilterOp::Grayscale(1.0),
                    ple::FilterOp::Opacity(0.5),
                    ple::FilterOp::Blur(2.0),
                ],
                ..ple::LayerSpec::default()
            }),
            PaintCmd::PopLayer,
        ],
    );
    let scene = translate_paint_list(&list);
    let layer = match &scene.ops[0] {
        netrender::SceneOp::PushLayer(l) => l,
        other => panic!("expected PushLayer, got {other:?}"),
    };
    assert_eq!(
        layer.filters,
        vec![
            netrender::SceneFilter::Grayscale(1.0),
            netrender::SceneFilter::Blur(2.0)
        ],
        "color/blur ops map to SceneLayer.filters in order"
    );
    assert_eq!(layer.alpha, 0.5, "opacity() folds into the layer alpha");
}

#[test]
fn push_transform_adds_palette_entry_and_positions_child_ops() {
    // PushTransform is a coordinate-space change, NOT a compositing
    // layer: it adds a transform palette entry and threads the id
    // onto child ops, but emits no PushLayer/PopLayer.
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![
            PaintCmd::PushTransform(ple::TransformSpec {
                origin: LayoutPoint::new(10.0, 20.0),
                transform: paint_list_api::LayoutTransform::identity(),
                kind: ple::TransformKind::Standard,
            }),
            PaintCmd::DrawRect(RectItem {
                placement: placement_at(box2d(0.0, 0.0, 100.0, 50.0)),
                color: ColorF {
                    r: 1.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            }),
            PaintCmd::PopTransform,
        ],
    );
    let scene = translate_paint_list(&list);
    // One transform palette entry beyond identity (index 1).
    assert!(
        scene.transforms.len() >= 2,
        "transforms: {:?}",
        scene.transforms
    );
    // No layers — just the rect.
    assert_eq!(scene.ops.len(), 1);
    let rect = match &scene.ops[0] {
        netrender::SceneOp::Rect(r) => r,
        other => panic!("expected Rect, got {other:?}"),
    };
    // The rect picks up the pushed transform (non-zero id).
    assert!(
        rect.transform_id > 0,
        "rect should carry the pushed transform id"
    );
    // That transform translates by the origin (10, 20).
    let t = &scene.transforms[rect.transform_id as usize];
    assert_eq!(t.m[12], 10.0, "tx");
    assert_eq!(t.m[13], 20.0, "ty");
}

#[test]
fn nested_transforms_compose() {
    // Outer translate(10, 20), inner translate(5, 5) → a rect
    // inside both should resolve to translate(15, 25).
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![
            PaintCmd::PushTransform(ple::TransformSpec {
                origin: LayoutPoint::new(10.0, 20.0),
                transform: paint_list_api::LayoutTransform::identity(),
                kind: ple::TransformKind::Standard,
            }),
            PaintCmd::PushTransform(ple::TransformSpec {
                origin: LayoutPoint::new(5.0, 5.0),
                transform: paint_list_api::LayoutTransform::identity(),
                kind: ple::TransformKind::Standard,
            }),
            PaintCmd::DrawRect(RectItem {
                placement: placement_at(box2d(0.0, 0.0, 10.0, 10.0)),
                color: ColorF {
                    r: 0.0,
                    g: 1.0,
                    b: 0.0,
                    a: 1.0,
                },
            }),
            PaintCmd::PopTransform,
            PaintCmd::PopTransform,
        ],
    );
    let scene = translate_paint_list(&list);
    let rect = match &scene.ops[0] {
        netrender::SceneOp::Rect(r) => r,
        other => panic!("expected Rect, got {other:?}"),
    };
    let t = &scene.transforms[rect.transform_id as usize];
    assert_eq!(t.m[12], 15.0, "composed tx (10 + 5)");
    assert_eq!(t.m[13], 25.0, "composed ty (20 + 5)");
}

#[test]
fn push_clip_rect_emits_clipped_layer() {
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![
            PaintCmd::PushClip(ple::ClipSpec {
                kind: ple::ClipKind::Rect(box2d(0.0, 0.0, 100.0, 100.0)),
            }),
            PaintCmd::PopClip,
        ],
    );
    let scene = translate_paint_list(&list);
    assert_eq!(scene.ops.len(), 2);
    let layer = match &scene.ops[0] {
        netrender::SceneOp::PushLayer(l) => l,
        other => panic!("expected PushLayer, got {other:?}"),
    };
    assert!(matches!(layer.clip, netrender::SceneClip::Rect { .. }));
    assert!(matches!(scene.ops[1], netrender::SceneOp::PopLayer));
}

#[test]
fn external_texture_stages_an_in_order_scene_image() {
    use paint_list_api::ExternalTextureItem;
    let list = list_with(
        DeviceIntSize::new(800, 600),
        vec![PaintCmd::DrawExternalTexture(ExternalTextureItem {
            placement: placement_at(box2d(0.0, 0.0, 200.0, 200.0)),
            texture_key: 0xC0FFEE,
            opacity: 0.75,
            content_generation: None,
        })],
    );
    let out = translate_paint_cmd_stream(list.viewport, &list.commands, &[], &[]);
    let image = match &out.scene.ops[..] {
        [netrender::SceneOp::Image(image)] => image,
        other => panic!("expected one in-order external image, got {other:?}"),
    };
    assert_eq!(out.external_textures.len(), 1);
    assert_eq!(out.external_textures[0].texture_key, 0xC0FFEE);
    assert_eq!(out.external_textures[0].scene_op_boundary, 0);
    assert_eq!(image.key, out.external_textures[0].image_key);
    assert_eq!(image.color, [0.75; 4]);
}

#[test]
fn external_texture_keeps_the_active_transform_and_layer_scope() {
    use paint_list_api::{ExternalTextureItem, LayerSpec, TransformKind, TransformSpec};

    let list = list_with(
        DeviceIntSize::new(160, 120),
        vec![
            PaintCmd::PushTransform(TransformSpec {
                origin: LayoutPoint::new(0.0, 0.0),
                transform: paint_list_api::LayoutTransform::translation(12.0, 8.0, 0.0),
                kind: TransformKind::Standard,
            }),
            PaintCmd::PushClip(paint_list_api::ClipSpec {
                kind: paint_list_api::ClipKind::Rect(box2d(0.0, 0.0, 64.0, 48.0)),
            }),
            PaintCmd::PushLayer(LayerSpec {
                opacity: 0.5,
                ..LayerSpec::default()
            }),
            PaintCmd::DrawExternalTexture(ExternalTextureItem {
                placement: placement_at(box2d(4.0, 6.0, 20.0, 10.0)),
                texture_key: 77,
                opacity: 1.0,
                content_generation: None,
            }),
            PaintCmd::PopLayer,
            PaintCmd::PopClip,
            PaintCmd::PopTransform,
        ],
    );

    let out = translate_paint_cmd_stream(list.viewport, &list.commands, &[], &[]);
    let image = out
        .scene
        .ops
        .iter()
        .find_map(|op| match op {
            netrender::SceneOp::Image(image) => Some(image),
            _ => None,
        })
        .expect("external image");
    assert_ne!(image.transform_id, 0, "canvas inherits its CTM");
    assert_eq!(image.color, [1.0; 4], "layer owns element opacity");
    assert!(matches!(out.scene.ops[0], netrender::SceneOp::PushLayer(_)));
    assert!(matches!(out.scene.ops[1], netrender::SceneOp::PushLayer(_)));
    assert_eq!(out.external_textures[0].scene_op_boundary, 2);
}
