use super::*;
use super::develop_panel::draw_develop_panel;
use super::grid::{draw_folders_panel, thumbnail_cell, STRIP_CELL_STYLE};

use crate::app::{App, CropEdge, FocusLevel, Region};
use crate::image_decode;


pub(super) fn draw_loupe(ui: &mut egui::Ui, app: &mut App, out: &mut FrameOutput) {
    let thumb_px = app.thumb_px();
    let sel = app.sel();

    // Left folder-tree sidebar (same as the grid) so structure stays visible.
    draw_folders_panel(ui, app, out);

    // The bottom filmstrip, unless hidden (Shift+Tab). When hidden, arrow keys
    // still step the photo — the strip is just the visual.
    if app.filmstrip_visible() {
        let strip_h = (thumb_px as f32 * 0.55).clamp(72.0, 200.0) + 8.0;
        egui::Panel::bottom("filmstrip")
            .exact_size(strip_h)
            .show_inside(ui, |ui| {
                let cell = strip_h - 16.0;
                let cell_full = cell + ui.spacing().item_spacing.x;
                let len = app.visible_len();

                // A horizontal-only ScrollArea doesn't respond to a plain
                // vertical mouse wheel in egui (only to a trackpad's
                // horizontal swipe, or an explicit shift+scroll) — so on a
                // vanilla mouse, wheeling over the filmstrip would otherwise
                // do nothing. Read the vertical delta here, before the
                // ScrollArea below (it never touches the y axis, so this
                // doesn't fight it), and step through photos instead —
                // that's the more useful reading of "scroll over the
                // filmstrip" anyway.
                if ui.rect_contains_pointer(ui.max_rect()) {
                    let dy = ui.input(|i| i.smooth_scroll_delta.y);
                    if dy != 0.0 {
                        out.actions.push(UiAction::ScrollFilmstrip(dy));
                    }
                }

                // Virtualized horizontal strip (egui has no `show_columns`, so do
                // the grid's `show_rows` trick by hand): build only the cells in
                // view and report the range so loading tracks the scroll position.
                let mut area = egui::ScrollArea::horizontal().auto_shrink([false, false]);
                // The strip only scrolls when the selection is about to run off
                // the currently-visible edge — stepping through its middle moves
                // just the highlight, not the strip itself. `filmstrip_last_sel`
                // (egui memory, not App state — purely a rendering concern) marks
                // when the selection actually changed; only then do we check
                // proximity to last frame's visible range (`strip_range`) and, if
                // warranted, arm a new `filmstrip_scroll_target` to glide toward
                // (clamped so it never scrolls past either end of the strip).
                // Once armed, the target keeps being animated toward — smoothly,
                // not teleported — every frame until it converges, at which point
                // we stop touching `scroll_offset` entirely so manual drags/clicks
                // on settled frames aren't fought.
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
                        // Leading + trailing spacers preserve the full content width
                        // so the scrollbar extent stays correct.
                        ui.add_space(first as f32 * cell_full);
                        for pos in first..last {
                            filmstrip_cell(ui, app, pos, cell, sel, out);
                        }
                        ui.add_space(len.saturating_sub(last) as f32 * cell_full);
                    });
                });

                // F6 can select the filmstrip as a region without entering an
                // individual thumbnail; show the same amber region marker used
                // by the other keyboard-focusable panels.
                region_focus_marker(ui, app, Region::Filmstrip);
            });
    }

    // Info bar: exposure/filename/date on the left+center, live star rating on
    // the right. A docked panel like the filmstrip (reserved space, not drawn
    // over the image), added right after the filmstrip so it sits directly
    // above it — or at the very bottom of the window when the filmstrip is
    // hidden, so it's always "below the image" either way.
    if app.metadata_panel_visible() {
        draw_loupe_info_bar(ui, app, out);
    }

    // Right-hand develop panel (Temp/Tint/Exposure/… sliders + histogram). Drawn
    // before the central rect is read so it reserves its width first — otherwise
    // the wgpu image viewport would overlap the panel.
    if app.develop_visible() {
        draw_develop_panel(ui, app, out);
    }

    // Central region: deliberately NOT a CentralPanel. Leaving it as the root
    // UI's unused rect is what makes egui report the pointer there as "not over
    // egui" (`is_pointer_over_egui` checks `!root_ui_available_rect.contains`),
    // so scroll (zoom), clicks, and Space+drag (pan) reach the app. A
    // CentralPanel would consume the rect and egui would claim all pointer input
    // over the image, killing zoom/pan. The wgpu image is drawn into this rect.
    let central = ui.available_rect_before_wrap();
    out.loupe_rect = Some(central);

    // Detail focus indicator: the central rect is deliberately left unclaimed
    // by egui (see the comment above) so `region_focus_marker`'s reliance on
    // `ui.min_rect()` doesn't apply here — draw directly against `central`.
    if app.focus() == Region::Detail && app.focus_level() == FocusLevel::Selected {
        ui.painter_at(central).rect_stroke(
            central.shrink(2.0),
            2.0,
            egui::Stroke::new(1.0f32, theme::CURSOR_AMBER),
            egui::StrokeKind::Outside,
        );
    }

    if app.crop_rect().is_some() {
        // Crop mode: the crop overlay owns the whole central area (mask + edges).
        loupe_crop_overlay(ui, app, central, out);
    } else if app.touchup_active() {
        loupe_touchup_overlay(ui, app, central, out);
    } else if app.wb_picker_active() {
        // WB picker: a click-catcher over the whole central area.
        loupe_wb_picker_overlay(ui, app, central, out);
    } else if app.compare() {
        // Before/after: a center divider and corner labels over the split image.
        loupe_compare_overlay(ui, central);
    }
}

