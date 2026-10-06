use super::grid::{thumbnail_cell, STRIP_CELL_STYLE};
use super::*;

use super::form::{self, Button, Role};
use crate::app::GRID_CELL_PT;
use crate::app::{
    spike_zoom_uv, App, CropEdge, FocusLevel, GroupView, PickHow, Region, TileFidelity,
};
use crate::image_decode;

pub(super) fn draw_loupe(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let sel = app.sel();

    if app.filmstrip_visible() {
        let strip_h = (GRID_CELL_PT * 0.55).clamp(72.0, 200.0) + 8.0;
        egui::Panel::bottom("filmstrip")
            .exact_size(strip_h)
            .show_inside(ui, |ui| {
                let cell = strip_h - 16.0;
                let cell_full = cell + ui.spacing().item_spacing.x;
                let len = app.visible_len();

                // egui's horizontal ScrollArea ignores a vertical mouse wheel,
                // so read the vertical delta here and step through photos.
                if ui.rect_contains_pointer(ui.max_rect()) {
                    let dy = ui.input(|i| i.smooth_scroll_delta.y);
                    if dy != 0.0 {
                        out.actions.push(UiAction::ScrollFilmstrip(dy));
                    }
                }

                // egui has no `show_columns`, so build only the cells in view by
                // hand and report the range so thumbnail loading follows scrolling.
                let mut area = egui::ScrollArea::horizontal().auto_shrink([false, false]);
                // The strip scrolls only when a new selection lands at the edge
                // of last frame's visible range. It then animates toward a
                // centered target and stops setting `scroll_offset` once there,
                // so it doesn't fight manual scrolling.
                if let Some(sel) = sel {
                    let last_sel_id = egui::Id::new("filmstrip_last_sel");
                    let target_id = egui::Id::new("filmstrip_scroll_target");
                    let anim_id = egui::Id::new("filmstrip_scroll_anim");

                    let prev_sel = ui.ctx().data(|d| d.get_temp::<usize>(last_sel_id));
                    let sel_changed = prev_sel != Some(sel);
                    ui.ctx().data_mut(|d| d.insert_temp(last_sel_id, sel));

                    if sel_changed {
                        let (first, last) = app.strip_range();
                        const EDGE_MARGIN: usize = 1;
                        let near_edge =
                            sel < first.saturating_add(EDGE_MARGIN) || sel + EDGE_MARGIN >= last;
                        if near_edge {
                            let max_scroll =
                                (len as f32 * cell_full - ui.available_width()).max(0.0);
                            let target = (sel as f32 * cell_full + cell_full * 0.5
                                - ui.available_width() * 0.5)
                                .clamp(0.0, max_scroll);
                            ui.ctx().data_mut(|d| d.insert_temp(target_id, target));
                        }
                    }

                    if let Some(target) = ui.ctx().data(|d| d.get_temp::<f32>(target_id)) {
                        let animated = ui.ctx().animate_value_with_time(anim_id, target, 0.15);
                        area = area.scroll_offset(egui::vec2(animated, 0.0));
                        if (animated - target).abs() > 0.5 {
                            app.request_redraw();
                        } else {
                            ui.ctx().data_mut(|d| d.remove::<f32>(target_id));
                        }
                    }
                }
                area.show_viewport(ui, |ui, viewport| {
                    let first = (viewport.min.x / cell_full).floor().max(0.0) as usize;
                    let last = ((viewport.max.x / cell_full).ceil() as usize).min(len);
                    app.set_visible_strip_range(first, last);

                    ui.horizontal(|ui| {
                        // Spacers keep the full content width for the scrollbar.
                        ui.add_space(first as f32 * cell_full);
                        for pos in first..last {
                            filmstrip_cell(ui, app, pos, cell, sel, out);
                        }
                        ui.add_space(len.saturating_sub(last) as f32 * cell_full);
                    });
                });

                region_focus_marker(ui, app, Region::Filmstrip);
            });
    }

    // Added after the filmstrip so it sits directly above it.
    if app.metadata_panel_visible() {
        draw_loupe_info_bar(ui, app, out);
    }

    // Not a CentralPanel. egui treats the root UI's unused rect as "not over
    // egui" (`is_pointer_over_egui`), so zoom, pan, and clicks there reach the
    // app. A CentralPanel would claim that input.
    let mut central = ui.available_rect_before_wrap();
    if app.spike.is_some() {
        // The pane's own controls sit in the info bar (`compare_actions`).
        if crate::app::spike_claims_pane() {
            egui::Panel::right("spike_tiles")
                .exact_size(central.width() / 2.0)
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| spike_tiles(ui, app, out));
                });
            central = ui.available_rect_before_wrap();
        } else {
            let whole = central;
            central =
                egui::Rect::from_min_max(whole.min, egui::pos2(whole.center().x, whole.max.y));
            let right =
                egui::Rect::from_min_max(egui::pos2(whole.center().x, whole.min.y), whole.max);
            ui.painter_at(right)
                .rect_filled(right, 0.0, theme::colors(ui.ctx()).panel);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(right));
            spike_tiles(&mut child, app, out);
        }
    }
    out.loupe_rect = Some(central);

    // `region_focus_marker` uses `ui.min_rect()`, which doesn't cover this
    // unclaimed rect, so draw against `central` directly.
    if app.focus() == Region::Detail && app.focus_level() == FocusLevel::Selected {
        ui.painter_at(central).rect_stroke(
            central.shrink(2.0),
            2.0,
            egui::Stroke::new(1.0f32, theme::colors(ui.ctx()).cursor),
            egui::StrokeKind::Outside,
        );
    }

    app.spike_marker_hovered = false;
    app.spike_photo_hovered = false;
    if app.crop_rect().is_some() {
        loupe_crop_overlay(ui, app, central, out);
    } else if app.touchup_active() {
        loupe_touchup_overlay(ui, app, central, out);
    } else if app.wb_picker_active() {
        loupe_wb_picker_overlay(ui, app, central, out);
    } else if app.compare() {
        loupe_compare_overlay(ui, central);
    } else {
        spike_zoom_marker(ui, app, central);
    }

    if app.dragging {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
}

