use super::*;

use crate::app::{App, Region, SHOW_GROUPING_TOOLS};
use crate::navigation::Cmp;

/// The Grid and Survey toolbar: rating filter, grouping toggles (while
/// `SHOW_GROUPING_TOOLS` is on), and the photo count. Actions on the selection
/// live in `selection_bar`.
pub(super) fn grid_toolbar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    egui::Panel::top("grid_toolbar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Keyboard-cursor index of each control, matching
            // `App::activate_toolbar_focus`.
            let mut idx = 0usize;

            ui.label("Rating:");
            let resp = ui.selectable_label(app.filter().is_none(), "All");
            if resp.clicked() {
                out.actions.push(UiAction::SetFilter(None));
            }
            toolbar_focus_sync(ui, app, idx, &resp, out);
            idx += 1;
            ui.separator();
            // The comparator applies to the next star click. It stays
            // highlighted even when no filter is set.
            let sel_cmp = app.filter_cmp();
            for (cmp, glyph, tip) in [
                (Cmp::Gte, "\u{2265}", "At least N stars"),
                (Cmp::Eq, "=", "Exactly N stars"),
                (Cmp::Lte, "\u{2264}", "At most N stars"),
            ] {
                let resp = ui
                    .selectable_label(sel_cmp == cmp, glyph)
                    .on_hover_text(tip);
                if resp.clicked() {
                    out.actions.push(UiAction::SetFilterCmp(cmp));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;
            }
            ui.separator();
            // `Eq` lights only star N; `Gte` and `Lte` light stars 1 through N.
            let (active_cmp, active_n) = match app.filter() {
                Some((c, v)) if (1..=5).contains(&v) => (Some(c), v),
                _ => (None, 0),
            };
            let cmp_sym = match sel_cmp {
                Cmp::Gte => "\u{2265}",
                Cmp::Eq => "=",
                Cmp::Lte => "\u{2264}",
            };
            for n in 1u8..=5 {
                let filled = match active_cmp {
                    Some(Cmp::Eq) => n == active_n,
                    Some(_) => n <= active_n,
                    None => false,
                };
                let glyph = if filled { "\u{2605}" } else { "\u{2606}" };
                let color = if filled {
                    theme::STAR_GOLD
                } else {
                    egui::Color32::from_gray(160)
                };
                let star = egui::Label::new(egui::RichText::new(glyph).size(20.0).color(color))
                    .sense(egui::Sense::click());
                let resp = ui
                    .add(star)
                    .on_hover_text(format!("Show photos rated {cmp_sym} {n}"));
                if resp.clicked() {
                    out.actions.push(UiAction::SetFilter(Some((sel_cmp, n))));
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;
            }
            ui.separator();
            let unrated = matches!(app.filter(), Some((Cmp::Eq, 0)));
            let resp = ui
                .selectable_label(unrated, "Unrated")
                .on_hover_text("Show only photos with no rating");
            if resp.clicked() {
                let next = if unrated { None } else { Some((Cmp::Eq, 0)) };
                out.actions.push(UiAction::SetFilter(next));
            }
            toolbar_focus_sync(ui, app, idx, &resp, out);
            idx += 1;

            // Hidden while `SHOW_GROUPING_TOOLS` is off; the `B` and `D` keys
            // still work. `App::TOOLBAR_CONTROLS` reads the same flag, so the
            // `?` button's index stays correct either way.
            if SHOW_GROUPING_TOOLS {
                // Bursts need the whole unfiltered folder.
                ui.separator();
                let filter_active = app.filter().is_some();
                let resp = ui
                    .add_enabled(
                        !filter_active,
                        egui::Button::selectable(app.bursts_on(), "Bursts"),
                    )
                    .on_hover_text(if filter_active {
                        "Clear the filter to use Bursts"
                    } else {
                        "Group bursts and badge the sharpest frame (B)"
                    });
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleBursts);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;

                let resp = ui
                    .selectable_label(app.dupes_on(), "Duplicates")
                    .on_hover_text("Group visually-similar frames and badge them (D)");
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleDupes);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
                idx += 1;

                // Blink data comes only from the face pass that Bursts or
                // Duplicates starts. Vision decodes at full resolution, which is
                // too heavy to run on a whole folder unasked.
                let grouped = app.bursts_on() || app.dupes_on();
                let resp = ui
                    .add_enabled(
                        grouped || app.eyes_filter_on(),
                        egui::Button::selectable(app.eyes_filter_on(), "Eyes closed"),
                    )
                    .on_hover_text(if grouped || app.eyes_filter_on() {
                        "Show only photos where someone blinked"
                    } else {
                        "Turn on Bursts or Duplicates to detect blinks"
                    });
                if resp.clicked() {
                    out.actions.push(UiAction::ToggleEyesClosed);
                }
                toolbar_focus_sync(ui, app, idx, &resp, out);
            }

            // Survey's own header already counts its photos.
            if app.mode() == ViewMode::Grid {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak(format!("{} photos", app.visible_len()));
                });
            }

            region_focus_marker(ui, app, Region::Toolbar);
        });
    });
}

