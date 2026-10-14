//! The Loupe's mask overlay: each mask's pin, the selected mask's lines or
//! ellipse with handles to drag, and the draw tool's drag-to-create. Each
//! handle is its own small area, so a press anywhere else still pans the
//! photo; only the armed draw tool covers the whole image.

use super::*;

use crate::app::MaskKind;
use crate::develop::{Mask, MaskShape};

/// Handle and pin radius, in points.
const HANDLE_PT: f32 = 6.0;
/// How far a drag must go before it draws a mask, in points, so a click
/// with the linear tool adds nothing.
const MIN_DRAW_PT: f32 = 4.0;
/// A radial mask a click without a drag adds, in shorter-side units.
const CLICK_RADIUS: f32 = 0.15;

/// What a drag in progress moves.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    Move,
    Start,
    End,
    /// Either end of the ellipse's own x axis, which sizes that radius.
    AxisX,
    AxisY,
    Rotate,
}

/// A drag in progress, kept in egui's memory between frames.
#[derive(Clone, Copy, Debug)]
enum Drag {
    Edit {
        index: usize,
        handle: Handle,
        from: Mask,
        origin: [f32; 2],
    },
    Draw {
        kind: MaskKind,
        origin: [f32; 2],
        /// The index the drawn mask took, once the drag went far enough.
        index: Option<usize>,
    },
}

/// Source UV and shorter-side units for the shown photo: lengths in a mask
/// are fractions of the shorter side, so a circle stays round.
struct Space<'a> {
    app: &'a App,
    central: egui::Rect,
    /// UV per shorter-side unit along u and v.
    per_unit: [f32; 2],
}

impl Space<'_> {
    fn screen(&self, uv: [f32; 2]) -> egui::Pos2 {
        self.app.loupe_source_to_screen(self.central, uv[0], uv[1])
    }

    fn uv(&self, p: egui::Pos2) -> [f32; 2] {
        let (u, v) = self.app.loupe_screen_to_source(self.central, p);
        [u, v]
    }

    /// `uv - center` in shorter-side units.
    fn units(&self, uv: [f32; 2], center: [f32; 2]) -> [f32; 2] {
        [
            (uv[0] - center[0]) / self.per_unit[0],
            (uv[1] - center[1]) / self.per_unit[1],
        ]
    }

    /// The UV of point `local` of an ellipse at `center` turned by `angle`.
    fn on_ellipse(&self, center: [f32; 2], angle: f32, local: [f32; 2]) -> [f32; 2] {
        let (s, c) = angle.sin_cos();
        let q = [c * local[0] - s * local[1], s * local[0] + c * local[1]];
        [
            center[0] + q[0] * self.per_unit[0],
            center[1] + q[1] * self.per_unit[1],
        ]
    }
}

/// Where a mask's pin sits: a radial's center, a linear's midpoint.
fn pin_of(m: &Mask) -> [f32; 2] {
    match m.shape {
        MaskShape::Linear { start, end } => [(start[0] + end[0]) / 2.0, (start[1] + end[1]) / 2.0],
        MaskShape::Radial { center, .. } => center,
    }
}