fn loupe_touchup_overlay(ui: &mut egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let painter = ui.painter_at(central);
    let spots: &[crate::develop::TouchUp] = if app.touchup_spots_shown() {
        app.current_touchups()
    } else {
        &[]
    };
    for (i, t) in spots.iter().enumerate() {
        let c = app.loupe_tex_to_screen(central, t.center[0], t.center[1]);
        let radius = touchup_screen_radius(app, central, t.center, t.radius).max(3.0);
        let selected = app.touchup_selected() == Some(i);
        painter.circle_stroke(
            c,
            radius,
            egui::Stroke::new(
                if selected { 2.5_f32 } else { 1.2_f32 },
                if selected {
                    theme::colors(ui.ctx()).cursor
                } else {
                    egui::Color32::from_white_alpha(190)
                },
            ),
        );
        painter.circle_filled(
            c,
            3.0,
            if selected {
                theme::colors(ui.ctx()).cursor
            } else {
                egui::Color32::WHITE
            },
        );
    }
    egui::Area::new(egui::Id::new("loupe_touchup"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::click());
            if let Some(p) = resp.hover_pos() {
                // The brush itself is the cursor: a circle the size of the
                // spot a click would add.
                ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                let (u, v) = app.loupe_screen_to_tex(central, p);
                let r = touchup_screen_radius(app, central, [u, v], app.touchup_radius()).max(3.0);
                let painter = ui.painter_at(central);
                // Dark under light, so the ring reads on bright and dark photos.
                painter.circle_stroke(
                    p,
                    r,
                    egui::Stroke::new(2.5_f32, egui::Color32::from_black_alpha(160)),
                );
                painter.circle_stroke(p, r, egui::Stroke::new(1.2_f32, egui::Color32::WHITE));
                // The inner ring is where the feather ends and the fix is at
                // full strength.
                let feather = app.touchup_feather();
                if feather < 1.0 {
                    painter.circle_stroke(
                        p,
                        r * (1.0 - feather),
                        egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha(170)),
                    );
                }
                painter.circle_filled(p, 1.5, egui::Color32::WHITE);

                // The wheel sizes the brush instead of zooming, and Shift+wheel
                // feathers it. Alt still pans through `App::on_scroll`.
                let (delta, shift, alt) =
                    ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift, i.modifiers.alt));
                if !alt && shift {
                    // macOS turns a Shift+wheel into horizontal scrolling.
                    let d = if delta.y != 0.0 { delta.y } else { delta.x };
                    if d != 0.0 {
                        out.actions.push(UiAction::SetTouchUpFeather(
                            app.touchup_feather() + d * 0.0025,
                        ));
                    }
                } else if !alt && delta.y != 0.0 {
                    out.actions.push(UiAction::SetTouchUpRadius(
                        app.touchup_radius() * (delta.y * 0.0025).exp(),
                    ));
                }
            }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (u, v) = app.loupe_screen_to_tex(central, p);
                    let mut hit = None;
                    for (i, t) in spots.iter().enumerate() {
                        let (radius_u, radius_v) = app.touchup_uv_radii(t.radius);
                        let dx = (u - t.center[0]) / radius_u;
                        let dy = (v - t.center[1]) / radius_v;
                        if dx * dx + dy * dy <= 1.0 {
                            hit = Some(i);
                            break;
                        }
                    }
                    if let Some(i) = hit {
                        out.actions.push(UiAction::SelectTouchUp(i));
                    } else if (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v) {
                        out.actions.push(UiAction::TouchUpClick(u, v));
                    }
                }
            }
        });
}

/// On-screen radius of a touch-up of `radius` centered at texture `center`,
/// averaged across the two axes since the stored radius is relative to the
/// image's shorter side.
fn touchup_screen_radius(app: &App, central: egui::Rect, center: [f32; 2], radius: f32) -> f32 {
    let c = app.loupe_tex_to_screen(central, center[0], center[1]);
    let (radius_u, radius_v) = app.touchup_uv_radii(radius);
    let edge_u = app.loupe_tex_to_screen(central, center[0] + radius_u, center[1]);
    let edge_v = app.loupe_tex_to_screen(central, center[0], center[1] + radius_v);
    ((edge_u - c).length() + (edge_v - c).length()) * 0.5
}

/// A transparent click-catcher over the image while the WB picker is armed.
/// The app disarms the picker after the click.
fn loupe_wb_picker_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    egui::Area::new(egui::Id::new("loupe_wb_picker"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::click());
            let painter = ui.painter_at(central);
            // A faint tint shows that picker mode is on.
            painter.rect_filled(central, 0.0, egui::Color32::from_white_alpha(10));
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (u, v) = app.loupe_screen_to_tex(central, p);
                    out.actions.push(UiAction::PickWhiteBalance(u, v));
                }
            }
        });
}

/// Divider and labels for before/after. The wgpu renderer draws the two halves.
fn loupe_compare_overlay(ui: &egui::Ui, central: egui::Rect) {
    let painter = ui.painter_at(central);
    let mid_x = central.center().x;
    painter.line_segment(
        [
            egui::pos2(mid_x, central.min.y),
            egui::pos2(mid_x, central.max.y),
        ],
        egui::Stroke::new(1.0f32, theme::colors(ui.ctx()).divider),
    );
    // Shadowed text so labels read over any image.
    let label = |p: egui::Pos2, align: egui::Align2, text: &str| {
        let font = egui::FontId::proportional(font_size::px(ui.style(), 13.0));
        painter.text(
            p + egui::vec2(1.0, 1.0),
            align,
            text,
            font.clone(),
            egui::Color32::BLACK,
        );
        painter.text(p, align, text, font, egui::Color32::WHITE);
    };
    let pad = 8.0;
    label(
        central.min + egui::vec2(pad, pad),
        egui::Align2::LEFT_TOP,
        t().before,
    );
    label(
        egui::pos2(central.max.x - pad, central.min.y + pad),
        egui::Align2::RIGHT_TOP,
        t().after,
    );
}

