//! egui chrome: the thumbnail Grid, the Loupe filmstrip, the filter bar, and
//! rating overlays. The GPU renderer draws the loupe image itself; this module
//! draws everything around it and reports back the central image rect plus any
//! user actions (clicks, slider, filter changes) for `main.rs` to apply.
//!
//! Built against egui 0.34's `Panel`/`show_inside` API: `main.rs` runs us with
//! the root background `Ui` (from `Context::run_ui`), and we nest panels inside
//! it. The Loupe leaves its central region frameless/transparent so the wgpu
//! image shows through.

use crate::navigation::Cmp;
use crate::{App, ViewMode};

/// An action the UI wants `App` to perform after the frame is built. Positions
/// are indices into the *visible* list (same space as `App::sel`).
pub enum UiAction {
    /// Select the cell at this visible position.
    Select(usize),
    /// Open the loupe on this visible position.
    OpenLoupe(usize),
    /// Set the thumbnail size (longest-side px).
    SetThumbPx(u32),
    /// Set (or clear) the star filter.
    SetFilter(Option<(Cmp, u8)>),
    /// Rate the current selection/shown image (0 clears).
    SetRating(u8),
}

/// What `draw` returns to `main.rs` each frame.
#[derive(Default)]
pub struct FrameOutput {
    /// The central rect (logical px) left for the loupe image, in Loupe mode.
    pub loupe_rect: Option<egui::Rect>,
    /// Actions to apply this frame.
    pub actions: Vec<UiAction>,
}

/// Build the egui UI for one frame and return the loupe rect + actions.
pub fn draw(ui: &mut egui::Ui, app: &mut App) -> FrameOutput {
    let mut out = FrameOutput::default();
    match app.mode() {
        ViewMode::Grid => draw_grid(ui, app, &mut out),
        ViewMode::Loupe => draw_loupe(ui, app, &mut out),
    }
    out
}

/// Stars as a compact string, e.g. 3 → "★★★☆☆".
fn star_string(stars: u8) -> String {
    let s = stars.min(5) as usize;
    let mut out = String::new();
    for _ in 0..s {
        out.push('\u{2605}'); // ★
    }
    for _ in s..5 {
        out.push('\u{2606}'); // ☆
    }
    out
}

/// The shared filter bar (comparator buttons + value 0..5 + clear).
fn filter_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    ui.horizontal(|ui| {
        ui.label("Filter:");
        let (cur_cmp, cur_val) = match app.filter() {
            Some((c, v)) => (Some(c), v),
            None => (None, 0),
        };
        for cmp in [Cmp::Gte, Cmp::Eq, Cmp::Lte] {
            let selected = cur_cmp == Some(cmp);
            if ui.selectable_label(selected, cmp.symbol()).clicked() {
                if selected {
                    out.actions.push(UiAction::SetFilter(None));
                } else {
                    let v = if cur_val == 0 { 1 } else { cur_val };
                    out.actions.push(UiAction::SetFilter(Some((cmp, v))));
                }
            }
        }
        ui.separator();
        for v in 0u8..=5 {
            let selected = cur_cmp.is_some() && cur_val == v;
            if ui.selectable_label(selected, format!("{v}")).clicked() {
                let cmp = cur_cmp.unwrap_or(Cmp::Gte);
                out.actions.push(UiAction::SetFilter(Some((cmp, v))));
            }
        }
        ui.separator();
        if ui.button("Clear").clicked() {
            out.actions.push(UiAction::SetFilter(None));
        }
    });
}

fn draw_grid(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let thumb_px = app.thumb_px();
    let sel = app.sel();

    egui::Panel::top("toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Size");
            let mut px = thumb_px as f32;
            if ui
                .add(egui::Slider::new(&mut px, 96.0..=512.0).show_value(false))
                .changed()
            {
                out.actions.push(UiAction::SetThumbPx(px.round() as u32));
            }
            ui.separator();
            ui.label(format!("{} photos", app.visible_len()));
        });
        if app.filter_bar_open() {
            filter_bar(ui, app, out);
        }
    });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let cell = thumb_px as f32;
                let spacing = ui.spacing().item_spacing.x;
                let avail = ui.available_width();
                let cols = ((avail + spacing) / (cell + spacing)).floor().max(1.0) as usize;
                app.set_grid_cols(cols);

                let len = app.visible_len();
                egui::Grid::new("thumb_grid")
                    .num_columns(cols)
                    .spacing([spacing, spacing])
                    .show(ui, |ui| {
                        for pos in 0..len {
                            grid_cell(ui, app, pos, cell, sel, out);
                            if (pos + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    });
            });
    });
}

/// One grid cell: a thumbnail image-button with selection highlight + stars.
fn grid_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: usize,
    out: &mut FrameOutput,
) {
    let size = egui::vec2(cell, cell);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    // No outline in the grid's browse-first state (nothing selected yet).
    let selected = app.sel_active() && pos == sel;
    let bg = if selected {
        egui::Color32::from_rgb(40, 80, 140)
    } else {
        egui::Color32::from_gray(28)
    };
    ui.painter().rect_filled(rect, 4.0, bg);

    if let Some((tex, tw, th)) = app.thumb_texture_for(pos) {
        let inner = rect.shrink(4.0);
        let scale = (inner.width() / tw as f32).min(inner.height() / th as f32);
        let dw = tw as f32 * scale;
        let dh = th as f32 * scale;
        let img_rect = egui::Rect::from_center_size(inner.center(), egui::vec2(dw, dh));
        egui::Image::from_texture((tex.id(), egui::vec2(dw, dh))).paint_at(ui, img_rect);
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "\u{2026}",
            egui::FontId::proportional(18.0),
            egui::Color32::GRAY,
        );
    }

    let stars = app.rating_at(pos);
    ui.painter().text(
        egui::pos2(rect.left() + 6.0, rect.bottom() - 14.0),
        egui::Align2::LEFT_CENTER,
        star_string(stars),
        egui::FontId::proportional(13.0),
        egui::Color32::from_rgb(255, 210, 80),
    );

    // Prominent selection outline on top of the thumbnail.
    if selected {
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(3.0, egui::Color32::from_rgb(90, 160, 255)),
            egui::StrokeKind::Inside,
        );
    }

    if response.clicked() {
        out.actions.push(UiAction::Select(pos));
    }
    if response.double_clicked() {
        out.actions.push(UiAction::OpenLoupe(pos));
    }
}