pub(super) fn loupe_mask_overlay(
    ui: &mut egui::Ui,
    app: &App,
    central: egui::Rect,
    out: &mut FrameOutput,
) {
    let (u1, v1) = app.touchup_uv_radii(1.0);
    if !(u1 > 0.0 && v1 > 0.0) {
        return;
    }
    let space = Space {
        app,
        central,
        per_unit: [u1, v1],
    };
    let ctx = ui.ctx().clone();
    let drag_id = egui::Id::new("mask_drag");
    let painter = ui.painter_at(central);
    let masks = app.current_masks();
    let selected = app.selected_mask();
    let accent = theme::colors(&ctx).cursor;

    for (i, m) in masks.iter().enumerate() {
        let at = space.screen(pin_of(m));
        if selected == Some(i) {
            draw_selected(&painter, &space, m, accent);
            continue;
        }
        pin(&painter, at, egui::Color32::WHITE);
        let resp = handle_area(&ctx, egui::Id::new(("mask_pin", i)), at, central);
        if resp.is_some_and(|r| r.clicked()) {
            out.actions.push(UiAction::SelectMask(Some(i)));
        }
    }

    if let Some((i, m)) = selected.and_then(|i| masks.get(i).map(|m| (i, *m))) {
        for (k, (handle, at)) in handles(&space, &m).into_iter().enumerate() {
            let id = egui::Id::new(("mask_handle", i, k));
            let Some(resp) = handle_area(&ctx, id, at, central) else {
                continue;
            };
            let cursor = match handle {
                Handle::Move => egui::CursorIcon::Move,
                Handle::Rotate => egui::CursorIcon::Alias,
                _ => egui::CursorIcon::Grab,
            };
            let resp = resp.on_hover_cursor(cursor);
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let drag = Drag::Edit {
                        index: i,
                        handle,
                        from: m,
                        origin: space.uv(p),
                    };
                    ctx.data_mut(|d| d.insert_temp(drag_id, drag));
                }
            }
            if resp.hovered() && handle == Handle::Move {
                if let MaskShape::Radial { feather, .. } = m.shape {
                    feather_wheel(ui, i, m, feather, out);
                }
            }
        }
        let drag = ctx.data(|d| d.get_temp::<Drag>(drag_id));
        if let Some(Drag::Edit {
            index,
            handle,
            from,
            origin,
        }) = drag
        {
            if let Some(p) = ctx.pointer_interact_pos().filter(|_| index == i) {
                if let Some(next) = dragged(&space, from, handle, origin, space.uv(p)) {
                    out.actions.push(UiAction::SetMask(i, next));
                }
            }
        }
    }

    if let Some(kind) = app.mask_tool() {
        draw_tool(&ctx, &space, kind, masks.len(), drag_id, out);
    }

    if !ctx.input(|i| i.pointer.any_down()) {
        ctx.data_mut(|d| d.remove::<Drag>(drag_id));
    }
}

/// The whole-image area that catches the draw tool's drag.
fn draw_tool(
    ctx: &egui::Context,
    space: &Space,
    kind: MaskKind,
    count: usize,
    drag_id: egui::Id,
    out: &mut FrameOutput,
) {
    egui::Area::new(egui::Id::new("loupe_mask_draw"))
        .order(egui::Order::Foreground)
        .fixed_pos(space.central.min)
        .show(ctx, |ui| {
            let (_id, resp) =
                ui.allocate_exact_size(space.central.size(), egui::Sense::click_and_drag());
            if resp.hovered() {
                ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let drag = Drag::Draw {
                        kind,
                        origin: space.uv(p),
                        index: None,
                    };
                    ctx.data_mut(|d| d.insert_temp(drag_id, drag));
                }
            }
            let drag = ctx.data(|d| d.get_temp::<Drag>(drag_id));
            if let (
                Some(Drag::Draw {
                    kind,
                    origin,
                    index,
                }),
                Some(p),
            ) = (drag, resp.interact_pointer_pos())
            {
                let far = (p - space.screen(origin)).length() >= MIN_DRAW_PT;
                if let Some(m) = drawn(space, kind, origin, space.uv(p)) {
                    match index {
                        Some(i) => out.actions.push(UiAction::SetMask(i, m)),
                        None if far => {
                            out.actions.push(UiAction::AddMask(m));
                            let drag = Drag::Draw {
                                kind,
                                origin,
                                index: Some(count),
                            };
                            ctx.data_mut(|d| d.insert_temp(drag_id, drag));
                        }
                        None => {}
                    }
                }
            }
            if resp.drag_stopped() {
                out.actions.push(UiAction::DisarmMaskTool);
            }
            if resp.clicked() && kind == MaskKind::Radial {
                if let Some(p) = resp.interact_pointer_pos() {
                    let m = Mask::radial(space.uv(p), [CLICK_RADIUS; 2]);
                    out.actions.push(UiAction::AddMask(m));
                    out.actions.push(UiAction::DisarmMaskTool);
                }
            }
        });
}

/// The mask a draw from `origin` to `to` makes: a linear one from where the
/// drag starts to where it ends, or a radial one centered on the start with
/// the pointer on its edge.
fn drawn(space: &Space, kind: MaskKind, origin: [f32; 2], to: [f32; 2]) -> Option<Mask> {
    let m = match kind {
        MaskKind::Linear => Mask::linear(origin, to),
        MaskKind::Radial => {
            let q = space.units(to, origin);
            // √2 puts the pointer on the ellipse whose axes it spans.
            let radii = q.map(|d| (d.abs() * std::f32::consts::SQRT_2).max(0.01));
            Mask::radial(origin, radii)
        }
    };
    m.sanitized()
}