/// The info bar below the image: filename and rating centered, then the
/// selection controls. Exposure sits under the Develop panel's histogram.
fn draw_loupe_info_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let row_h = font_size::px(ui.style(), 36.0);
    // The photo's actions share the row when they fit beside the filename
    // group, else they take a second row. Their width is last frame's.
    let actions_id = egui::Id::new("loupe_actions_w");
    let actions_w = ui.ctx().data(|d| d.get_temp::<f32>(actions_id));
    let bar_w = ui.available_width();
    let measure_pad = font_size::px(ui.style(), 14.0);
    let group_w_guess = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(egui::Id::new("loupe_group_w")))
        .unwrap_or(0.0);
    let fits = actions_w.is_none_or(|w| group_w_guess + w + 4.0 * measure_pad <= bar_w);
    let bar_h = if fits { row_h } else { 2.0 * row_h };
    egui::Panel::bottom("loupe_info_bar")
        .exact_size(bar_h)
        .show_inside(ui, |ui| {
            let full = ui.max_rect();
            let rect = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), row_h));
            let filename = app
                .selected_path()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();

            let painter = ui.painter();
            let main_font = egui::FontId::proportional(font_size::px(ui.style(), 13.0));
            let colors = theme::colors(ui.ctx());
            let text_color = colors.value;
            let main_y = rect.center().y;
            let pad = font_size::px(ui.style(), 14.0);

            // Center the filename and stars as one group.
            let star_w = font_size::px(ui.style(), 20.0);
            let stars_total_w = star_w * 5.0;
            let group_gap = font_size::px(ui.style(), 10.0);
            let filename_w = if filename.is_empty() {
                0.0
            } else {
                painter
                    .layout_no_wrap(filename.clone(), main_font.clone(), text_color)
                    .size()
                    .x
            };
            // A grouped photo's view icons join the centered
            // group, so the row fits until it is wider than the bar, then
            // starts at the left.
            let tabs_w = if app.shown_in_group() {
                3.0 * group_gap + group_view_buttons_width(ui)
            } else {
                0.0
            };
            let score = app.shown_score().map(|(score, stale)| {
                let galley = painter.layout_no_wrap(
                    score.value.to_string(),
                    main_font.clone(),
                    super::grid::score_color(&colors, stale),
                );
                (score, stale, galley)
            });
            let score_w = score
                .as_ref()
                .map_or(0.0, |(_, _, g)| 3.0 * group_gap + g.size().x);
            let group_w = filename_w
                + if filename.is_empty() { 0.0 } else { group_gap }
                + stars_total_w
                + 4.0 * ui.spacing().item_spacing.x
                + score_w
                + tabs_w;
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new("loupe_group_w"), group_w));
            // With the actions beside it, the group and the actions center
            // as one run; alone on its row the group centers.
            let gap = 3.0 * group_gap;
            let run_w = match actions_w {
                Some(w) if fits => group_w + gap + w,
                _ => group_w,
            };
            let group_left = (rect.center().x - run_w / 2.0).max(rect.left() + pad);

            if !filename.is_empty() {
                painter.text(
                    egui::pos2(group_left, main_y),
                    egui::Align2::LEFT_CENTER,
                    &filename,
                    main_font,
                    text_color,
                );
            }

            let stars_left =
                group_left + filename_w + if filename.is_empty() { 0.0 } else { group_gap };
            let stars_rect = egui::Rect::from_center_size(
                egui::pos2(stars_left + stars_total_w / 2.0, main_y),
                egui::vec2(stars_total_w, star_w),
            );
            // The stars' item spacing runs past `stars_rect`, so what sits
            // right of them goes by the row they actually drew.
            let stars_drawn = ui
                .scope_builder(egui::UiBuilder::new().max_rect(stars_rect), |ui| {
                    ui.horizontal_centered(|ui| {
                        let current = app.selected_rating();
                        for i in 0..5u8 {
                            let (r, resp) = ui.allocate_exact_size(
                                egui::vec2(star_w, star_w),
                                egui::Sense::click(),
                            );
                            let filled = (i + 1) <= current;
                            let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                            let color = if filled {
                                theme::colors(ui.ctx()).star
                            } else {
                                theme::colors(ui.ctx()).label
                            };
                            ui.painter().text(
                                r.center(),
                                egui::Align2::CENTER_CENTER,
                                glyph,
                                egui::FontId::proportional(font_size::px(ui.style(), 18.0)),
                                color,
                            );
                            if resp.clicked() {
                                let n = i + 1;
                                // Clicking the current rating clears it, as in Lightroom.
                                let stars = if n == current { 0 } else { n };
                                out.actions.push(UiAction::SetRating(stars));
                            }
                        }
                    });
                })
                .response
                .rect;

            if let Some(label) = app.selected_label() {
                ui.painter().circle_filled(
                    egui::pos2(stars_rect.right() + group_gap, main_y),
                    font_size::px(ui.style(), 5.0),
                    label_color(label),
                );
            }

            // Past the label dot, like the tabs below.
            let mut row_right = stars_drawn.right();
            if let Some((score, stale, galley)) = score {
                let label_right = if app.selected_label().is_some() {
                    stars_rect.right() + 2.0 * group_gap
                } else {
                    stars_drawn.right()
                };
                let at = egui::pos2(
                    stars_drawn.right().max(label_right) + group_gap,
                    main_y - galley.size().y / 2.0,
                );
                let hit = egui::Rect::from_min_size(at, galley.size());
                ui.painter().galley(at, galley, colors.label);
                ui.interact(hit, egui::Id::new("loupe_score"), egui::Sense::hover())
                    .on_hover_text(super::grid::score_tip(&score, stale));
                row_right = row_right.max(hit.right());
            }

            // Past the label dot, so the two never overlap.
            if app.shown_in_group() {
                let view_rect = egui::Rect::from_min_max(
                    egui::pos2(row_right + 3.0 * group_gap, rect.top()),
                    rect.right_bottom(),
                );
                let layout = egui::Layout::left_to_right(egui::Align::Center);
                ui.scope_builder(
                    egui::UiBuilder::new().max_rect(view_rect).layout(layout),
                    |ui| group_view_buttons(ui, app, out),
                );
            }

            let w = actions_w.unwrap_or(0.0);
            let actions_rect = if fits {
                let left = group_left + group_w + gap;
                egui::Rect::from_min_max(
                    egui::pos2(left, rect.top()),
                    egui::pos2(left + w, rect.bottom()),
                )
            } else {
                let left = (full.center().x - w / 2.0).max(full.left() + pad);
                egui::Rect::from_min_max(
                    egui::pos2(left, rect.bottom()),
                    egui::pos2(full.right() - pad, full.bottom()),
                )
            };
            let layout = egui::Layout::left_to_right(egui::Align::Center);
            let drawn = ui
                .scope_builder(
                    egui::UiBuilder::new().max_rect(actions_rect).layout(layout),
                    |ui| {
                        super::toolbar::toolbar_spacing(ui);
                        if app.spike.is_some() {
                            compare_actions(ui, app, out);
                        } else {
                            super::toolbar::loupe_actions(ui, app, out);
                        }
                    },
                )
                .response
                .rect
                .width();
            if actions_w.is_none_or(|l| (l - drawn).abs() > 0.5) {
                ui.ctx().data_mut(|d| d.insert_temp(actions_id, drawn));
                ui.ctx().request_repaint();
            }

            // Subject selection is a way of viewing the photo, not an edit, so
            // it lives here rather than in the Develop panel.
            if crate::app::SHOW_SELECTION_BUTTONS && App::selection_supported() {
                let sel_size = egui::vec2(190.0, 22.0) * font_size::px(ui.style(), 1.0);
                let sel_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.right() - pad - sel_size.x, main_y - sel_size.y / 2.0),
                    sel_size,
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(sel_rect), |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let inverted = app.selection_inverted();
                            if ui
                                .add_enabled(
                                    app.selection_on(),
                                    egui::Button::selectable(inverted, t().invert),
                                )
                                .on_hover_text(t().invert_tip)
                                .clicked()
                            {
                                out.actions.push(UiAction::ToggleSelectionInvert);
                            }

                            // The label shows pending and "no subject" states, which
                            // would otherwise look like a broken button.
                            let label = if !app.selection_on() {
                                t().show_selection
                            } else if app.selection_pending() {
                                t().selection_pending
                            } else if app.current_selection().is_some() {
                                t().show_selection
                            } else {
                                t().no_subject
                            };
                            if ui
                                .add(egui::Button::selectable(app.selection_on(), label))
                                .on_hover_text(t().show_selection_tip)
                                .clicked()
                            {
                                out.actions.push(UiAction::ToggleSelection);
                            }
                        });
                    });
                });
            }
        });
}

