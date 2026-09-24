use super::grid::{thumbnail_cell, STRIP_CELL_STYLE};
use super::*;

use crate::app::GRID_CELL_PT;
use crate::app::{App, CropEdge, FocusLevel, Region};
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
    let central = ui.available_rect_before_wrap();
    out.loupe_rect = Some(central);

    // `region_focus_marker` uses `ui.min_rect()`, which doesn't cover this
    // unclaimed rect, so draw against `central` directly.
    if app.focus() == Region::Detail && app.focus_level() == FocusLevel::Selected {
        ui.painter_at(central).rect_stroke(
            central.shrink(2.0),
            2.0,
            egui::Stroke::new(1.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Outside,
        );
    }

    if app.crop_rect().is_some() {
        loupe_crop_overlay(ui, app, central, out);
    } else if app.touchup_active() {
        loupe_touchup_overlay(ui, app, central, out);
    } else if app.wb_picker_active() {
        loupe_wb_picker_overlay(ui, app, central, out);
    } else if app.compare() {
        loupe_compare_overlay(ui, central);
    }

    if app.dragging {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
}

pub(super) fn loupe_touchup_overlay(
    ui: &mut egui::Ui,
    app: &App,
    central: egui::Rect,
    out: &mut FrameOutput,
) {
    let painter = ui.painter_at(central);
    for (i, t) in app.current_touchups().iter().enumerate() {
        let c = app.loupe_tex_to_screen(central, t.center[0], t.center[1]);
        let (radius_u, radius_v) = app.touchup_uv_radii(t.radius);
        let edge_u = app.loupe_tex_to_screen(central, t.center[0] + radius_u, t.center[1]);
        let edge_v = app.loupe_tex_to_screen(central, t.center[0], t.center[1] + radius_v);
        let radius = ((edge_u - c).length() + (edge_v - c).length()) * 0.5;
        let radius = radius.max(3.0);
        let selected = app.touchup_selected() == Some(i);
        painter.circle_stroke(
            c,
            radius,
            egui::Stroke::new(
                if selected { 2.5_f32 } else { 1.2_f32 },
                if selected {
                    theme::CURSOR_AMBER
                } else {
                    egui::Color32::from_white_alpha(190)
                },
            ),
        );
        painter.circle_filled(
            c,
            3.0,
            if selected {
                theme::CURSOR_AMBER
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
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    let (u, v) = app.loupe_screen_to_tex(central, p);
                    let mut hit = None;
                    for (i, t) in app.current_touchups().iter().enumerate() {
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

/// A transparent click-catcher over the image while the WB picker is armed.
/// The app disarms the picker after the click.
pub(super) fn loupe_wb_picker_overlay(
    ui: &egui::Ui,
    app: &App,
    central: egui::Rect,
    out: &mut FrameOutput,
) {
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
pub(super) fn loupe_compare_overlay(ui: &egui::Ui, central: egui::Rect) {
    let painter = ui.painter_at(central);
    let mid_x = central.center().x;
    painter.line_segment(
        [
            egui::pos2(mid_x, central.min.y),
            egui::pos2(mid_x, central.max.y),
        ],
        egui::Stroke::new(1.0f32, egui::Color32::from_gray(90)),
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

/// The info bar below the image. Top row: exposure, then filename and rating
/// centered, then the selection controls. Bottom row: camera, lens, and date.
/// EXIF fields the file lacks are omitted.
pub(super) fn draw_loupe_info_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let bar_h = font_size::px(ui.style(), 54.0);
    egui::Panel::bottom("loupe_info_bar")
        .exact_size(bar_h)
        .show_inside(ui, |ui| {
            let rect = ui.max_rect();
            let meta = app.current_metadata();

            let exposure = meta.map(exposure_text).unwrap_or_default();
            let secondary = meta.map(secondary_text).unwrap_or_default();
            let filename = app
                .selected_path()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();

            let painter = ui.painter();
            let main_font = egui::FontId::proportional(font_size::px(ui.style(), 13.0));
            let sub_font = egui::FontId::proportional(font_size::px(ui.style(), 11.0));
            let text_color = egui::Color32::from_gray(220);
            let dim_color = egui::Color32::from_gray(140);
            let main_y = rect.top() + bar_h * 0.36;
            let sub_y = rect.top() + bar_h * 0.72;
            let pad = font_size::px(ui.style(), 14.0);

            if !exposure.is_empty() {
                painter.text(
                    egui::pos2(rect.left() + pad, main_y),
                    egui::Align2::LEFT_CENTER,
                    &exposure,
                    main_font.clone(),
                    text_color,
                );
            }
            if !secondary.is_empty() {
                painter.text(
                    egui::pos2(rect.right() - pad, sub_y),
                    egui::Align2::RIGHT_CENTER,
                    &secondary,
                    sub_font,
                    dim_color,
                );
            }

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
            let group_w =
                filename_w + if filename.is_empty() { 0.0 } else { group_gap } + stars_total_w;
            let group_left = rect.center().x - group_w / 2.0;

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
            ui.scope_builder(egui::UiBuilder::new().max_rect(stars_rect), |ui| {
                ui.horizontal_centered(|ui| {
                    let current = app.selected_rating();
                    for i in 0..5u8 {
                        let (r, resp) = ui
                            .allocate_exact_size(egui::vec2(star_w, star_w), egui::Sense::click());
                        let filled = (i + 1) <= current;
                        let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                        let color = if filled {
                            theme::STAR_GOLD
                        } else {
                            egui::Color32::from_gray(160)
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
            });

            if let Some(label) = app.selected_label() {
                ui.painter().circle_filled(
                    egui::pos2(stars_rect.right() + group_gap, main_y),
                    font_size::px(ui.style(), 5.0),
                    label_color(label),
                );
            }

            // Subject selection is a way of viewing the photo, not an edit, so
            // it lives here rather than in the Develop panel.
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
        });
}

/// For example `f/2.8  ISO 200  1/125s  55mm`. Missing fields are omitted.
pub(super) fn exposure_text(meta: &image_decode::ImageMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(f) = meta.f_number {
        parts.push(format!("f/{f:.1}"));
    }
    if let Some(iso) = meta.iso {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(t) = meta.exposure_time {
        parts.push(format_shutter(t));
    }
    if let Some(fl) = meta.focal_length {
        parts.push(format!("{}mm", fl.round() as i64));
    }
    parts.join("  ")
}

pub(super) fn secondary_text(meta: &image_decode::ImageMetadata) -> String {
    let mut parts: Vec<String> = Vec::new();
    let camera = match (&meta.camera_make, &meta.camera_model) {
        (Some(make), Some(model)) if model.starts_with(make.as_str()) => Some(model.clone()),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (None, Some(model)) => Some(model.clone()),
        (Some(make), None) => Some(make.clone()),
        (None, None) => None,
    };
    match (camera, &meta.lens_model) {
        (Some(cam), Some(lens)) => parts.push(format!("{cam} \u{b7} {lens}")),
        (Some(cam), None) => parts.push(cam),
        (None, Some(lens)) => parts.push(lens.clone()),
        (None, None) => {}
    }
    if let Some(d) = meta.capture_date {
        parts.push((t().capture_date)(d.year, d.month, d.day, d.hour, d.minute));
    }
    parts.join("   \u{b7}   ")
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
    format!("https://maps.apple.com/?ll={:.6},{:.6}", gps.lat, gps.lon)
}

/// The crop overlay and its drag handling. The crop rect is in texture space.
/// `App::loupe_tex_to_screen` maps it to screen, accounting for zoom, pan, and
/// rotation.
pub(super) fn loupe_crop_overlay(
    ui: &egui::Ui,
    app: &App,
    central: egui::Rect,
    out: &mut FrameOutput,
) {
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
pub(super) fn nearest_edge(
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

pub(super) fn dist_to_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    let proj = a + ab * t;
    (p - proj).length()
}

pub(super) fn filmstrip_cell(
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