/// Bulk actions on the Grid or Survey selection, in a row under the toolbar.
/// The row stays up with nothing selected so the grid doesn't shift when a
/// selection starts. Every action opens a confirm modal before it runs. These
/// are not in the keyboard cycle because they come and go with the selection;
/// each has its own shortcut instead.
pub(super) fn selection_bar(ui: &mut egui::Ui, app: &App, out: &mut FrameOutput) {
    let n = app.selection_count();
    egui::Panel::top("selection_bar").show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            // Buttons are taller than a label; keep the row one height.
            ui.set_min_height(ui.spacing().interact_size.y);
            if n == 0 {
                ui.weak("No selection \u{2014} click a photo, or Cmd+A to select all");
                return;
            }
            ui.strong(format!("{n} selected"));
            ui.separator();
            egui::ComboBox::from_id_salt("bulk_star")
                .selected_text("Rate \u{2605}")
                .show_ui(ui, |ui| {
                    for s in (1u8..=5).rev() {
                        if ui.button(star_string(s)).clicked() {
                            out.actions.push(UiAction::RequestBulk(BulkKind::Rate(s)));
                        }
                    }
                    if ui.button("Clear rating").clicked() {
                        out.actions.push(UiAction::RequestBulk(BulkKind::Rate(0)));
                    }
                });
            if ui
                .button("Auto Tone")
                .on_hover_text(
                    "Set each selected photo's tone sliders from its own histogram (Cmd+Shift+U)",
                )
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::AutoTone));
            }
            ui.separator();
            if ui
                .add_enabled(n == 1, egui::Button::new("Copy Settings"))
                .on_hover_text("Copy this photo's develop settings (Cmd+Shift+C)")
                .on_disabled_hover_text("Select a single photo to copy its settings")
                .clicked()
            {
                out.actions.push(UiAction::CopySettings);
            }
            let apply = ui
                .add_enabled(
                    app.has_copied_settings(),
                    egui::Button::new("Apply Settings"),
                )
                .on_disabled_hover_text("Copy settings from a photo first");
            if apply.clicked() {
                out.actions
                    .push(UiAction::RequestBulk(BulkKind::ApplySettings));
            }
            if let Some(name) = app.copied_settings_name() {
                ui.weak(format!("from {name}"));
            }
            ui.separator();
            if ui
                .button("Export JPG")
                .on_hover_text("Export each selected photo as a baked JPG")
                .clicked()
            {
                out.actions.push(UiAction::RequestBulk(BulkKind::Export));
            }

            // Destructive, so it sits apart from the others at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new("Delete").color(theme::DANGER_RED))
                    .on_hover_text(if cfg!(target_arch = "wasm32") {
                        "Permanently delete selected photos; cannot be undone (Delete)"
                    } else {
                        "Move selected photos to the Trash (Delete)"
                    })
                    .clicked()
                {
                    out.actions.push(UiAction::RequestBulk(BulkKind::Delete));
                }
            });
        });
    });
}