pub(super) fn loupe_touchup_overlay(ui: &mut egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
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

/// The White Balance gray-picker overlay: a transparent click-catcher over the
/// whole central rect, shown only while `App::wb_picker_active()` is true. A
/// click samples that pixel (via `UiAction::PickWhiteBalance`) and the app
/// disarms picker mode in response, so this overlay stops being drawn.
pub(super) fn loupe_wb_picker_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    egui::Area::new(egui::Id::new("loupe_wb_picker"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::click());
            let painter = ui.painter_at(central);
            // A faint tint over the whole frame makes "picker mode is on" obvious.
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

/// The before/after overlay: a vertical divider down the middle of the central
/// rect and a "Before"/"After" label in each top corner. The two image halves
/// themselves are drawn by the wgpu renderer.
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
        let font = egui::FontId::proportional(13.0);
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
        "Before",
    );
    label(
        egui::pos2(central.max.x - pad, central.min.y + pad),
        egui::Align2::RIGHT_TOP,
        "After",
    );
}

/// The bar below the image (docked, reserved space — not an overlay): a
/// two-row info readout plus the live star-rating control, Lightroom
/// toolbar-style. Main row: exposure (left), filename + rating grouped and
/// centered. Secondary row: camera+lens and capture date, right-aligned
/// (less important than the filename/rating, so pushed out of the center).
/// Fields absent from the file's EXIF (screenshots, re-exports) are simply
/// omitted; the bar itself, filename, and rating control always render
/// regardless.
pub(super) fn draw_loupe_info_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let bar_h = 54.0;
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
            let main_font = egui::FontId::proportional(13.0);
            let sub_font = egui::FontId::proportional(11.0);
            let text_color = egui::Color32::from_gray(220);
            let dim_color = egui::Color32::from_gray(140);
            let main_y = rect.top() + bar_h * 0.36;
            let sub_y = rect.top() + bar_h * 0.72;
            let pad = 14.0;

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

            // Filename + star rating are grouped and centered together as a
            // single unit — measure the filename first so the stars can sit
            // immediately to its right while the pair as a whole stays centered.
            let star_w = 20.0;
            let stars_total_w = star_w * 5.0;
            let group_gap = 10.0;
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

            // Star rating: a plain child Ui pinned next to the filename. No
            // Area/Foreground trick needed here (unlike the old floating
            // overlay) — this bar is docked space, not drawn over the pannable
            // image, so egui already owns clicks within it.
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
                            egui::FontId::proportional(18.0),
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

            // Subject-selection controls, right-hand end of the main row (the
            // secondary row's camera/date text sits below, so nothing collides).
            // They live here rather than in the Develop panel on purpose: this
            // is a way of *looking* at the photo, not an edit to it.
            let sel_w = 190.0;
            let sel_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - pad - sel_w, main_y - 11.0),
                egui::vec2(sel_w, 22.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(sel_rect), |ui| {
                ui.horizontal_centered(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Invert is meaningless with the overlay off, and says
                        // so by being disabled rather than by vanishing.
                        let inverted = app.selection_inverted();
                        if ui
                            .add_enabled(
                                app.selection_on(),
                                egui::Button::selectable(inverted, "Invert"),
                            )
                            .on_hover_text("Highlight the background instead of the subject")
                            .clicked()
                        {
                            out.actions.push(UiAction::ToggleSelectionInvert);
                        }

                        // The label carries the state the user can't otherwise
                        // see: segmentation takes a moment, and "no subject
                        // found" is a real answer that would otherwise look
                        // identical to a broken button.
                        let label = if !app.selection_on() {
                            "Show Selection"
                        } else if app.selection_pending() {
                            "Selection…"
                        } else if app.current_selection().is_some() {
                            "Show Selection"
                        } else {
                            "No subject"
                        };
                        if ui
                            .add(egui::Button::selectable(app.selection_on(), label))
                            .on_hover_text("Outline the subject Vision finds in this photo")
                            .clicked()
                        {
                            out.actions.push(UiAction::ToggleSelection);
                        }
                    });
                });
            });
        });
}

