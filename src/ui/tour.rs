//! The guided tour: one stop per region of the window, each lit up over a
//! dimmed backdrop with a callout explaining it. The regions record where
//! they drew each frame through `anchor`, so the same tour works over the
//! home page's placeholder layout and over a real folder's Grid.

use super::*;

/// The tour's stops, one per region, in the order the tour visits them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TourStep {
    Folders,
    Filters,
    Content,
    Actions,
    /// The rail's Adjustments, Info and Export icons, one stop each.
    RailAdjust,
    RailInfo,
    RailExport,
    /// The whole rail, for the icons only the Loupe has.
    RailLoupe,
    Header,
}

impl TourStep {
    pub(crate) const ALL: [TourStep; 9] = [
        TourStep::Folders,
        TourStep::Filters,
        TourStep::Content,
        TourStep::Actions,
        TourStep::RailAdjust,
        TourStep::RailInfo,
        TourStep::RailExport,
        TourStep::RailLoupe,
        TourStep::Header,
    ];

    fn id(self) -> egui::Id {
        egui::Id::new(("tour_anchor", self))
    }
}

/// Record where `step`'s region drew this frame.
pub(super) fn anchor(ctx: &egui::Context, step: TourStep, rect: egui::Rect) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(step.id(), (pass, rect)));
}

/// Where `step`'s region drew this frame, if it drew at all.
pub(super) fn anchored(ctx: &egui::Context, step: TourStep) -> Option<egui::Rect> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<(u64, egui::Rect)>(step.id()))
        .filter(|(p, _)| *p == pass)
        .map(|(_, r)| r)
}

/// How far the dim backdrop darkens what the step doesn't point at.
const DIM: u8 = 140;
const CALLOUT_WIDTH: f32 = 320.0;
const GAP: f32 = 12.0;

/// Draw the shown stop, if the tour is running. Drawn last, so every region
/// has recorded its anchor this frame.
pub(super) fn draw(ui: &egui::Ui, app: &App, out: &mut FrameOutput) {
    let Some(index) = app.tour_step() else {
        return;
    };
    let ctx = ui.ctx();
    let step = TourStep::ALL[index];
    let screen = ctx.content_rect();
    // A region this screen doesn't have, such as the Loupe's missing
    // toolbar, gets the whole window rather than a stray highlight.
    let target = anchored(ctx, step).unwrap_or(screen).intersect(screen);

    egui::Area::new(egui::Id::new("tour_backdrop"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .fade_in(false)
        .show(ctx, |ui| {
            // Swallows clicks so nothing behind the tour reacts to them.
            ui.allocate_rect(screen, egui::Sense::click());
            let dim = egui::Color32::from_black_alpha(DIM);
            let painter = ui.painter();
            for rect in [
                egui::Rect::from_min_max(screen.min, egui::pos2(screen.max.x, target.min.y)),
                egui::Rect::from_min_max(egui::pos2(screen.min.x, target.max.y), screen.max),
                egui::Rect::from_min_max(
                    egui::pos2(screen.min.x, target.min.y),
                    egui::pos2(target.min.x, target.max.y),
                ),
                egui::Rect::from_min_max(
                    egui::pos2(target.max.x, target.min.y),
                    egui::pos2(screen.max.x, target.max.y),
                ),
            ] {
                painter.rect_filled(rect, 0.0, dim);
            }
            painter.rect_stroke(
                target.shrink(1.0),
                4.0,
                egui::Stroke::new(2.0_f32, theme::BRAND_BLUE),
                egui::StrokeKind::Inside,
            );
        });

    let width = font_size::px(ui.style(), CALLOUT_WIDTH).min(screen.width() - 2.0 * GAP);
    let gap = font_size::px(ui.style(), GAP);
    let (pos, pivot) = if step == TourStep::Header {
        // Its buttons sit at the header's right end, so the callout does too.
        (
            egui::pos2(target.max.x - gap, target.max.y + gap),
            egui::Align2::RIGHT_TOP,
        )
    } else {
        callout_place(screen, target, width, gap)
    };
    egui::Area::new(egui::Id::new("tour_callout"))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos)
        .pivot(pivot)
        .constrain_to(screen)
        .fade_in(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(width);
                    callout(ui, index, out);
                });
        });
}