/// The exposure readout under the histogram, Lightroom's order:
/// `ISO 200`, `55 mm`, `f/2.8`, `1/125 s`. Missing fields are omitted.
pub(super) fn exposure_parts(meta: &image_decode::ImageMetadata) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(iso) = meta.iso {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(fl) = meta.focal_length {
        parts.push(format!("{} mm", fl.round() as i64));
    }
    if let Some(f) = meta.f_number {
        parts.push(format!("f/{f:.1}"));
    }
    if let Some(t) = meta.exposure_time {
        let shutter = format_shutter(t);
        if let Some(value) = shutter.strip_suffix('s') {
            parts.push(format!("{value} s"));
        }
    }
    parts
}

/// A fraction below one second (`1/250s`), otherwise whole or one-decimal
/// seconds.
pub(super) fn format_shutter(seconds: f64) -> String {
    if seconds <= 0.0 {
        return String::new();
    }
    if seconds < 1.0 {
        format!("1/{:.0}s", (1.0 / seconds).round())
    } else if (seconds - seconds.round()).abs() < 0.05 {
        format!("{seconds:.0}s")
    } else {
        format!("{seconds:.1}s")
    }
}

/// Decimal units, as Finder shows them: `3.7 MB`.
pub(super) fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `4032 × 3024 (12.2 MP)`.
pub(super) fn format_dimensions(w: u32, h: u32) -> String {
    let mp = w as f64 * h as f64 / 1_000_000.0;
    format!("{w} \u{d7} {h} ({mp:.1} MP)")
}

pub(super) fn format_aperture(f_number: f64) -> String {
    format!("f/{f_number:.1}")
}

/// Whole millimeters stay whole (`50 mm`), phone lenses keep a decimal
/// (`4.2 mm`).
pub(super) fn format_focal_length(mm: f64) -> String {
    if (mm - mm.round()).abs() < 0.05 {
        format!("{mm:.0} mm")
    } else {
        format!("{mm:.1} mm")
    }
}

/// `+1.3 EV`, `-0.7 EV`, and a plain `0 EV` with no sign.
pub(super) fn format_exposure_bias(ev: f64) -> String {
    if ev.abs() < 0.05 {
        "0 EV".to_string()
    } else {
        format!("{ev:+.1} EV")
    }
}