/// The exposure string for the info bar's left section: `f/2.8  ISO 200
/// 1/125s  55mm` — aperture, ISO, shutter, focal length, in that order.
/// Fields absent from the file's EXIF are simply omitted.
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

/// The secondary line for the info bar: camera + lens, then capture date.
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
        parts.push(format_capture_date(
            d.year, d.month, d.day, d.hour, d.minute,
        ));
    }
    parts.join("   \u{b7}   ")
}

/// Format a shutter speed in seconds as EXIF conventionally displays it: a
/// fraction for sub-second exposures, whole/one-decimal seconds otherwise.
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

/// Format an EXIF capture date/time for display: `"Jul 14, 2026 3:42 PM"`.
pub(super) fn format_capture_date(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mon = MONTHS
        .get(month.wrapping_sub(1) as usize)
        .copied()
        .unwrap_or("");
    let (h12, ampm) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!("{mon} {day}, {year} {h12}:{minute:02} {ampm}")
}

/// The crop-mode overlay: a dimmed mask outside the crop rectangle, a bright
/// outline with edge handles, and drag handling that moves whichever edge the
/// user grabs. The rectangle is stored in the app in texture space; here we map
/// it to screen via `App::loupe_tex_to_screen` (which accounts for zoom, pan and
/// rotation), so a grabbed screen edge maps back to the correct texture edge.
pub(super) fn loupe_crop_overlay(ui: &egui::Ui, app: &App, central: egui::Rect, out: &mut FrameOutput) {
    let Some(rect) = app.crop_rect() else { return };

    // The four texture-space edges as screen segments (endpoint pairs).
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
    // Screen bounds of the crop (min/max copes with rotation flipping corners).
    let crop_screen = egui::Rect::from_points(&[tl, tr, bl, br]).intersect(central);

    egui::Area::new(egui::Id::new("loupe_crop"))
        .order(egui::Order::Foreground)
        .fixed_pos(central.min)
        .show(ui.ctx(), |ui| {
            let (_id, resp) = ui.allocate_exact_size(central.size(), egui::Sense::drag());
            let painter = ui.painter_at(central);

            // Dim the four bands around the crop rectangle.
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

            // Crop outline + rule-of-thirds guides.
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
            // Edge handles: a short bright bar at each edge midpoint.
            for (_, a, b) in edges {
                let mid = egui::pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                painter.circle_filled(mid, 5.0, line);
            }

            // Classify a pointer position: an edge (within grab threshold) takes
            // priority; otherwise inside the rectangle means "move the whole crop".
            const EDGE_GRAB_PX: f32 = 24.0;
            let inside_rect = |p: egui::Pos2| {
                let (u, v) = app.loupe_screen_to_tex(central, p);
                u >= rect.left && u <= rect.right && v >= rect.top && v <= rect.bottom
            };

            // Hover cursor hints: resize arrows on the edges, move icon inside.
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

            // Drag handling: on press, grab the nearest edge (resize) or, if the
            // press is inside the rectangle, grab the whole rect (move). While
            // dragging, feed the pointer's texture coordinate to the active grab.
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

/// The crop edge whose screen segment is nearest to `p`, if within `threshold`
/// px. Segments are `(edge, endpoint_a, endpoint_b)`.
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

/// Euclidean distance from point `p` to segment `a`–`b`.
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

/// One filmstrip cell. Returns the response so the caller can auto-scroll.
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