/// The step's title, body, position and buttons.
fn callout(ui: &mut egui::Ui, index: usize, out: &mut FrameOutput) {
    let t = t();
    let (title, body) = t.tour_steps[index];
    let last = index + 1 == TourStep::ALL.len();
    ui.label(
        egui::RichText::new(title)
            .size(font_size::px(ui.style(), 17.0))
            .strong(),
    );
    ui.add_space(font_size::px(ui.style(), 6.0));
    ui.label(body);
    if TourStep::ALL[index] == TourStep::RailLoupe {
        loupe_icons(ui);
    }
    ui.add_space(font_size::px(ui.style(), 6.0));
    ui.weak((t.tour_step_of)(index + 1, TourStep::ALL.len()));
    let next = if last { t.tour_done } else { t.tour_next };
    let clicked = form::footer(
        ui,
        &[
            form::Button {
                enabled: index > 0,
                ..form::Button::new(t.tour_back, form::Role::Cancel)
            },
            form::Button::new(next, form::Role::Primary),
        ],
    );
    match clicked {
        Some(form::Role::Primary) => out.actions.push(UiAction::TourNext),
        Some(form::Role::Cancel) => out.actions.push(UiAction::TourBack),
        _ => {}
    }
    // The footer lays its buttons against the right edge. Skip sits alone at
    // the left, level with them.
    if !last {
        let row = ui.min_rect();
        let h = ui.spacing().interact_size.y;
        let left = egui::Rect::from_min_max(
            egui::pos2(row.min.x, row.max.y - h),
            egui::pos2(row.center().x, row.max.y),
        );
        let skip = ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(left)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
            |ui| ui.link(t.skip_tour),
        );
        if skip.inner.clicked() {
            out.actions.push(UiAction::EndTour);
        }
    }
}

/// The rail icons only the Loupe has, each drawn as the rail draws it,
/// beside its name and what it does.
fn loupe_icons(ui: &mut egui::Ui) {
    use crate::app::{DevelopTab, RailItem};
    let t = t();
    let side = font_size::px(ui.style(), ICON);
    let color = ui.visuals().text_color();
    for (item, name, what) in [
        (
            RailItem::Develop(DevelopTab::Crop),
            t.tab_crop,
            t.tour_loupe_icons[0],
        ),
        (
            RailItem::Develop(DevelopTab::Masks),
            t.tab_masks,
            t.tour_loupe_icons[1],
        ),
        (
            RailItem::GroupCompare,
            t.view_compare,
            t.tour_loupe_icons[2],
        ),
    ] {
        ui.add_space(font_size::px(ui.style(), 8.0));
        ui.horizontal_top(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
            super::develop_panel::paint_rail_icon(
                ui.painter(),
                rect.center(),
                font_size::px(ui.style(), 1.0),
                item,
                color,
            );
            ui.vertical(|ui| {
                ui.strong(name);
                ui.label(what);
            });
        });
    }
}

/// The side of a rail icon drawn in the callout, as on the rail.
const ICON: f32 = 36.0;

/// Where the callout goes: beside the target on whichever side has room for
/// it, or centered inside a target that fills the window.
fn callout_place(
    screen: egui::Rect,
    target: egui::Rect,
    width: f32,
    gap: f32,
) -> (egui::Pos2, egui::Align2) {
    let room_right = screen.max.x - target.max.x;
    let room_left = target.min.x - screen.min.x;
    let room_below = screen.max.y - target.max.y;
    let room_above = target.min.y - screen.min.y;
    let wide = width + 2.0 * gap;
    if room_right >= wide {
        (
            egui::pos2(target.max.x + gap, target.min.y + gap),
            egui::Align2::LEFT_TOP,
        )
    } else if room_left >= wide {
        (
            egui::pos2(target.min.x - gap, target.min.y + gap),
            egui::Align2::RIGHT_TOP,
        )
    } else if room_below >= room_above && room_below > 160.0 {
        (
            egui::pos2(target.center().x, target.max.y + gap),
            egui::Align2::CENTER_TOP,
        )
    } else if room_above > 160.0 {
        (
            egui::pos2(target.center().x, target.min.y - gap),
            egui::Align2::CENTER_BOTTOM,
        )
    } else {
        (target.center(), egui::Align2::CENTER_CENTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    }

    #[test]
    fn the_callout_sits_beside_a_narrow_panel_and_under_a_toolbar() {
        let screen = rect(0.0, 0.0, 1200.0, 800.0);
        let (pos, pivot) = callout_place(screen, rect(0.0, 40.0, 220.0, 800.0), 320.0, 12.0);
        assert_eq!(pivot, egui::Align2::LEFT_TOP);
        assert!(pos.x > 220.0);

        let (_, pivot) = callout_place(screen, rect(1150.0, 40.0, 1200.0, 800.0), 320.0, 12.0);
        assert_eq!(
            pivot,
            egui::Align2::RIGHT_TOP,
            "the rail's callout goes left"
        );

        let (pos, pivot) = callout_place(screen, rect(0.0, 40.0, 1200.0, 80.0), 320.0, 12.0);
        assert_eq!(pivot, egui::Align2::CENTER_TOP);
        assert!(pos.y > 80.0);

        let (_, pivot) = callout_place(screen, rect(0.0, 760.0, 1200.0, 800.0), 320.0, 12.0);
        assert_eq!(
            pivot,
            egui::Align2::CENTER_BOTTOM,
            "the actions bar's goes above"
        );

        let (_, pivot) = callout_place(screen, screen, 320.0, 12.0);
        assert_eq!(pivot, egui::Align2::CENTER_CENTER);
    }
}