/// Five decimals is about a meter, finer than phone GPS.
pub(super) fn format_latitude(lat: f64) -> String {
    format!(
        "{:.5}\u{b0} {}",
        lat.abs(),
        if lat < 0.0 { 'S' } else { 'N' }
    )
}

pub(super) fn format_longitude(lon: f64) -> String {
    format!(
        "{:.5}\u{b0} {}",
        lon.abs(),
        if lon < 0.0 { 'W' } else { 'E' }
    )
}

pub(super) fn format_altitude(m: f64) -> String {
    format!("{m:.0} m")
}

pub(super) fn maps_url(gps: &image_decode::Gps) -> String {
    format!(
        "https://www.google.com/maps/search/?api=1&query={:.6},{:.6}",
        gps.lat, gps.lon
    )
}

/// The crop overlay and its drag handling. The crop rect is in texture space.
/// `App::loupe_tex_to_screen` maps it to screen, accounting for zoom, pan, and
/// rotation.
fn loupe_crop_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let Some(rect) = app.crop_rect() else { return };

    let corner = |u, v| app.loupe_tex_to_screen(central, u, v);
    let tl = corner(rect.left, rect.top);
    let tr = corner(rect.right, rect.top);
    let bl = corner(rect.left, rect.bottom);
    let br = corner(rect.right, rect.bottom);
    let edges = [
        (CropEdge::Left, tl, bl),
        (CropEdge::Right, tr, br),
        (CropEdge::Top, tl, tr),
        (CropEdge::Bottom, bl, br),
    ];
    // `from_points` handles rotation swapping which corner is top-left.
    let crop_screen = egui::Rect::from_points(&[tl, tr, bl, br]).intersect(central);

    egui::Area::new(egui::Id::new("loupe_crop"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::drag());
            let painter = ui.painter_at(central);

            let dim = egui::Color32::from_black_alpha(150);
            let r = crop_screen;
            let full = central;
            let bands = [
                egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, r.min.y)), // top
                egui::Rect::from_min_max(egui::pos2(full.min.x, r.max.y), full.max), // bottom
                egui::Rect::from_min_max(
                    egui::pos2(full.min.x, r.min.y),
                    egui::pos2(r.min.x, r.max.y),
                ), // left
                egui::Rect::from_min_max(
                    egui::pos2(r.max.x, r.min.y),
                    egui::pos2(full.max.x, r.max.y),
                ), // right
            ];
            for b in bands {
                if b.is_positive() {
                    painter.rect_filled(b, 0.0, dim);
                }
            }

            // Outline and rule-of-thirds guides.
            let line = egui::Color32::from_gray(235);
            painter.rect_stroke(
                r,
                0.0,
                egui::Stroke::new(1.5f32, line),
                egui::StrokeKind::Inside,
            );
            for i in 1..3 {
                let fx = r.min.x + r.width() * i as f32 / 3.0;
                let fy = r.min.y + r.height() * i as f32 / 3.0;
                let faint = egui::Color32::from_white_alpha(70);
                painter.line_segment(
                    [egui::pos2(fx, r.min.y), egui::pos2(fx, r.max.y)],
                    egui::Stroke::new(1.0f32, faint),
                );
                painter.line_segment(
                    [egui::pos2(r.min.x, fy), egui::pos2(r.max.x, fy)],
                    egui::Stroke::new(1.0f32, faint),
                );
            }
            // A handle dot at each edge midpoint.
            for (_, a, b) in edges {
                let mid = egui::pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                painter.circle_filled(mid, 5.0, line);
            }

            // An edge within the grab distance wins; otherwise a point inside
            // the rect moves the whole crop.
            const EDGE_GRAB_PX: f32 = 24.0;
            let inside_rect = |p: egui::Pos2| {
                let (u, v) = app.loupe_screen_to_tex(central, p);
                u >= rect.left && u <= rect.right && v >= rect.top && v <= rect.bottom
            };

            if let Some(p) = resp.hover_pos() {
                let icon = if let Some(edge) = nearest_edge(&edges, p, EDGE_GRAB_PX) {
                    match edge {
                        CropEdge::Left | CropEdge::Right => egui::CursorIcon::ResizeHorizontal,
                        CropEdge::Top | CropEdge::Bottom => egui::CursorIcon::ResizeVertical,
                    }
                } else if inside_rect(p) {
                    egui::CursorIcon::Move
                } else {
                    egui::CursorIcon::Default
                };
                ui.ctx().set_cursor_icon(icon);
            }

            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    if let Some(edge) = nearest_edge(&edges, p, EDGE_GRAB_PX) {
                        out.actions.push(UiAction::CropGrab(edge));
                    } else if inside_rect(p) {
                        let (u, v) = app.loupe_screen_to_tex(central, p);
                        out.actions.push(UiAction::CropGrabMove(u, v));
                    }
                }
            }
            if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (u, v) = app.loupe_screen_to_tex(central, p);
                    out.actions.push(UiAction::CropDragTo(u, v));
                }
            }
            if resp.drag_stopped() {
                out.actions.push(UiAction::CropRelease);
            }
        });
}

/// The edge nearest to `p`, if within `threshold` px.
fn nearest_edge(
    edges: &[(CropEdge, egui::Pos2, egui::Pos2)],
    p: egui::Pos2,
    threshold: f32,
) -> Option<CropEdge> {
    let mut best: Option<(CropEdge, f32)> = None;
    for &(edge, a, b) in edges {
        let d = dist_to_segment(p, a, b);
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((edge, d));
        }
    }
    best.filter(|&(_, d)| d <= threshold).map(|(e, _)| e)
}

fn dist_to_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    (p - proj).length()
}