/// `from` with `handle` dragged from `origin` to `to`, all in source UV.
fn dragged(
    space: &Space,
    from: Mask,
    handle: Handle,
    origin: [f32; 2],
    to: [f32; 2],
) -> Option<Mask> {
    let d = [to[0] - origin[0], to[1] - origin[1]];
    let shift = |p: [f32; 2]| [p[0] + d[0], p[1] + d[1]];
    let shape = match (from.shape, handle) {
        (MaskShape::Linear { start, end }, Handle::Move) => MaskShape::Linear {
            start: shift(start),
            end: shift(end),
        },
        (MaskShape::Linear { end, .. }, Handle::Start) => MaskShape::Linear { start: to, end },
        (MaskShape::Linear { start, .. }, Handle::End) => MaskShape::Linear { start, end: to },
        (
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            },
            handle,
        ) => {
            let q = space.units(to, center);
            let (s, c) = angle.sin_cos();
            let local = [c * q[0] + s * q[1], c * q[1] - s * q[0]];
            let (center, radii, angle) = match handle {
                Handle::Move => (shift(center), radii, angle),
                Handle::AxisX => (center, [local[0].abs(), radii[1]], angle),
                Handle::AxisY => (center, [radii[0], local[1].abs()], angle),
                Handle::Rotate => (center, radii, q[0].atan2(-q[1])),
                Handle::Start | Handle::End => return None,
            };
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            }
        }
        (MaskShape::Linear { .. }, _) => return None,
    };
    Mask { shape, ..from }.sanitized()
}

/// The selected mask's handles and where they sit on screen.
fn handles(space: &Space, m: &Mask) -> Vec<(Handle, egui::Pos2)> {
    match m.shape {
        MaskShape::Linear { start, end } => vec![
            (Handle::Start, space.screen(start)),
            (Handle::End, space.screen(end)),
            (Handle::Move, space.screen(pin_of(m))),
        ],
        MaskShape::Radial {
            center,
            radii,
            angle,
            ..
        } => {
            let at = |local| space.screen(space.on_ellipse(center, angle, local));
            let c = space.screen(center);
            let top = at([0.0, -radii[1]]);
            let out = (top - c).normalized() * 4.0 * HANDLE_PT;
            vec![
                (Handle::AxisX, at([radii[0], 0.0])),
                (Handle::AxisX, at([-radii[0], 0.0])),
                (Handle::AxisY, at([0.0, radii[1]])),
                (Handle::AxisY, top),
                (Handle::Rotate, top + out),
                (Handle::Move, c),
            ]
        }
    }
}

/// A handle's own area, or `None` when it sits off the image.
fn handle_area(
    ctx: &egui::Context,
    id: egui::Id,
    at: egui::Pos2,
    central: egui::Rect,
) -> Option<egui::Response> {
    if !central.contains(at) {
        return None;
    }
    let side = egui::Vec2::splat(2.0 * HANDLE_PT + 4.0);
    let resp = egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(at - side / 2.0)
        .show(ctx, |ui| {
            ui.allocate_exact_size(side, egui::Sense::click_and_drag())
                .1
        });
    Some(resp.inner)
}

/// Shift+wheel over a radial mask's center feathers it, as it does the
/// Touch Up brush.
fn feather_wheel(ui: &egui::Ui, i: usize, m: Mask, feather: f32, out: &mut FrameOutput) {
    let (delta, shift, alt) =
        ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift, i.modifiers.alt));
    if !shift || alt {
        return;
    }
    // macOS turns a Shift+wheel into horizontal scrolling.
    let d = if delta.y != 0.0 { delta.y } else { delta.x };
    if d == 0.0 {
        return;
    }
    let mut next = m;
    if let MaskShape::Radial { feather: f, .. } = &mut next.shape {
        *f = (feather + d * 0.0025).clamp(0.0, 1.0);
    }
    out.actions.push(UiAction::SetMask(i, next));
}

/// A filled pin with a dark rim, so it reads on bright and dark photos.
fn pin(painter: &egui::Painter, at: egui::Pos2, fill: egui::Color32) {
    painter.circle_filled(at, HANDLE_PT + 1.5, egui::Color32::from_black_alpha(160));
    painter.circle_filled(at, HANDLE_PT, fill);
}

/// A line drawn dark under light, so it reads on any photo.
fn line(painter: &egui::Painter, a: egui::Pos2, b: egui::Pos2, width: f32) {
    painter.line_segment(
        [a, b],
        egui::Stroke::new(width + 2.0, egui::Color32::from_black_alpha(140)),
    );
    painter.line_segment([a, b], egui::Stroke::new(width, egui::Color32::WHITE));
}