fn draw_loupe(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let thumb_px = app.thumb_px();
    let sel = app.sel();
    // Only auto-scroll the strip to the selection when it actually changed.
    // Doing it every frame fights the user's clicks: the strip shifts between
    // press and release, so egui never registers the click.
    let follow = app.take_filmstrip_follow();

    if app.filter_bar_open() {
        egui::Panel::top("loupe_filter").show_inside(ui, |ui| {
            filter_bar(ui, app, out);
        });
    }

    let strip_h = (thumb_px as f32 * 0.55).clamp(72.0, 200.0) + 8.0;
    egui::Panel::bottom("filmstrip")
        .exact_size(strip_h)
        .show_inside(ui, |ui| {
            let cell = strip_h - 16.0;
            egui::ScrollArea::horizontal()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let len = app.visible_len();
                        for pos in 0..len {
                            let resp = filmstrip_cell(ui, app, pos, cell, sel, out);
                            if follow && pos == sel {
                                resp.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });
                });
        });

    // Central region: deliberately NOT a CentralPanel. Leaving it as the root
    // UI's unused rect is what makes egui report the pointer there as "not over
    // egui" (`is_pointer_over_egui` checks `!root_ui_available_rect.contains`),
    // so scroll (zoom), clicks, and Space+drag (pan) reach the app. A
    // CentralPanel would consume the rect and egui would claim all pointer input
    // over the image, killing zoom/pan. The wgpu image is drawn into this rect.
    let central = ui.available_rect_before_wrap();
    out.loupe_rect = Some(central);

    // Star overlay in its own foreground Area, so egui owns clicks on the stars
    // (only there) without claiming the rest of the image area.
    loupe_star_overlay(ui, app, central, out);
}

/// Clickable 0–5 star rating overlay near the top of the loupe image.
/// Clicking the Nth star sets rating N; clicking the current rating clears it.
fn loupe_star_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let current = app.selected_rating();
    let star_w = 26.0;
    let total_w = star_w * 5.0;
    let left = central.center().x - total_w / 2.0;
    let top = central.top() + 10.0;

    // A foreground Area: egui claims pointer input over the stars (so a click
    // rates instead of starting a pan) but nowhere else in the image.
    egui::Area::new(egui::Id::new("loupe_stars"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(left, top))
        .show(ui.ctx(), |ui| {
            ui.horizontal(|ui| {
                for i in 0..5u8 {
                    let (rect, resp) = ui
                        .allocate_exact_size(egui::vec2(star_w, star_w), egui::Sense::click());
                    let filled = (i + 1) <= current;
                    let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                    let color = if filled {
                        egui::Color32::from_rgb(255, 210, 80)
                    } else {
                        egui::Color32::from_gray(160)
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        glyph,
                        egui::FontId::proportional(22.0),
                        color,
                    );
                    if resp.clicked() {
                        let n = i + 1;
                        // Clicking the current rating clears it (Lightroom behavior).
                        let stars = if n == current { 0 } else { n };
                        out.actions.push(UiAction::SetRating(stars));
                    }
                }
            });
        });
}

/// One filmstrip cell. Returns the response so the caller can auto-scroll.
fn filmstrip_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: usize,
    out: &mut FrameOutput,
) -> egui::Response {
    let size = egui::vec2(cell, cell);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    let selected = pos == sel;
    let bg = if selected {
        egui::Color32::from_rgb(40, 80, 140)
    } else {
        egui::Color32::from_gray(24)
    };
    ui.painter().rect_filled(rect, 3.0, bg);

    if let Some((tex, tw, th)) = app.thumb_texture_for(pos) {
        let inner = rect.shrink(3.0);
        let scale = (inner.width() / tw as f32).min(inner.height() / th as f32);
        let dw = tw as f32 * scale;
        let dh = th as f32 * scale;
        let img_rect = egui::Rect::from_center_size(inner.center(), egui::vec2(dw, dh));
        egui::Image::from_texture((tex.id(), egui::vec2(dw, dh))).paint_at(ui, img_rect);
    }

    let stars = app.rating_at(pos);
    if stars > 0 {
        ui.painter().text(
            egui::pos2(rect.left() + 4.0, rect.bottom() - 8.0),
            egui::Align2::LEFT_CENTER,
            star_string(stars),
            egui::FontId::proportional(10.0),
            egui::Color32::from_rgb(255, 210, 80),
        );
    }

    // Prominent selection outline on top of the thumbnail (the thin background
    // border alone is easy to miss).
    if selected {
        ui.painter().rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(3.0, egui::Color32::from_rgb(90, 160, 255)),
            egui::StrokeKind::Inside,
        );
    }

    if response.clicked() {
        out.actions.push(UiAction::Select(pos));
    }
    response
}