fn filmstrip_cell(
    ui: &mut egui::Ui,
    app: &App,
    pos: usize,
    cell: f32,
    sel: Option<usize>,
    out: &mut FrameOutput,
) -> egui::Response {
    let primary = sel == Some(pos);
    let response = thumbnail_cell(ui, app, pos, cell, primary, primary, &STRIP_CELL_STYLE);
    if response.clicked() {
        out.actions.push(UiAction::Select(pos));
        out.actions.push(UiAction::Focus(Region::Filmstrip));
    }
    response
}

/// Two icons right of a grouped photo's rating stars, one per
/// `GroupView`: a lone frame for the photo alone, and a large frame beside
/// a column of small ones for the Compare pane. The current view's icon
/// stays lit.
fn group_view_buttons(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    ui.spacing_mut().item_spacing.x = font_size::px(ui.style(), VIEW_ICON_GAP);
    let t = crate::i18n::t();
    let current = app.group_view();
    for (view, tip) in [
        (GroupView::Edit, t.view_single),
        (GroupView::Compare, t.view_compare),
    ] {
        if group_view_icon(ui, view, current == view)
            .on_hover_text(tip)
            .clicked()
            && current != view
        {
            out.actions.push(UiAction::SetGroupView(view));
        }
    }
}

const VIEW_ICON_SIZE: egui::Vec2 = egui::vec2(30.0, 22.0);
const VIEW_ICON_GAP: f32 = 2.0;

/// How wide `group_view_buttons` draws.
fn group_view_buttons_width(ui: &egui::Ui) -> f32 {
    font_size::px(ui.style(), 2.0 * VIEW_ICON_SIZE.x + VIEW_ICON_GAP)
}

/// One of `group_view_buttons`' icons, drawn as outlines so it follows the
/// theme's text color.
fn group_view_icon(ui: &mut egui::Ui, view: GroupView, selected: bool) -> egui::Response {
    let size = egui::vec2(
        font_size::px(ui.style(), VIEW_ICON_SIZE.x),
        font_size::px(ui.style(), VIEW_ICON_SIZE.y),
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let visuals = ui.style().interact_selectable(&response, selected);
    let painter = ui.painter();
    if selected || response.hovered() {
        painter.rect_filled(rect, visuals.corner_radius, visuals.weak_bg_fill);
    }
    let stroke = egui::Stroke::new(font_size::px(ui.style(), 1.3), visuals.fg_stroke.color);
    let unit = font_size::px(ui.style(), 1.0);
    let art = egui::Rect::from_center_size(rect.center(), egui::vec2(18.0, 12.0) * unit);
    let radius = 1.5 * unit;
    let frame = |r: egui::Rect| painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Middle);
    match view {
        GroupView::Edit => {
            frame(art);
        }
        GroupView::Compare => {
            let gap = 2.0 * unit;
            let small_w = 4.0 * unit;
            frame(egui::Rect::from_min_max(
                art.min,
                egui::pos2(art.right() - small_w - gap, art.bottom()),
            ));
            let small_h = (art.height() - 2.0 * gap) / 3.0;
            for i in 0..3 {
                let top = art.top() + i as f32 * (small_h + gap);
                frame(egui::Rect::from_min_size(
                    egui::pos2(art.right() - small_w, top),
                    egui::vec2(small_w, small_h),
                ));
            }
        }
    }
    response
}

/// The Compare pane's controls, in the info bar while the pane is open:
/// how to load the tiles on native, Set as representative, live while
/// exactly one member is picked, then the actions on the picks
/// (`toolbar::pick_actions`).
fn compare_actions(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let t = crate::i18n::t();
    let picks = app.group_picks().len();
    #[cfg(not(target_arch = "wasm32"))]
    {
        ui.label(t.tile_load);
        for (f, label) in [
            (TileFidelity::Speed, t.tile_speed),
            (TileFidelity::Full, t.tile_full),
        ] {
            let current = app.tile_fidelity();
            if ui.radio(current == f, label).clicked() && current != f {
                out.actions.push(UiAction::SetTileFidelity(f));
            }
        }
    }
    let set = Button {
        label: t.set_as_rep,
        role: Role::Primary,
        enabled: picks == 1,
    };
    if form::button(ui, &set).clicked() {
        out.actions.push(UiAction::SetPickAsRep);
    }
    super::toolbar::pick_actions(ui, app, out);
}