fn draw_selected(painter: &egui::Painter, space: &Space, m: &Mask, accent: egui::Color32) {
    match m.shape {
        MaskShape::Linear { start, end } => {
            let (a, b) = (space.screen(start), space.screen(end));
            let along = b - a;
            if along.length() >= 1.0 {
                // The three lines run across the gradient, past the image.
                let across =
                    egui::vec2(-along.y, along.x).normalized() * space.central.size().length();
                let mid = a + along / 2.0;
                line(painter, a - across, a + across, 1.0);
                line(painter, mid - across, mid + across, 1.5);
                line(painter, b - across, b + across, 1.0);
            }
            pin(painter, a, egui::Color32::WHITE);
            pin(painter, b, egui::Color32::WHITE);
            pin(painter, space.screen(pin_of(m)), accent);
        }
        MaskShape::Radial {
            center,
            radii,
            angle,
            feather,
        } => {
            let ring = |scale: f32| -> Vec<egui::Pos2> {
                (0..=64)
                    .map(|k| {
                        let t = k as f32 / 64.0 * std::f32::consts::TAU;
                        let local = [radii[0] * scale * t.cos(), radii[1] * scale * t.sin()];
                        space.screen(space.on_ellipse(center, angle, local))
                    })
                    .collect()
            };
            for w in ring(1.0).windows(2) {
                line(painter, w[0], w[1], 1.5);
            }
            // The inner ring is where the feather ends and the mask is full.
            if feather > 0.0 && feather < 1.0 {
                for w in ring(1.0 - feather).windows(2) {
                    painter.line_segment(
                        [w[0], w[1]],
                        egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(170)),
                    );
                }
            }
            for (handle, at) in handles(space, m) {
                let fill = if handle == Handle::Move {
                    accent
                } else {
                    egui::Color32::WHITE
                };
                if handle == Handle::Rotate {
                    let top = space.screen(space.on_ellipse(center, angle, [0.0, -radii[1]]));
                    line(painter, top, at, 1.0);
                }
                pin(painter, at, fill);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(app: &App) -> Space<'_> {
        Space {
            app,
            central: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0)),
            per_unit: [2.0 / 3.0, 1.0],
        }
    }

    fn radial_of(m: Mask) -> ([f32; 2], [f32; 2], f32) {
        let MaskShape::Radial {
            center,
            radii,
            angle,
            ..
        } = m.shape
        else {
            panic!("a radial mask: {m:?}");
        };
        (center, radii, angle)
    }

    #[test]
    fn a_radial_draw_puts_the_pointer_on_the_ellipse() {
        let app = App::new(None);
        let s = space(&app);
        let to = [0.7, 0.6];
        let (center, radii, _) = radial_of(drawn(&s, MaskKind::Radial, [0.5, 0.5], to).unwrap());
        let q = s.units(to, center);
        let d = (q[0] / radii[0]).hypot(q[1] / radii[1]);
        assert!((d - 1.0).abs() < 1e-5, "{d}");
    }

    #[test]
    fn the_rotate_handle_follows_the_pointer_around_the_center() {
        let app = App::new(None);
        let s = space(&app);
        let m = Mask::radial([0.5, 0.5], [0.2, 0.1]);
        let right_of_center = [0.5 + 0.3 * s.per_unit[0], 0.5];
        let (center, radii, angle) =
            radial_of(dragged(&s, m, Handle::Rotate, [0.5, 0.4], right_of_center).unwrap());
        let top = s.on_ellipse(center, angle, [0.0, -radii[1]]);
        assert!((top[1] - 0.5).abs() < 1e-5 && top[0] > 0.5, "{top:?}");
    }

    #[test]
    fn dragging_an_axis_handle_sizes_only_that_radius() {
        let app = App::new(None);
        let s = space(&app);
        let m = Mask::radial([0.5, 0.5], [0.2, 0.1]);
        let to = [0.5 + 0.35 * s.per_unit[0], 0.52];
        let (_, radii, _) = radial_of(dragged(&s, m, Handle::AxisX, [0.6, 0.5], to).unwrap());
        assert!(
            (radii[0] - 0.35).abs() < 1e-5 && radii[1] == 0.1,
            "{radii:?}"
        );
    }
}