/// SPIKE: one tile per member on the group's current page, each sampling
/// the zoom square. The representative is outlined in the selection color
/// and the picked members in the cursor color. A click picks only that
/// tile, Cmd-click toggles it, and Shift-click picks the run from the last
/// click across pages; the info bar's buttons act on the picks. Each
/// tile carries its member's stars and score (`tile_marks`). A page
/// holds `SPIKE_PAGE` tiles, and a group with more gets arrows under the
/// grid. The grid keeps one size across pages, so a short last page leaves
/// cells empty.
fn spike_tiles(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let colors = theme::colors(ui.ctx());
    let Some(tiles) = app.spike.as_ref() else {
        return;
    };
    let square = app.spike_square();
    let full = app.tile_fidelity() == TileFidelity::Full;
    let picks = app.group_picks();
    let fit = tiles.group_len().clamp(1, crate::app::SPIKE_PAGE);
    let cols = (fit as f32).sqrt().ceil().max(1.0) as usize;
    let rows = fit.div_ceil(cols);
    let pad = 8.0;
    let paged = tiles.pages() > 1;
    let nav_h = if paged {
        ui.spacing().interact_size.y + pad
    } else {
        0.0
    };
    let avail = ui.available_size();
    let tile = ((avail.x - pad * (cols as f32 - 1.0)) / cols as f32)
        .min((avail.y - nav_h - pad * (rows as f32 - 1.0)) / rows as f32)
        .max(16.0);
    ui.spacing_mut().item_spacing = egui::vec2(pad, pad);
    for row in tiles.members.chunks(cols) {
        ui.horizontal(|ui| {
            for m in row {
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(tile, tile), egui::Sense::click_and_drag());
                let Some(m) = m else {
                    ui.painter().rect_filled(rect, 0.0, colors.divider);
                    continue;
                };
                let (id, uv) = m.texture(square);
                ui.painter().image(id, rect, uv, egui::Color32::WHITE);
                if full && m.full_loading(square) {
                    let side = font_size::px(ui.style(), 16.0);
                    let at = egui::Rect::from_min_size(
                        rect.right_top() + egui::vec2(-side - pad / 2.0, pad / 2.0),
                        egui::vec2(side, side),
                    );
                    ui.painter().circle_filled(
                        at.center(),
                        side * 0.7,
                        egui::Color32::from_black_alpha(140),
                    );
                    egui::Spinner::new()
                        .size(side)
                        .color(egui::Color32::WHITE)
                        .paint_at(ui, at);
                }
                let is_rep = m.path == tiles.shown;
                tile_marks(ui, app, rect, &m.path, is_rep, out);
                let picked = picks.contains(&m.path.as_path());
                if resp.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    drag_tile_square(app, tile, resp.drag_delta(), out);
                } else if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    let mods = ui.input(|i| i.modifiers);
                    let how = if mods.shift {
                        PickHow::Range
                    } else if mods.command {
                        PickHow::Toggle
                    } else {
                        PickHow::Only
                    };
                    out.actions.push(UiAction::PickGroupTile {
                        path: m.path.clone(),
                        how,
                    });
                }
                // One width for every tile, so only the color tells the
                // representative and the picks from the rest.
                let color = if is_rep {
                    colors.selection
                } else if picked {
                    colors.cursor
                } else if resp.hovered() {
                    egui::Color32::from_gray(160)
                } else {
                    egui::Color32::from_gray(110)
                };
                ui.painter().rect_stroke(
                    rect,
                    0.0,
                    egui::Stroke::new(TILE_BORDER, color),
                    egui::StrokeKind::Outside,
                );
            }
        });
    }
    if paged {
        // Keep the arrows put on a short last page.
        let missing = rows - tiles.members.len().div_ceil(cols);
        ui.add_space(missing as f32 * (tile + pad));
        spike_page_nav(
            ui,
            tiles,
            cols as f32 * tile + pad * (cols as f32 - 1.0),
            out,
        );
    }
}

/// A drag on a tile moves what every tile shows, as grabbing the photo
/// would: the square moves against the drag, by the photo it covers.
fn drag_tile_square(app: &App, tile: f32, delta: egui::Vec2, out: &mut FrameOutput) {
    let Some(tiles) = app.spike.as_ref() else {
        return;
    };
    let Some(&(w, h)) = tiles.sizes.get(&tiles.shown) else {
        return;
    };
    if delta == egui::Vec2::ZERO {
        return;
    }
    let uv = spike_zoom_uv(w, h, app.spike_center, app.spike_side);
    let moved = uv.center() - delta / tile * uv.size();
    let center = spike_zoom_uv(w, h, moved, app.spike_side).center();
    out.actions.push(UiAction::SetSpikeCenter(center));
}

/// Every Compare tile's border width.
const TILE_BORDER: f32 = 4.0;

/// A band along a tile's bottom with the member's stars, which rate it as
/// the info bar's do, and its score, which explains itself on hover. A tile
/// too narrow for the band goes without. The stars sit over the tile, so a
/// click on them rates and does not pick.
fn tile_marks(
    ui: &mut egui::Ui,
    app: &App,
    rect: egui::Rect,
    path: &std::path::Path,
    is_rep: bool,
    out: &mut FrameOutput,
) {
    let colors = theme::colors(ui.ctx());
    let star_w = font_size::px(ui.style(), 16.0);
    let pad = font_size::px(ui.style(), 4.0);
    let score = app.member_score(path);
    let font = egui::FontId::proportional(font_size::px(ui.style(), 12.0));
    // The band is dark in every theme, so the score is light, dimmed when
    // stale as `grid::score_color` dims it.
    let galley = score.map(|(s, stale)| {
        let color = egui::Color32::from_white_alpha(if stale { 110 } else { 230 });
        ui.painter()
            .layout_no_wrap(s.value.to_string(), font.clone(), color)
    });
    let score_w = galley.as_ref().map_or(0.0, |g| g.size().x + pad);
    if rect.width() < 5.0 * star_w + score_w + 2.0 * pad {
        return;
    }
    // The representative says so after its stars, when the band has room.
    let rep_label = is_rep
        .then(|| {
            ui.painter().layout_no_wrap(
                crate::i18n::t().representative.to_string(),
                font.clone(),
                colors.selection,
            )
        })
        .filter(|g| rect.width() >= 5.0 * star_w + g.size().x + score_w + 3.0 * pad);
    let band = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - star_w - pad),
        rect.right_bottom(),
    );
    ui.painter()
        .rect_filled(band, 0.0, egui::Color32::from_black_alpha(150));
    let y = band.center().y;
    let current = app.member_rating(path);
    for i in 0..5u8 {
        let n = i + 1;
        let r = egui::Rect::from_center_size(
            egui::pos2(band.left() + pad + star_w * (i as f32 + 0.5), y),
            egui::vec2(star_w, star_w),
        );
        let resp = ui.interact(
            r,
            egui::Id::new(("tile_star", path, i)),
            egui::Sense::click(),
        );
        let filled = n <= current;
        let color = if filled {
            colors.star
        } else if resp.hovered() {
            colors.star.gamma_multiply(0.6)
        } else {
            egui::Color32::from_white_alpha(170)
        };
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            if filled { "\u{2605}" } else { "\u{2606}" },
            egui::FontId::proportional(font_size::px(ui.style(), 14.0)),
            color,
        );
        if resp.clicked() {
            // Clicking the current rating clears it, as in the info bar.
            let stars = if n == current { 0 } else { n };
            out.actions.push(UiAction::RateGroupMember {
                path: path.to_path_buf(),
                stars,
            });
        }
    }
    if let Some(g) = rep_label {
        let at = egui::pos2(band.left() + 2.0 * pad + 5.0 * star_w, y - g.size().y / 2.0);
        ui.painter().galley(at, g, egui::Color32::WHITE);
    }
    if let (Some((score, stale)), Some(galley)) = (score, galley) {
        let at = egui::pos2(
            band.right() - pad - galley.size().x,
            y - galley.size().y / 2.0,
        );
        let hit = egui::Rect::from_min_size(at, galley.size());
        ui.painter().galley(at, galley, egui::Color32::WHITE);
        ui.interact(
            hit,
            egui::Id::new(("tile_score", path)),
            egui::Sense::hover(),
        )
        .on_hover_text(super::grid::score_tip(score, stale));
    }
}

/// SPIKE: previous and next arrows either side of the page's member range,
/// as wide as the tile grid above them.
fn spike_page_nav(
    ui: &mut egui::Ui,
    tiles: &crate::app::SpikeTiles,
    width: f32,
    out: &mut FrameOutput,
) {
    let first = tiles.page * crate::app::SPIKE_PAGE + 1;
    let last = first + tiles.page_paths().len() - 1;
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(width, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            if ui
                .add_enabled(tiles.page > 0, egui::Button::new("‹"))
                .clicked()
            {
                out.actions.push(UiAction::SpikePage(tiles.page - 1));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(tiles.page + 1 < tiles.pages(), egui::Button::new("›"))
                    .clicked()
                {
                    out.actions.push(UiAction::SpikePage(tiles.page + 1));
                }
                ui.centered_and_justified(|ui| {
                    ui.label((crate::i18n::t().group_page)(
                        first,
                        last,
                        tiles.group_len(),
                    ));
                });
            });
        },
    );
}

/// SPIKE: outline on the shown photo of the square the tiles zoom into. A
/// drag that starts inside it moves it, and one that starts elsewhere on
/// the photo centers it under the pointer and carries it along. egui claims
/// both presses so the Loupe does not pan.
fn spike_zoom_marker(ui: &egui::Ui, app: &mut App, central: egui::Rect) {
    let Some(tiles) = app.spike.as_ref() else {
        return;
    };
    let Some(&(w, h)) = tiles.sizes.get(&tiles.shown) else {
        return;
    };
    let uv = spike_zoom_uv(w, h, app.spike_center, app.spike_side);
    let rect = egui::Rect::from_two_pos(
        app.loupe_tex_to_screen(central, uv.min.x, uv.min.y),
        app.loupe_tex_to_screen(central, uv.max.x, uv.max.y),
    );
    // Beneath the square's own area, so a press on the square reaches it.
    // Held Space leaves the photo to the Loupe, which pans on Space+drag.
    let photo = (!app.space_down).then(|| {
        egui::Area::new(egui::Id::new("spike_zoom_photo"))
            .order(egui::Order::Middle)
            .fixed_pos(central.min)
            .show(ui.ctx(), |ui| {
                ui.allocate_exact_size(central.size(), egui::Sense::click_and_drag())
                    .1
            })
            .inner
    });
    let photo_dragged = photo.as_ref().is_some_and(|p| p.dragged());
    let resp = egui::Area::new(egui::Id::new("spike_zoom_marker"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .show(ui.ctx(), |ui| {
            ui.allocate_exact_size(rect.size(), egui::Sense::click_and_drag())
                .1
        })
        .inner;
    app.spike_marker_hovered = resp.hovered() || resp.dragged();
    app.spike_photo_hovered =
        app.spike_marker_hovered || photo.as_ref().is_some_and(|p| p.hovered()) || photo_dragged;
    // A click anywhere on the photo, the square included, centers the
    // square there, and so does each frame of a drag off the square.
    let centered_at = if resp.clicked() {
        resp.interact_pointer_pos()
    } else if let Some(p) = photo.filter(|p| p.clicked() || p.dragged()) {
        p.interact_pointer_pos()
    } else {
        None
    };
    if let Some(p) = centered_at {
        let (u, v) = app.loupe_screen_to_tex(central, p);
        app.spike_center = spike_zoom_uv(w, h, egui::pos2(u, v), app.spike_side).center();
        ui.ctx().request_repaint();
    }
    if photo_dragged {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        if let Some(p) = resp.interact_pointer_pos() {
            let (u0, v0) = app.loupe_screen_to_tex(central, p - resp.drag_delta());
            let (u1, v1) = app.loupe_screen_to_tex(central, p);
            // Store the clamped center, so dragging past an edge and back
            // moves the square at once rather than after the overshoot.
            app.spike_center = spike_zoom_uv(
                w,
                h,
                uv.center() + egui::vec2(u1 - u0, v1 - v0),
                app.spike_side,
            )
            .center();
            ui.ctx().request_repaint();
        }
    } else if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    let rect = {
        let uv = spike_zoom_uv(w, h, app.spike_center, app.spike_side);
        egui::Rect::from_two_pos(
            app.loupe_tex_to_screen(central, uv.min.x, uv.min.y),
            app.loupe_tex_to_screen(central, uv.max.x, uv.max.y),
        )
    };
    let painter = ui.painter_at(central);
    // Dark under light, so the square reads on bright and dark photos.
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(3.0f32, egui::Color32::from_black_alpha(160)),
        egui::StrokeKind::Middle,
    );
    painter.rect_stroke(
        rect,
        0.0,
        egui::Stroke::new(1.5f32, egui::Color32::WHITE),
        egui::StrokeKind::Middle,
    );
}
